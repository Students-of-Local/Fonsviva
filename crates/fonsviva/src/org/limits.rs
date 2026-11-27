//! Per-person limits: which models a member may use, how many tokens a
//! day, and the hours of the week they may chat in the org's time zone
//! (docs/ARCHITECTURE.md, Limits).

use anyhow::{Context, Result};
use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Extension, Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ZonedDateTime};
use serde_json::{Value, json};

use super::chat::split_model;
use super::http::{Caller, MODEL_LIMIT, OrgState, Session, St, err, internal};
use super::store::{DAY, Member, Role, Settings};
use fonsviva_core::util::rfc3339;

const MAX_MODELS: usize = 200;
const MAX_WINDOWS: usize = 14;
const MAX_DAILY: u64 = 1_000_000_000;
/// Days ahead the next open window is looked for.
const LOOK_AHEAD: usize = 15;

/// Days of the week, 0 = Sunday, and minutes of the day from `start` up to
/// but not including `end`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    days: Vec<u8>,
    start: u16,
    end: u16,
}

/// A member's limits as stored; `None` leaves each one unset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Limits {
    pub models: Option<Vec<String>>,
    pub daily_tokens: Option<u64>,
    pub hours: Option<Vec<Window>>,
}

fn hhmm(m: u16) -> String {
    format!("{:02}:{:02}", m / 60, m % 60)
}

/// Minutes of the day in `HH:MM`; only an `end` may be `24:00`.
fn minutes(v: &Value, end: bool) -> Option<u16> {
    let s = v.as_str()?;
    let (h, m) = s.split_once(':')?;
    if h.len() != 2 || m.len() != 2 || !s.bytes().all(|b| b.is_ascii_digit() || b == b':') {
        return None;
    }
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    let ok = (h <= 23 && m <= 59) || (end && h == 24 && m == 0);
    ok.then_some(h * 60 + m)
}

impl Window {
    fn parse(v: &Value) -> Option<Window> {
        let o = v.as_object()?;
        if o.len() != 3 {
            return None;
        }
        let mut days = vec![];
        for d in o.get("days")?.as_array()? {
            let d = d.as_u64().filter(|d| *d <= 6)? as u8;
            if days.contains(&d) {
                return None;
            }
            days.push(d);
        }
        let start = minutes(o.get("start")?, false)?;
        let end = minutes(o.get("end")?, true)?;
        (!days.is_empty() && start < end).then_some(Window { days, start, end })
    }

    fn json(&self) -> Value {
        json!({"days": self.days, "start": hhmm(self.start), "end": hhmm(self.end)})
    }
}

impl Limits {
    /// A `PUT` body: an object of `models`, `dailyTokens` and `hours`, each
    /// null or absent when unset. `None` when any part is a bad shape.
    pub fn parse(v: &Value) -> Option<Limits> {
        let o = v.as_object()?;
        if o.keys()
            .any(|k| !["models", "dailyTokens", "hours"].contains(&k.as_str()))
        {
            return None;
        }
        let set = |k: &str| o.get(k).filter(|v| !v.is_null());
        let models = match set("models") {
            None => None,
            Some(v) => {
                let list = v.as_array().filter(|l| l.len() <= MAX_MODELS)?;
                let ids = list.iter().map(|m| {
                    m.as_str()
                        .filter(|m| m.len() <= MODEL_LIMIT && split_model(m).is_some())
                        .map(str::to_string)
                });
                Some(ids.collect::<Option<Vec<_>>>()?)
            }
        };
        let daily = match set("dailyTokens") {
            None => None,
            Some(v) => Some(v.as_u64().filter(|n| (1..=MAX_DAILY).contains(n))?),
        };
        let hours = match set("hours") {
            None => None,
            Some(v) => {
                let list = v
                    .as_array()
                    .filter(|l| (1..=MAX_WINDOWS).contains(&l.len()))?;
                Some(list.iter().map(Window::parse).collect::<Option<Vec<_>>>()?)
            }
        };
        Some(Limits {
            models,
            daily_tokens: daily,
            hours,
        })
    }

    pub fn json(&self) -> Value {
        json!({
            "models": self.models,
            "dailyTokens": self.daily_tokens,
            "hours": self.hours.as_ref().map(|h| h.iter().map(Window::json).collect::<Vec<_>>()),
        })
    }

    pub fn is_empty(&self) -> bool {
        *self == Limits::default()
    }

    /// The names of the fields that differ from `was`.
    pub fn changed(&self, was: &Limits) -> Vec<&'static str> {
        let mut out = vec![];
        if self.models != was.models {
            out.push("models");
        }
        if self.daily_tokens != was.daily_tokens {
            out.push("dailyTokens");
        }
        if self.hours != was.hours {
            out.push("hours");
        }
        out
    }

    /// The `models`, `daily_tokens` and `hours` columns of a `limits` row.
    pub fn columns(&self) -> (Option<String>, Option<i64>, Option<String>) {
        let j = self.json();
        let text = |k: &str| (!j[k].is_null()).then(|| j[k].to_string());
        (
            text("models"),
            self.daily_tokens.map(|n| n as i64),
            text("hours"),
        )
    }

    pub fn from_columns(
        models: Option<String>,
        daily: Option<i64>,
        hours: Option<String>,
    ) -> Result<Limits> {
        let parsed = Limits::parse(&json!({
            "models": models.as_deref().map(serde_json::from_str::<Value>).transpose()?,
            "dailyTokens": daily,
            "hours": hours.as_deref().map(serde_json::from_str::<Value>).transpose()?,
        }));
        parsed.context("stored limits")
    }
}

/// The limits that hold for a member: none for an admin; for anyone else
/// their own, with the org's daily limit for users and students who have
/// none of their own.
pub fn effective(role: Role, own: &Limits, s: &Settings) -> Limits {
    if role == Role::Admin {
        return Limits::default();
    }
    let default = matches!(role, Role::User | Role::Student)
        .then_some(s.daily_tokens)
        .flatten();
    let daily = own.daily_tokens.or(default);
    Limits {
        models: own.models.clone(),
        daily_tokens: daily,
        hours: own.hours.clone(),
    }
}

/// The member's limits as they hold now, and the org's settings.
pub fn of(st: &OrgState, m: &Member) -> Result<(Limits, Settings)> {
    let store = st.store();
    let s = store.settings()?;
    let own = store.limits(&m.id)?.map(|(_, l)| l).unwrap_or_default();
    Ok((effective(m.role, &own, &s), s))
}

/// The time now. Debug builds read `FONSVIVA_TEST_NOW` (RFC 3339) instead
/// when it is set, so tests choose the clock without waiting.
pub fn now() -> Timestamp {
    #[cfg(debug_assertions)]
    if let Some(t) = std::env::var("FONSVIVA_TEST_NOW")
        .ok()
        .and_then(|t| t.parse().ok())
    {
        return t;
    }
    Timestamp::now()
}

/// The org's time zone from the bundled tz database, UTC if it is unknown.
fn zone(s: &Settings) -> TimeZone {
    TimeZone::get(&s.timezone).unwrap_or(TimeZone::UTC)
}

/// The IANA name of a zone the bundled tz database knows.
pub fn zone_name(name: &str) -> Option<String> {
    let tz = TimeZone::get(name).ok()?;
    tz.iana_name().map(str::to_string)
}

fn inside(hours: &[Window], z: &ZonedDateTime) -> bool {
    let day = z.weekday().to_sunday_zero_offset() as u8;
    let min = z.hour() as u16 * 60 + z.minute() as u16;
    hours
        .iter()
        .any(|w| w.days.contains(&day) && w.start <= min && min < w.end)
}

/// `None` while the member's hours let them chat at `now`. Otherwise the
/// seconds until the next window opens by the wall clock, or `None` when
/// none opens in the next two weeks. A start a clock change skips opens
/// when the clock reads a time inside a window again, and a start the
/// clock reads twice opens at whichever comes next.
pub fn shut(l: &Limits, s: &Settings, now: Timestamp) -> Option<Option<i64>> {
    let hours = l.hours.as_deref()?;
    let tz = zone(s);
    let z = now.to_zoned_datetime(tz.clone());
    if inside(hours, &z) {
        return None;
    }
    let mut date = z.date();
    for _ in 0..LOOK_AHEAD {
        let day = date.weekday().to_sunday_zero_offset() as u8;
        let next = hours
            .iter()
            .filter(|w| w.days.contains(&day))
            .flat_map(|w| {
                let at = date.at((w.start / 60) as i8, (w.start % 60) as i8, 0, 0);
                let both = tz.to_ambiguous_zoned_datetime(at);
                [both.clone().earlier().ok(), both.later().ok()]
            })
            .flatten()
            .filter(|at| at.timestamp() > now && inside(hours, at))
            .map(|at| at.timestamp())
            .min();
        if let Some(t) = next {
            return Some(Some(t.as_second() - now.as_second()));
        }
        date = match date.tomorrow() {
            Ok(d) => d,
            Err(_) => break,
        };
    }
    Some(None)
}

fn member_reply(own: &Limits, role: Role, s: &Settings) -> Value {
    let mut v = own.json();
    v["effective"] = effective(role, own, s).json();
    v
}

pub async fn member_limits(State(st): State<St>, UrlPath(id): UrlPath<String>) -> Response {
    let store = st.store();
    match (store.limits(&id), store.settings()) {
        (Ok(Some((role, own))), Ok(s)) => Json(member_reply(&own, role, &s)).into_response(),
        (Ok(None), _) => err(StatusCode::NOT_FOUND, "not_found"),
        (Err(e), _) | (_, Err(e)) => internal(e),
    }
}

pub async fn put_limits(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    UrlPath(id): UrlPath<String>,
    b: Result<Bytes, BytesRejection>,
) -> Response {
    let b = match b {
        Ok(b) => b,
        Err(r) if r.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return err(StatusCode::PAYLOAD_TOO_LARGE, "too_large");
        }
        Err(_) => return err(StatusCode::BAD_REQUEST, "bad_request"),
    };
    let Some(l) = serde_json::from_slice::<Value>(&b)
        .ok()
        .as_ref()
        .and_then(Limits::parse)
    else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    let mut store = st.store();
    let set = store.set_limits(&s.member.id, &id, &l);
    match (set, store.settings()) {
        (Ok(Some(role)), Ok(settings)) => Json(member_reply(&l, role, &settings)).into_response(),
        (Ok(None), _) => err(StatusCode::NOT_FOUND, "not_found"),
        (Err(e), _) | (_, Err(e)) => internal(e),
    }
}

/// The caller's limits and whether they may chat right now.
pub async fn my_limits(State(st): State<St>, Extension(c): Extension<Caller>) -> Response {
    let (l, s) = match of(&st, &c.member) {
        Ok(f) => f,
        Err(e) => return internal(e),
    };
    let now = now();
    let off = c.member.role == Role::Student && !s.students_allowed;
    let spent = match l.daily_tokens {
        Some(limit) => {
            let real = fonsviva_core::util::unix_now() as i64;
            let today = real - real.rem_euclid(DAY as i64);
            match st.store().tokens_since(&c.member.id, today) {
                Ok(used) => (used >= limit as i64).then(|| rfc3339((today + DAY as i64) as u64)),
                Err(e) => return internal(e),
            }
        }
        None => None,
    };
    let (reason, next) = if off {
        (Some("students_off"), None)
    } else if let Some(wait) = shut(&l, &s, now) {
        (
            Some("outside_hours"),
            wait.map(|w| rfc3339((now.as_second() + w).max(0) as u64)),
        )
    } else if l.models.as_ref().is_some_and(|m| m.is_empty()) {
        (Some("model_not_allowed"), None)
    } else if let Some(next) = spent {
        (Some("daily_limit"), Some(next))
    } else {
        (None, None)
    };
    Json(json!({
        "effective": l.json(),
        "now": {"allowed": reason.is_none(), "reason": reason, "nextOpen": next},
    }))
    .into_response()
}
