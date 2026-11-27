//! Slice 6: compute requests over real HTTP, with the stub as the source:
//! filing, the lists, approval, denial, cancel, delete and results.

mod org_support;

use fonsviva_core::util::{rfc3339, unix_now};
use org_support::stub::Stub;
use org_support::{Org, Resp, add_source, admin, holds, member, start};
use serde_json::{Value, json};

const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;

fn ready() -> (Org, Stub, String, String, String) {
    let org = start();
    let stub = Stub::start();
    stub.state().models = vec!["a".into(), "b".into()];
    let (a, aid) = admin(&org);
    let r = add_source(&org, &a, "stub", &stub.base(), None);
    assert_eq!(r.status, 200, "{}", r.body);
    let (u, _) = member(&org, &a, "user", "Bo");
    (org, stub, a, aid, u)
}

fn at(t: i64) -> String {
    rfc3339(t as u64)
}

/// The next 22:00 UTC at least a minute away.
fn tonight() -> i64 {
    let now = unix_now() as i64 + 60;
    let t = now - now % DAY + 22 * HOUR;
    if t > now { t } else { t + DAY }
}

fn ask(prompts: &[&str], start: i64, end: i64, budget: u64) -> Value {
    json!({
        "model": "stub/a",
        "prompts": prompts,
        "start": at(start),
        "end": at(end),
        "maxTokens": budget,
    })
}

fn file(org: &Org, cookie: &str, body: Value) -> Resp {
    org.call("POST", "/api/org/v1/compute", Some(cookie), body)
}

/// Files a request for tonight and returns its id.
fn filed(org: &Org, cookie: &str, prompts: &[&str]) -> String {
    let t = tonight();
    let r = file(org, cookie, ask(prompts, t, t + 8 * HOUR, 5000));
    assert_eq!(r.status, 201, "{}", r.body);
    r.json()["id"].as_str().unwrap().to_string()
}

fn act(org: &Org, cookie: &str, id: &str, what: &str, body: Value) -> Resp {
    let path = format!("/api/org/v1/compute/{id}/{what}");
    org.call("POST", &path, Some(cookie), body)
}

fn one(org: &Org, cookie: &str, id: &str) -> Resp {
    org.get(&format!("/api/org/v1/compute/{id}"), Some(cookie))
}

fn remove(org: &Org, cookie: &str, id: &str) -> Resp {
    let path = format!("/api/org/v1/compute/{id}");
    let mut r = org_support::Req::new("DELETE", &path, org);
    r.headers.push(("X-Fonsviva-Request", "1".into()));
    r.headers.push(("Cookie", cookie.into()));
    org.send(r)
}

fn refused(r: &Resp, status: u16, code: &str) {
    assert_eq!(
        (r.status, r.json()),
        (status, json!({"error": code})),
        "{}",
        r.body
    );
}

fn settings(org: &Org, admin: &str, patch: Value) {
    let r = org.call("PATCH", "/api/org/v1/settings", Some(admin), patch);
    assert_eq!(r.status, 200, "{}", r.body);
}

/// The audit rows, newest first, as (action, target).
fn audit(org: &Org, admin: &str) -> Vec<(String, String)> {
    let r = org.get("/api/org/v1/audit", Some(admin));
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            let s = |k: &str| a[k].as_str().unwrap().to_string();
            (s("action"), s("target"))
        })
        .collect()
}

/// A result written straight into the store, as the runner will write one.
fn seed_result(org: &Org, id: &str, idx: i64, text: &str) {
    org.db()
        .execute(
            "INSERT INTO compute_results (compute_id, idx, text, status, prompt_tokens, completion_tokens, cost_nanos)
             VALUES (?1, ?2, ?3, 200, 11, 7, 1500)",
            rusqlite::params![id, idx, text],
        )
        .unwrap();
}

#[test]
fn case_1_a_filed_request_lists_for_its_member_and_admins_without_its_prompts() {
    let (org, _stub, a, _, u) = ready();
    let t = tonight();
    let local = |t: i64| at(t - 5 * HOUR).replace('Z', "-05:00");
    let prompts = ["zqprompt-one", "zqprompt-two", "zqprompt-three"];
    let mut body = ask(&prompts, t, t + 8 * HOUR, 5000);
    body["start"] = local(t).into();
    body["end"] = local(t + 8 * HOUR).into();
    body["note"] = "for the essay".into();
    let r = file(&org, &u, body);
    assert_eq!(r.status, 201, "{}", r.body);
    let id = r.json()["id"].as_str().unwrap().to_string();
    assert_eq!(r.json(), json!({"id": id, "status": "pending"}));
    let mine = org.get("/api/org/v1/me/compute", Some(&u));
    assert_eq!(mine.status, 200, "{}", mine.body);
    let got = &mine.json()["requests"][0];
    assert_eq!(got["id"], id);
    assert_eq!(got["member"]["name"], "Bo");
    assert_eq!(
        (&got["model"], &got["prompts"]),
        (&json!("stub/a"), &json!(3))
    );
    assert_eq!(
        got["asked"],
        json!({"start": at(t), "end": at(t + 8 * HOUR), "maxTokens": 5000})
    );
    assert_eq!(
        (&got["status"], &got["approved"]),
        (&json!("pending"), &Value::Null)
    );
    assert_eq!(
        (&got["note"], &got["done"]),
        (&json!("for the essay"), &json!(0))
    );
    assert_eq!(
        got["usage"],
        json!({"promptTokens": 0, "completionTokens": 0, "costNanos": "0", "costKnown": true})
    );
    let all = org.get("/api/org/v1/compute?status=pending", Some(&a));
    assert_eq!(all.status, 200, "{}", all.body);
    assert_eq!(all.json()["requests"][0]["id"], id);
    let none = org.get("/api/org/v1/compute?status=approved", Some(&a));
    assert_eq!(none.json()["requests"], json!([]));
    for body in [&mine.body, &all.body, &one(&org, &a, &id).body] {
        assert!(!body.contains("zqprompt"), "a prompt in {body}");
    }
    refused(
        &org.get("/api/org/v1/compute?status=late", Some(&a)),
        400,
        "bad_request",
    );
    refused(&org.get("/api/org/v1/compute", Some(&u)), 403, "forbidden");
}

#[test]
fn case_5_an_approval_keeps_the_admins_window_and_budget() {
    let (org, _stub, a, aid, u) = ready();
    let id = filed(&org, &u, &["one"]);
    let t = tonight();
    let (start, end) = (t + 2 * HOUR, t + 10 * HOUR);
    let r = act(
        &org,
        &a,
        &id,
        "approve",
        json!({"start": at(start), "end": at(end), "maxTokens": 1200}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let got = one(&org, &u, &id).json();
    assert_eq!(got["status"], "approved");
    assert_eq!(
        got["asked"],
        json!({"start": at(t), "end": at(t + 8 * HOUR), "maxTokens": 5000})
    );
    let approved = &got["approved"];
    assert_eq!(
        (&approved["start"], &approved["end"], &approved["maxTokens"]),
        (&json!(at(start)), &json!(at(end)), &json!(1200))
    );
    assert_eq!(approved["by"], aid);
    assert!(approved["at"].as_str().unwrap().ends_with('Z'));
    let plain = filed(&org, &u, &["two"]);
    assert_eq!(act(&org, &a, &plain, "approve", json!({})).status, 200);
    let got = one(&org, &u, &plain).json();
    assert_eq!(
        (&got["approved"]["start"], &got["approved"]["maxTokens"]),
        (&json!(at(t)), &json!(5000))
    );
}

#[test]
fn case_6_an_approval_above_the_budget_after_a_decision_or_the_window_is_refused() {
    let (org, _stub, a, _, u) = ready();
    let id = filed(&org, &u, &["one"]);
    let r = act(&org, &a, &id, "approve", json!({"maxTokens": 5001}));
    refused(&r, 400, "bad_request");
    let r = act(
        &org,
        &a,
        &id,
        "approve",
        json!({"end": at(tonight() + 25 * HOUR)}),
    );
    refused(&r, 400, "bad_request");
    assert_eq!(one(&org, &u, &id).json()["status"], "pending");
    let denied = filed(&org, &u, &["two"]);
    assert_eq!(act(&org, &a, &denied, "deny", json!({})).status, 200);
    refused(
        &act(&org, &a, &denied, "approve", json!({})),
        409,
        "not_pending",
    );
    refused(
        &act(&org, &a, &denied, "deny", json!({})),
        409,
        "not_pending",
    );
    let late = filed(&org, &u, &["three"]);
    let now = unix_now() as i64;
    org.db()
        .execute(
            "UPDATE compute SET start_asked = ?1, end_asked = ?2 WHERE id = ?3",
            rusqlite::params![now - 2 * HOUR, now - HOUR, late],
        )
        .unwrap();
    refused(
        &act(&org, &a, &late, "approve", json!({})),
        409,
        "window_passed",
    );
    assert_eq!(one(&org, &u, &late).json()["status"], "pending");
}

#[test]
fn case_15_bad_filings_are_refused_and_store_nothing() {
    let (org, _stub, _a, _, u) = ready();
    let t = tonight();
    let now = unix_now() as i64;
    let many: Vec<String> = (0..51).map(|i| format!("p{i}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let big = "x".repeat(33 << 10);
    for (body, status) in [
        (ask(&many, t, t + HOUR, 100), 400),
        (ask(&[&big], t, t + HOUR, 100), 413),
        (ask(&["p"], t, t + 25 * HOUR, 100), 400),
        (ask(&["p"], now - 2 * HOUR, now - HOUR, 100), 400),
        (ask(&["p"], now + 31 * DAY, now + 31 * DAY + HOUR, 100), 400),
        (ask(&[" \n"], t, t + HOUR, 100), 400),
        (ask(&[""], t, t + HOUR, 100), 400),
        (ask(&["p"], t, t + HOUR, 0), 400),
    ] {
        let r = file(&org, &u, body);
        assert_eq!(r.status, status, "{}", r.body);
    }
    let stored = |org: &Org| -> i64 {
        org.db()
            .query_row("SELECT COUNT(*) FROM compute", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(stored(&org), 0);
    for _ in 0..10 {
        filed(&org, &u, &["p"]);
    }
    refused(
        &file(&org, &u, ask(&["p"], t, t + HOUR, 100)),
        409,
        "too_many_requests",
    );
    assert_eq!(stored(&org), 10);
}

#[test]
fn case_16_an_admin_reads_results_only_while_the_org_allows_it() {
    let (org, _stub, a, aid, u) = ready();
    let id = filed(&org, &u, &["zqfirst", "zqsecond"]);
    seed_result(&org, &id, 0, "the reply");
    refused(
        &org.get(&format!("/api/org/v1/compute/{id}/results"), Some(&a)),
        403,
        "threads_private",
    );
    assert!(!audit(&org, &a).iter().any(|(act, _)| act == "compute.read"));
    let own = org.get(&format!("/api/org/v1/compute/{id}/results"), Some(&u));
    assert_eq!(own.status, 200, "{}", own.body);
    assert_eq!(
        own.json(),
        json!({"results": [{
            "index": 0, "prompt": "zqfirst", "text": "the reply", "status": 200,
            "promptTokens": 11, "completionTokens": 7, "costNanos": "1500",
        }]})
    );
    assert!(!audit(&org, &a).iter().any(|(act, _)| act == "compute.read"));
    settings(&org, &a, json!({"adminsReadThreads": true}));
    let read = org.get(&format!("/api/org/v1/compute/{id}/results"), Some(&a));
    assert_eq!((read.status, &read.json()), (200, &own.json()));
    assert_eq!(audit(&org, &a)[0], ("compute.read".into(), id.clone()));
    let row: (String, String) = org
        .db()
        .query_row(
            "SELECT admin, target FROM audit WHERE action = 'compute.read'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(row, (aid, id));
}

#[test]
fn case_17_another_member_can_not_see_or_touch_a_request() {
    let (org, _stub, a, _, u) = ready();
    let (other, _) = member(&org, &a, "full", "Cy");
    let id = filed(&org, &u, &["one"]);
    let paths = [
        format!("/api/org/v1/compute/{id}"),
        format!("/api/org/v1/compute/{id}/results"),
    ];
    for p in &paths {
        refused(&org.get(p, Some(&other)), 404, "not_found");
    }
    refused(
        &act(&org, &other, &id, "cancel", json!({})),
        404,
        "not_found",
    );
    refused(&remove(&org, &other, &id), 404, "not_found");
    refused(
        &act(&org, &other, &id, "approve", json!({})),
        403,
        "forbidden",
    );
    assert_eq!(
        org.get("/api/org/v1/me/compute", Some(&other)).json()["requests"],
        json!([])
    );
    assert_eq!(one(&org, &u, &id).json()["status"], "pending");
    assert_eq!(act(&org, &a, &id, "cancel", json!({})).status, 200);
    refused(&remove(&org, &a, &id), 404, "not_found");
    assert_eq!(audit(&org, &a)[0], ("compute.cancel".into(), id));
}

#[test]
fn case_19_a_deleted_request_leaves_its_text_nowhere() {
    let (org, _stub, _a, _, u) = ready();
    let id = filed(&org, &u, &["zqpromptmark", "second"]);
    seed_result(&org, &id, 0, "zqreplymark");
    refused(&remove(&org, &u, &id), 409, "not_finished");
    assert_eq!(
        act(&org, &u, &id, "cancel", json!({})).json()["status"],
        "cancelled"
    );
    refused(&act(&org, &u, &id, "cancel", json!({})), 409, "finished");
    assert!(holds(&org.db_bytes(), "zqpromptmark"));
    let r = remove(&org, &u, &id);
    assert_eq!(r.status, 204, "{}", r.body);
    let bytes = org.db_bytes();
    for marker in ["zqpromptmark", "zqreplymark"] {
        assert!(
            !holds(&bytes, marker),
            "{marker} is still in org.db or org.db-wal"
        );
    }
    refused(&one(&org, &u, &id), 404, "not_found");
    refused(&remove(&org, &u, &id), 404, "not_found");
}

#[test]
fn case_21_decisions_are_audited_with_the_id_alone() {
    let (org, _stub, a, aid, u) = ready();
    let t = tonight();
    let mut body = ask(&["zqauditprompt"], t, t + HOUR, 100);
    body["note"] = "zqauditnote".into();
    let yes = file(&org, &u, body.clone()).json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let no = file(&org, &u, body).json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(act(&org, &a, &yes, "approve", json!({})).status, 200);
    let r = act(&org, &a, &no, "deny", json!({"reason": "zqauditreason"}));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        (&r.json()["status"], &r.json()["reason"]),
        (&json!("denied"), &json!("zqauditreason"))
    );
    let rows = audit(&org, &a);
    assert_eq!(
        rows[..2],
        [("compute.deny".into(), no), ("compute.approve".into(), yes)]
    );
    let all = org.get("/api/org/v1/audit", Some(&a)).body;
    assert!(!all.contains("zqaudit"), "{all}");
    assert!(all.contains(&aid));
}

#[test]
fn case_22_the_previous_schema_gets_compute_requests_and_keeps_its_rows() {
    let (org, _stub, a, _, u) = ready();
    let chat = org.call(
        "POST",
        "/api/org/v1/chat/completions",
        Some(&u),
        json!({"model": "stub/a", "messages": [{"role": "user", "content": "hi"}]}),
    );
    assert_eq!(chat.status, 200, "{}", chat.body);
    let version = |org: &Org| -> i64 {
        org.db()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    };
    let current = version(&org);
    org.db()
        .execute_batch(&format!(
            "DROP INDEX usage_compute; ALTER TABLE usage DROP COLUMN compute_id; DROP TABLE compute_results; DROP TABLE compute_prompts; DROP TABLE compute; PRAGMA user_version = {};",
            current - 1
        ))
        .unwrap();
    let rows = |org: &Org| -> (i64, i64, i64) {
        org.db()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM members), (SELECT COUNT(*) FROM usage), (SELECT COUNT(*) FROM audit)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    };
    let before = rows(&org);
    let out = org_support::fonsviva(org.scratch.path())
        .args(["org", "admin-link", "--data"])
        .arg(&org.data)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(version(&org), current);
    assert_eq!(rows(&org), (before.0, before.1, before.2 + 1));
    let kept: Option<String> = org
        .db()
        .query_row("SELECT compute_id FROM usage", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kept, None);
    let id = filed(&org, &u, &["one"]);
    assert_eq!(act(&org, &a, &id, "approve", json!({})).status, 200);
    assert_eq!(
        org.get("/api/org/v1/me/compute", Some(&u)).json()["requests"][0]["status"],
        "approved"
    );
}

fn stored(org: &Org) -> i64 {
    org.db()
        .query_row("SELECT COUNT(*) FROM compute", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn filing_holds_to_the_models_a_member_may_use_and_the_total_size() {
    let (org, _stub, a, _, u) = ready();
    let t = tonight();
    let (s, _) = member(&org, &a, "student", "Di");
    settings(&org, &a, json!({"studentsAllowed": false}));
    refused(
        &file(&org, &s, ask(&["p"], t, t + HOUR, 100)),
        403,
        "students_off",
    );
    let (l, lid) = member(&org, &a, "user", "Ed");
    let r = org.call(
        "PUT",
        &format!("/api/org/v1/members/{lid}/limits"),
        Some(&a),
        json!({"models": ["stub/b"]}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    refused(
        &file(&org, &l, ask(&["p"], t, t + HOUR, 100)),
        403,
        "model_not_allowed",
    );
    let mut gone = ask(&["p"], t, t + HOUR, 100);
    gone["model"] = json!("stub/zz");
    refused(&file(&org, &u, gone), 404, "unknown_model");
    let part = "x".repeat(31 << 10);
    let parts: Vec<&str> = (0..17).map(|_| part.as_str()).collect();
    refused(
        &file(&org, &u, ask(&parts, t, t + HOUR, 100)),
        413,
        "too_large",
    );
    assert_eq!(stored(&org), 0);
}

#[test]
fn a_member_keeps_at_most_fifty_requests_until_they_delete_some() {
    let (org, _stub, _a, _, u) = ready();
    for _ in 0..50 {
        let id = filed(&org, &u, &["p"]);
        assert_eq!(act(&org, &u, &id, "cancel", json!({})).status, 200);
    }
    let t = tonight();
    refused(
        &file(&org, &u, ask(&["p"], t, t + HOUR, 100)),
        409,
        "too_many_requests",
    );
    let last = org.get("/api/org/v1/me/compute", Some(&u)).json()["requests"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(remove(&org, &u, &last).status, 204);
    filed(&org, &u, &["p"]);
    assert_eq!(stored(&org), 50);
}

#[test]
fn an_approval_that_moves_only_the_start_of_a_passed_window_is_window_passed() {
    let (org, _stub, a, _, u) = ready();
    let future = filed(&org, &u, &["p"]);
    let now = unix_now() as i64;
    refused(
        &act(&org, &a, &future, "approve", json!({"end": at(now - HOUR)})),
        400,
        "bad_request",
    );
    let id = filed(&org, &u, &["p"]);
    org.db()
        .execute(
            "UPDATE compute SET start_asked = ?1, end_asked = ?2 WHERE id = ?3",
            rusqlite::params![now - 2 * HOUR, now - HOUR, id],
        )
        .unwrap();
    refused(
        &act(
            &org,
            &a,
            &id,
            "approve",
            json!({"start": at(now - 90 * 60)}),
        ),
        409,
        "window_passed",
    );
}

#[test]
fn an_admin_read_that_shows_no_answers_is_not_audited() {
    let (org, _stub, a, _, u) = ready();
    let id = filed(&org, &u, &["p"]);
    settings(&org, &a, json!({"adminsReadThreads": true}));
    let r = org.get(&format!("/api/org/v1/compute/{id}/results"), Some(&a));
    assert_eq!((r.status, r.json()), (200, json!({"results": []})));
    assert!(!audit(&org, &a).iter().any(|(act, _)| act == "compute.read"));
}

#[test]
fn a_summary_sums_only_its_own_usage_rows() {
    let (org, _stub, a, _, u) = ready();
    let one_id = filed(&org, &u, &["p"]);
    let two_id = filed(&org, &u, &["p"]);
    let member: String = org
        .db()
        .query_row(
            "SELECT member_id FROM compute WHERE id = ?1",
            [&one_id],
            |r| r.get(0),
        )
        .unwrap();
    for (id, p, c, nanos) in [
        (Some(&one_id), 10, 5, 3_000_000_001i64),
        (Some(&one_id), 20, 7, 999_999_999),
        (Some(&two_id), 1, 1, 1),
        (None, 100, 100, 100),
    ] {
        org.db()
            .execute(
                "INSERT INTO usage (at, member_id, model, status, prompt_tokens, completion_tokens, ms, cost_nanos, cost_known, compute_id)
                 VALUES (?1, ?2, 'stub/a', 200, ?3, ?4, 1, ?5, 1, ?6)",
                rusqlite::params![unix_now() as i64, member, p, c, nanos, id],
            )
            .unwrap();
    }
    let list = org.get("/api/org/v1/compute", Some(&a)).json();
    let usage = |id: &str| {
        list["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .unwrap()["usage"]
            .clone()
    };
    assert_eq!(
        usage(&one_id),
        json!({"promptTokens": 30, "completionTokens": 12, "costNanos": "4000000000", "costKnown": true})
    );
    assert_eq!(
        usage(&two_id),
        json!({"promptTokens": 1, "completionTokens": 1, "costNanos": "1", "costKnown": true})
    );
    assert_eq!(one(&org, &u, &one_id).json()["usage"], usage(&one_id));
}
