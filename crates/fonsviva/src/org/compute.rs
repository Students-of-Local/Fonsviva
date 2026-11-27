//! Compute requests: a batch of prompts to one model that a member asks to
//! run in a window, and an admin approves or denies (docs/ARCHITECTURE.md,
//! Compute requests).

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Extension, Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::collections::HashMap;

use fonsviva_core::util::{rfc3339, unix_now};

use super::http::{MODEL_LIMIT, Session, St, after_delete, body, err, internal, listed, may_chat};
use super::limits;
use super::store::{ComputeRow, DAY, NewCompute, Role};

const MAX_PROMPTS: usize = 50;
const PROMPT_LIMIT: usize = 32 << 10;
const PROMPTS_LIMIT: usize = 512 << 10;
const MAX_BUDGET: u64 = 1_000_000_000;
/// The longest `note` or deny `reason`, in characters.
const NOTE_LIMIT: usize = 500;
/// How far ahead a window may start.
const AHEAD: i64 = 30 * DAY as i64;
/// The largest filing: every prompt at its limit with every character
/// escaped as `\u00XX` (six bytes), and room for JSON.
pub const FILE_LIMIT: usize = 4 << 20;

const STATUSES: &[&str] = &[
    "pending",
    "approved",
    "denied",
    "running",
    "done",
    "stopped",
    "expired",
    "failed",
    "cancelled",
];

/// Seconds since the epoch of an RFC 3339 time with an offset, such as
/// `2026-11-27T22:00:00-05:00`; a fraction of a second is dropped.
fn parse_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let digits = |p: &str| -> Option<i64> {
        p.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| p.parse().ok())?
    };
    let part = |from: usize, to: usize| digits(s.get(from..to)?);
    let seps = [(4, b'-'), (7, b'-'), (13, b':'), (16, b':')];
    if b.len() < 20 || !matches!(b[10], b'T' | b't') || seps.iter().any(|(i, c)| b[*i] != *c) {
        return None;
    }
    let (y, mo, d) = (part(0, 4)?, part(5, 7)?, part(8, 10)?);
    let (h, mi, se) = (part(11, 13)?, part(14, 16)?, part(17, 19)?);
    let mut rest = s.get(19..)?;
    if let Some(f) = rest.strip_prefix('.') {
        let n = f.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        rest = &f[n..];
    }
    let off = match rest.as_bytes() {
        b"Z" | b"z" => 0,
        [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
            let (oh, om) = (digits(&rest[1..3])?, digits(&rest[4..6])?);
            if oh > 23 || om > 59 {
                return None;
            }
            let o = oh * 3600 + om * 60;
            if *sign == b'-' { -o } else { o }
        }
        _ => return None,
    };
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let month = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let day_ok = (1..=12).contains(&mo) && (1..=month[mo as usize - 1]).contains(&d);
    if !day_ok || h > 23 || mi > 59 || se > 59 {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    Some(days * DAY as i64 + h * 3600 + mi * 60 + se - off)
}

/// A window rule 1 allows: it ends after it starts, within a day of it and
/// in the future, and starts at most 30 days ahead.
fn window_ok(start: i64, end: i64, now: i64) -> bool {
    start < end && end - start <= DAY as i64 && end > now && start <= now + AHEAD
}

fn time(t: i64) -> String {
    rfc3339(t.max(0) as u64)
}

fn summary(c: &ComputeRow) -> Value {
    let (start, end, budget) = c.asked;
    json!({
        "id": c.id,
        "member": {"id": c.member, "name": c.name},
        "model": c.model,
        "prompts": c.prompts,
        "asked": {"start": time(start), "end": time(end), "maxTokens": budget},
        "approved": c.approved.as_ref().map(|a| json!({
            "start": time(a.start),
            "end": time(a.end),
            "maxTokens": a.budget,
            "by": a.by,
            "at": time(a.at),
        })),
        "status": c.status,
        "reason": c.reason,
        "note": c.note,
        "done": c.done,
        "usage": {
            "promptTokens": c.usage.prompt_tokens,
            "completionTokens": c.usage.completion_tokens,
            "costNanos": c.usage.cost_nanos.to_string(),
            "costKnown": c.usage.cost_known,
        },
        "createdAt": time(c.created),
        "startedAt": c.started.map(time),
        "endedAt": c.ended.map(time),
    })
}

#[derive(serde::Deserialize)]
struct FileReq {
    model: String,
    prompts: Vec<String>,
    start: String,
    end: String,
    #[serde(rename = "maxTokens")]
    budget: u64,
    note: Option<String>,
}

pub async fn file(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    b: Result<Bytes, BytesRejection>,
) -> Response {
    let bad = || err(StatusCode::BAD_REQUEST, "bad_request");
    let too_large = || err(StatusCode::PAYLOAD_TOO_LARGE, "too_large");
    let b = match b {
        Ok(b) => b,
        Err(r) if r.status() == StatusCode::PAYLOAD_TOO_LARGE => return too_large(),
        Err(_) => return bad(),
    };
    let Some(r): Option<FileReq> = body(&b) else {
        return bad();
    };
    if !(1..=MAX_PROMPTS).contains(&r.prompts.len())
        || r.prompts.iter().any(|p| p.trim().is_empty())
    {
        return bad();
    }
    let total: usize = r.prompts.iter().map(String::len).sum();
    if r.prompts.iter().any(|p| p.len() > PROMPT_LIMIT) || total > PROMPTS_LIMIT {
        return too_large();
    }
    let (Some(start), Some(end)) = (parse_time(&r.start), parse_time(&r.end)) else {
        return bad();
    };
    let note = r.note.as_deref().filter(|n| !n.trim().is_empty());
    if !window_ok(start, end, unix_now() as i64)
        || !(1..=MAX_BUDGET).contains(&r.budget)
        || note.is_some_and(|n| n.chars().count() > NOTE_LIMIT)
        || r.model.len() > MODEL_LIMIT
    {
        return bad();
    }
    let m = &s.member;
    match may_chat(&st, m) {
        Ok(true) => {}
        Ok(false) => return err(StatusCode::FORBIDDEN, "students_off"),
        Err(e) => return internal(e),
    }
    let lim = match limits::of(&st, m) {
        Ok((l, _)) => l,
        Err(e) => return internal(e),
    };
    if lim.models.as_ref().is_some_and(|ok| !ok.contains(&r.model)) {
        return err(StatusCode::FORBIDDEN, "model_not_allowed");
    }
    match listed(&st, m).await {
        Ok(list)
            if list
                .iter()
                .any(|(src, id)| format!("{src}/{id}") == r.model) => {}
        Ok(_) => return err(StatusCode::NOT_FOUND, "unknown_model"),
        Err(e) => return internal(e),
    }
    let filed = st.store().file_compute(&NewCompute {
        member: &m.id,
        model: &r.model,
        prompts: &r.prompts,
        start,
        end,
        budget: r.budget as i64,
        note,
    });
    match filed {
        Ok(Some(id)) => (
            StatusCode::CREATED,
            Json(json!({"id": id, "status": "pending"})),
        )
            .into_response(),
        Ok(None) => err(StatusCode::CONFLICT, "too_many_requests"),
        Err(e) => internal(e),
    }
}

fn list(rows: anyhow::Result<Vec<ComputeRow>>) -> Response {
    match rows {
        Ok(rows) => {
            Json(json!({"requests": rows.iter().map(summary).collect::<Vec<_>>()})).into_response()
        }
        Err(e) => internal(e),
    }
}

pub async fn mine(State(st): State<St>, Extension(s): Extension<Session>) -> Response {
    list(st.store().computes(Some(&s.member.id), None))
}

pub async fn all(State(st): State<St>, Query(q): Query<HashMap<String, String>>) -> Response {
    let status = q.get("status").map(String::as_str);
    if status.is_some_and(|s| !STATUSES.contains(&s)) {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    }
    list(st.store().computes(None, status))
}

/// The request when the caller owns it or is an admin; any other caller
/// gets the same 404 as for a request that doesn't exist. `Err` is a
/// status and an error code.
fn visible(st: &St, s: &Session, id: &str) -> Result<ComputeRow, (StatusCode, &'static str)> {
    let found = st.store().compute(id);
    match found {
        Ok(Some(c)) if c.member == s.member.id || s.member.role == Role::Admin => Ok(c),
        Ok(_) => Err((StatusCode::NOT_FOUND, "not_found")),
        Err(e) => {
            eprintln!("fonsviva org: {e:#}");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "internal"))
        }
    }
}

/// The request as it stands after a change.
fn reread(st: &St, id: &str) -> Response {
    match st.store().compute(id) {
        Ok(Some(c)) => Json(summary(&c)).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e),
    }
}

pub async fn one(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    match visible(&st, &s, &id) {
        Ok(c) => Json(summary(&c)).into_response(),
        Err((status, code)) => err(status, code),
    }
}

/// The owner reads their results; an admin reads another member's only
/// while the org lets admins read threads, and each such read is audited
/// under the same store guard.
pub async fn results(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let c = match visible(&st, &s, &id) {
        Ok(c) => c,
        Err((status, code)) => return err(status, code),
    };
    let store = st.store();
    let read = (|| {
        let other = c.member != s.member.id;
        if other && !store.settings()?.admins_read_threads {
            return Ok(err(StatusCode::FORBIDDEN, "threads_private"));
        }
        let results = store.compute_results(&id)?;
        if other {
            store.audit_compute_read(&s.member.id, &id)?;
        }
        Ok(Json(json!({"results": results})).into_response())
    })();
    read.unwrap_or_else(internal)
}

#[derive(serde::Deserialize, Default)]
struct ApproveReq {
    start: Option<String>,
    end: Option<String>,
    #[serde(rename = "maxTokens")]
    budget: Option<u64>,
}

/// An empty body reads as `{}`.
fn optional<T: serde::de::DeserializeOwned + Default>(b: &Bytes) -> Option<T> {
    if b.is_empty() {
        return Some(T::default());
    }
    body(b)
}

/// Approves with the asked window and budget, or the admin's: a window
/// rule 1 allows and a budget no larger than asked.
pub async fn approve(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    UrlPath(id): UrlPath<String>,
    b: Bytes,
) -> Response {
    let bad = || err(StatusCode::BAD_REQUEST, "bad_request");
    let Some(r): Option<ApproveReq> = optional(&b) else {
        return bad();
    };
    let c = match visible(&st, &s, &id) {
        Ok(c) => c,
        Err((status, code)) => return err(status, code),
    };
    if c.status != "pending" {
        return err(StatusCode::CONFLICT, "not_pending");
    }
    let (asked_start, asked_end, asked_budget) = c.asked;
    let at = |t: &Option<String>, asked: i64| match t {
        None => Some(asked),
        Some(t) => parse_time(t),
    };
    let (Some(start), Some(end)) = (at(&r.start, asked_start), at(&r.end, asked_end)) else {
        return bad();
    };
    let budget = r
        .budget
        .map_or(asked_budget, |n| n.min(i64::MAX as u64) as i64);
    let now = unix_now() as i64;
    if r.start.is_none() && r.end.is_none() && end <= now {
        return err(StatusCode::CONFLICT, "window_passed");
    }
    if !window_ok(start, end, now) || !(1..=asked_budget).contains(&budget) {
        return bad();
    }
    let approved = st
        .store()
        .approve_compute(&s.member.id, &id, (start, end, budget));
    match approved {
        Ok(true) => reread(&st, &id),
        Ok(false) => err(StatusCode::CONFLICT, "not_pending"),
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize, Default)]
struct DenyReq {
    reason: Option<String>,
}

pub async fn deny(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    UrlPath(id): UrlPath<String>,
    b: Bytes,
) -> Response {
    let r: Option<DenyReq> = optional(&b);
    let Some(r) = r.filter(|r| {
        r.reason
            .as_deref()
            .is_none_or(|t| t.chars().count() <= NOTE_LIMIT)
    }) else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    if let Err((status, code)) = visible(&st, &s, &id) {
        return err(status, code);
    }
    let reason = r.reason.as_deref().filter(|t| !t.trim().is_empty());
    let denied = st.store().deny_compute(&s.member.id, &id, reason);
    match denied {
        Ok(true) => reread(&st, &id),
        Ok(false) => err(StatusCode::CONFLICT, "not_pending"),
        Err(e) => internal(e),
    }
}

pub async fn cancel(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let c = match visible(&st, &s, &id) {
        Ok(c) => c,
        Err((status, code)) => return err(status, code),
    };
    let cancelled = st.store().cancel_compute(&s.member.id, &id, &c.member);
    match cancelled {
        Ok(true) => reread(&st, &id),
        Ok(false) => err(StatusCode::CONFLICT, "finished"),
        Err(e) => internal(e),
    }
}

/// Only the owner deletes, and only a finished request; its text is then
/// cleared from the log as a thread delete's is.
pub async fn remove(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let deleted = st.store().delete_compute(&s.member.id, &id);
    match deleted {
        Ok(Some(false)) => err(StatusCode::CONFLICT, "not_finished"),
        Ok(found) => after_delete(&st, &s.member.id, Ok(found.is_some())).await,
        Err(e) => after_delete(&st, &s.member.id, Err(e)).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_with_their_offset() {
        let t = parse_time("2026-11-27T22:00:00Z").unwrap();
        assert_eq!(time(t), "2026-11-27T22:00:00Z");
        assert_eq!(parse_time("2026-11-27T17:00:00-05:00"), Some(t));
        assert_eq!(parse_time("2026-11-28T03:30:00.250+05:30"), Some(t));
        let leap = parse_time("2024-02-29T00:00:00Z").unwrap();
        assert_eq!(time(leap), "2024-02-29T00:00:00Z");
        for bad in [
            "",
            "2026-11-27T22:00:00",
            "2026-11-27 22:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2026-11-27T24:00:00Z",
            "2026-11-27T22:00:00+5:00",
            "2026-11-27T22:00:00.Z",
            "+026-11-27T22:00:00Z",
            "2026-11-27T22:00:00+05:60",
            "2026-11-27T22:00:00+0é:00",
        ] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }
}
