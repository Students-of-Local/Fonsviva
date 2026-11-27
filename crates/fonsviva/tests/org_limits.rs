//! Slice 4: per-person limits on models, tokens and hours over real HTTP,
//! with the stub as the source and the server's clock set through
//! `FONSVIVA_TEST_NOW`.

mod org_support;

use org_support::stub::Stub;
use org_support::{Org, Resp, add_source, admin, member, start};
use serde_json::{Value, json};

const NEW_YORK: &str = "America/New_York";
/// Saturday 2026-03-07 10:00 in New York, the day before clocks go forward.
const SATURDAY_10: &str = "2026-03-07T15:00:00Z";
/// Monday 2026-03-09 09:00 in New York, after clocks went forward.
const MONDAY_9: &str = "2026-03-09T13:00:00Z";
/// Tuesday 2026-03-10 16:59 in New York.
const TUESDAY_1659: &str = "2026-03-10T20:59:00Z";
const TUESDAY_17: &str = "2026-03-10T21:00:00Z";

fn ready() -> (Org, Stub, String, String, String) {
    let org = start();
    let stub = Stub::start();
    stub.state().models = vec!["a".into(), "b".into()];
    let (a, _) = admin(&org);
    let r = add_source(&org, &a, "stub", &stub.base(), None);
    assert_eq!(r.status, 200, "{}", r.body);
    let (u, uid) = member(&org, &a, "user", "Bo");
    (org, stub, a, u, uid)
}

fn path(id: &str) -> String {
    format!("/api/org/v1/members/{id}/limits")
}

fn put(org: &Org, admin: &str, id: &str, body: Value) -> Resp {
    org.call("PUT", &path(id), Some(admin), body)
}

fn set(org: &Org, admin: &str, id: &str, body: Value) {
    let r = put(org, admin, id, body);
    assert_eq!(r.status, 200, "{}", r.body);
}

fn settings(org: &Org, admin: &str, patch: Value) -> Resp {
    org.call("PATCH", "/api/org/v1/settings", Some(admin), patch)
}

fn ask(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "hello"}]})
}

fn chat(org: &Org, cookie: &str, model: &str) -> Resp {
    org.call(
        "POST",
        "/api/org/v1/chat/completions",
        Some(cookie),
        ask(model),
    )
}

fn v1_chat(org: &Org, key: &str, model: &str) -> Resp {
    org.bearer(
        "POST",
        "/v1/chat/completions",
        Some(key),
        Some(ask(model).to_string()),
    )
}

fn send(org: &Org, cookie: &str, model: &str) -> Resp {
    org.call(
        "POST",
        "/api/org/v1/thread/messages",
        Some(cookie),
        json!({"text": "hello", "model": model}),
    )
}

fn models(org: &Org, cookie: &str) -> Vec<String> {
    let r = org.get("/api/org/v1/models", Some(cookie));
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

fn v1_models(org: &Org, key: &str) -> Vec<String> {
    let r = org.bearer("GET", "/v1/models", Some(key), None);
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

fn key_of(org: &Org, cookie: &str) -> String {
    let r = org.call(
        "POST",
        "/api/org/v1/keys",
        Some(cookie),
        json!({"name": "h"}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()["key"].as_str().unwrap().to_string()
}

fn refused(r: &Resp, status: u16, code: &str) {
    assert_eq!(
        (r.status, r.json()),
        (status, json!({"error": code})),
        "{}",
        r.body
    );
}

fn oai_refused(r: &Resp, status: u16, code: &str) {
    assert_eq!(r.status, status, "{}", r.body);
    assert_eq!(r.json()["error"]["code"], code, "{}", r.body);
    assert!(r.json()["error"]["message"].is_string(), "{}", r.body);
}

/// Rows in usage and messages.
fn stored(org: &Org) -> (i64, i64) {
    org.db()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM usage), (SELECT COUNT(*) FROM messages)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn retry_after(r: &Resp) -> i64 {
    r.header("retry-after")
        .expect("retry-after")
        .parse()
        .unwrap()
}

/// Mon to Fri 09:00 to 17:00.
fn weekdays() -> Value {
    json!({"hours": [{"days": [1, 2, 3, 4, 5], "start": "09:00", "end": "17:00"}]})
}

#[test]
fn case_1_a_model_list_limits_what_a_user_sees_and_chats_with() {
    let (org, _stub, a, u, uid) = ready();
    assert_eq!(models(&org, &u), ["stub/a", "stub/b"]);
    set(&org, &a, &uid, json!({"models": ["stub/a"]}));
    assert_eq!(models(&org, &u), ["stub/a"]);
    assert_eq!(models(&org, &a), ["stub/a", "stub/b"]);
    refused(&chat(&org, &u, "stub/b"), 403, "model_not_allowed");
    assert_eq!(chat(&org, &u, "stub/a").status, 200);
}

#[test]
fn case_2_a_personal_key_holds_to_its_members_models() {
    let (org, _stub, a, u, uid) = ready();
    let key = key_of(&org, &u);
    set(&org, &a, &uid, json!({"models": ["stub/a"]}));
    assert_eq!(v1_models(&org, &key), ["stub/a"]);
    oai_refused(&v1_chat(&org, &key, "stub/b"), 403, "model_not_allowed");
    assert_eq!(v1_chat(&org, &key, "stub/a").status, 200);
}

#[test]
fn case_3_a_thread_send_to_a_model_not_allowed_stores_nothing() {
    let (org, stub, a, u, uid) = ready();
    set(&org, &a, &uid, json!({"models": ["stub/a"]}));
    refused(&send(&org, &u, "stub/b"), 403, "model_not_allowed");
    let t = org.get("/api/org/v1/thread", Some(&u)).json();
    assert_eq!(t["messages"], json!([]));
    assert_eq!(stored(&org), (0, 0));
    assert!(stub.state().seen.is_empty());
}

#[test]
fn case_4_an_empty_model_list_lists_nothing_and_refuses_every_chat() {
    let (org, _stub, a, u, uid) = ready();
    set(&org, &a, &uid, json!({"models": []}));
    assert_eq!(models(&org, &u), Vec::<String>::new());
    for m in ["stub/a", "stub/b"] {
        refused(&chat(&org, &u, m), 403, "model_not_allowed");
    }
}

#[test]
fn case_5_a_members_own_daily_limit_holds_a_full_member() {
    let (org, stub, a, _u, _) = ready();
    {
        let mut s = stub.state();
        s.prompt_tokens = 50;
        s.completion_tokens = 30;
    }
    let (f, fid) = member(&org, &a, "full", "Cy");
    set(&org, &a, &fid, json!({"dailyTokens": 100}));
    assert_eq!(chat(&org, &f, "stub/a").status, 200);
    assert_eq!(chat(&org, &f, "stub/a").status, 200);
    refused(&chat(&org, &f, "stub/a"), 429, "daily_limit");
}

#[test]
fn case_6_a_members_own_limit_replaces_the_org_default() {
    let (org, stub, a, u, uid) = ready();
    {
        let mut s = stub.state();
        s.prompt_tokens = 50;
        s.completion_tokens = 30;
    }
    assert_eq!(settings(&org, &a, json!({"dailyTokens": 100})).status, 200);
    set(&org, &a, &uid, json!({"dailyTokens": 1000}));
    let (other, _) = member(&org, &a, "user", "Di");
    for _ in 0..3 {
        assert_eq!(chat(&org, &u, "stub/a").status, 200);
    }
    assert_eq!(chat(&org, &other, "stub/a").status, 200);
    assert_eq!(chat(&org, &other, "stub/a").status, 200);
    refused(&chat(&org, &other, "stub/a"), 429, "daily_limit");
    let r = org.get(&path(&uid), Some(&a)).json();
    assert_eq!(r["effective"]["dailyTokens"], 1000);
}

#[test]
fn case_7_outside_hours_is_refused_until_the_next_window_by_local_time() {
    let (mut org, _stub, a, u, uid) = ready();
    let key = key_of(&org, &u);
    let r = settings(&org, &a, json!({"timezone": NEW_YORK}));
    assert_eq!((r.status, &r.json()["timezone"]), (200, &json!(NEW_YORK)));
    set(&org, &a, &uid, weekdays());
    org.set_clock(Some(SATURDAY_10));
    let r = chat(&org, &u, "stub/a");
    refused(&r, 403, "outside_hours");
    assert_eq!(
        retry_after(&r),
        46 * 3600,
        "to Monday 09:00 across the clock change"
    );
    let r = v1_chat(&org, &key, "stub/a");
    oai_refused(&r, 403, "outside_hours");
    assert_eq!(retry_after(&r), 46 * 3600);
    let r = send(&org, &u, "stub/a");
    refused(&r, 403, "outside_hours");
    assert_eq!(retry_after(&r), 46 * 3600);
    assert_eq!(models(&org, &u), ["stub/a", "stub/b"]);
}

#[test]
fn case_8_inside_hours_is_allowed_up_to_the_windows_end() {
    let (mut org, _stub, a, u, uid) = ready();
    assert_eq!(
        settings(&org, &a, json!({"timezone": NEW_YORK})).status,
        200
    );
    set(&org, &a, &uid, weekdays());
    org.set_clock(Some(TUESDAY_1659));
    assert_eq!(chat(&org, &u, "stub/a").status, 200);
    org.set_clock(Some(TUESDAY_17));
    let r = chat(&org, &u, "stub/a");
    refused(&r, 403, "outside_hours");
    assert_eq!(retry_after(&r), 16 * 3600);
}

#[test]
fn case_9_a_window_the_clock_change_cuts_follows_wall_time() {
    let (mut org, _stub, a, u, uid) = ready();
    assert_eq!(
        settings(&org, &a, json!({"timezone": NEW_YORK})).status,
        200
    );
    set(
        &org,
        &a,
        &uid,
        json!({"hours": [{"days": [0], "start": "01:00", "end": "03:00"}]}),
    );
    org.set_clock(Some("2026-03-08T07:30:00Z"));
    let r = chat(&org, &u, "stub/a");
    refused(&r, 403, "outside_hours");
    assert_eq!(
        retry_after(&r),
        7 * 24 * 3600 - 9000,
        "03:30 EDT to next Sunday 01:00 EDT"
    );
    org.set_clock(Some("2026-03-08T06:59:00Z"));
    assert_eq!(chat(&org, &u, "stub/a").status, 200, "01:59 EST is inside");
}

#[test]
fn case_10_an_admin_with_limits_is_never_refused() {
    let (mut org, _stub, a, _u, _) = ready();
    let aid = org.get("/api/org/v1/me", Some(&a)).json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let limits = json!({
        "models": [],
        "dailyTokens": 1,
        "hours": [{"days": [3], "start": "00:00", "end": "00:01"}],
    });
    let r = put(&org, &a, &aid, limits.clone());
    assert_eq!(r.status, 200, "{}", r.body);
    let nulls = json!({"models": null, "dailyTokens": null, "hours": null});
    assert_eq!(r.json()["effective"], nulls);
    org.set_clock(Some(SATURDAY_10));
    for _ in 0..3 {
        assert_eq!(chat(&org, &a, "stub/b").status, 200);
    }
    assert_eq!(models(&org, &a), ["stub/a", "stub/b"]);
    let r = org.get(&path(&aid), Some(&a)).json();
    assert_eq!((&r["models"], &r["dailyTokens"]), (&json!([]), &json!(1)));
    assert_eq!(r["effective"], nulls);
    let me = org.get("/api/org/v1/me/limits", Some(&a)).json();
    assert_eq!(me["effective"], nulls);
    assert_eq!(me["now"]["allowed"], true);
}

#[test]
fn case_11_bad_shapes_are_400_and_change_nothing() {
    let (org, _stub, a, _u, uid) = ready();
    let good = json!({
        "models": ["stub/a"],
        "dailyTokens": 500,
        "hours": [{"days": [1], "start": "09:00", "end": "24:00"}],
    });
    set(&org, &a, &uid, good.clone());
    let before = org.get(&path(&uid), Some(&a)).json();
    let window = |d: Value, s: &str, e: &str| json!({"days": d, "start": s, "end": e});
    let many: Vec<String> = (0..201).map(|i| format!("stub/m{i}")).collect();
    let fifteen: Vec<Value> = (0..15)
        .map(|_| window(json!([1]), "09:00", "10:00"))
        .collect();
    for bad in [
        json!({"models": many}),
        json!({"hours": [window(json!([1]), "10:00", "10:00")]}),
        json!({"hours": [window(json!([1]), "11:00", "10:00")]}),
        json!({"hours": [window(json!([1]), "25:00", "26:00")]}),
        json!({"hours": [window(json!([1]), "09:00", "25:00")]}),
        json!({"hours": [window(json!([1]), "09:60", "10:00")]}),
        json!({"hours": [window(json!([7]), "09:00", "10:00")]}),
        json!({"hours": [window(json!([]), "09:00", "10:00")]}),
        json!({"hours": fifteen}),
        json!({"hours": []}),
        json!({"models": ["no-source"]}),
        json!({"models": "stub/a"}),
        json!({"dailyTokens": 0}),
        json!({"dailyTokens": 1_000_000_001u64}),
        json!({"limit": 3}),
        json!([]),
    ] {
        refused(&put(&org, &a, &uid, bad.clone()), 400, "bad_request");
        assert_eq!(org.get(&path(&uid), Some(&a)).json(), before, "{bad}");
    }
    refused(&put(&org, &a, "m_nobody", good), 404, "not_found");
    refused(&org.get(&path("m_nobody"), Some(&a)), 404, "not_found");
    for zone in [json!("Mars/Olympus"), json!(""), json!(5)] {
        refused(
            &settings(&org, &a, json!({"timezone": zone})),
            400,
            "bad_request",
        );
    }
    let s = org.get("/api/org/v1/settings", Some(&a)).json();
    assert_eq!(s["timezone"], "UTC");
}

#[test]
fn case_12_me_limits_says_whether_the_caller_may_chat_now() {
    let (mut org, _stub, a, u, uid) = ready();
    assert_eq!(
        settings(&org, &a, json!({"timezone": NEW_YORK})).status,
        200
    );
    set(&org, &a, &uid, weekdays());
    org.set_clock(Some(TUESDAY_1659));
    let me = org.get("/api/org/v1/me/limits", Some(&u)).json();
    assert_eq!(
        me["now"],
        json!({"allowed": true, "reason": null, "nextOpen": null})
    );
    assert_eq!(me["effective"]["hours"], weekdays()["hours"]);
    org.set_clock(Some(SATURDAY_10));
    let me = org.get("/api/org/v1/me/limits", Some(&u)).json();
    assert_eq!(
        me["now"],
        json!({"allowed": false, "reason": "outside_hours", "nextOpen": MONDAY_9})
    );
}

#[test]
fn case_13_all_nulls_remove_the_row_and_lift_the_limits() {
    let (org, _stub, a, u, uid) = ready();
    set(&org, &a, &uid, json!({"models": [], "dailyTokens": 5}));
    let rows = |org: &Org| -> i64 {
        org.db()
            .query_row("SELECT COUNT(*) FROM limits", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(rows(&org), 1);
    set(
        &org,
        &a,
        &uid,
        json!({"models": null, "dailyTokens": null, "hours": null}),
    );
    assert_eq!(rows(&org), 0);
    assert_eq!(models(&org, &u), ["stub/a", "stub/b"]);
    for _ in 0..2 {
        assert_eq!(chat(&org, &u, "stub/b").status, 200);
    }
}

#[test]
fn case_14_a_user_cannot_set_or_read_limits() {
    let (org, _stub, a, u, uid) = ready();
    refused(
        &put(&org, &u, &uid, json!({"models": []})),
        403,
        "forbidden",
    );
    refused(&org.get(&path(&uid), Some(&u)), 403, "forbidden");
    let r = org.get(&path(&uid), Some(&a)).json();
    assert_eq!(r["models"], Value::Null);
}

#[test]
fn case_15_refused_requests_write_no_usage_and_store_no_message() {
    let (mut org, stub, a, u, uid) = ready();
    let key = key_of(&org, &u);
    set(&org, &a, &uid, json!({"models": ["stub/a"]}));
    refused(&chat(&org, &u, "stub/b"), 403, "model_not_allowed");
    oai_refused(&v1_chat(&org, &key, "stub/b"), 403, "model_not_allowed");
    refused(&send(&org, &u, "stub/b"), 403, "model_not_allowed");
    assert_eq!(stored(&org), (0, 0));
    assert_eq!(
        settings(&org, &a, json!({"timezone": NEW_YORK})).status,
        200
    );
    let mut hours = weekdays();
    hours["models"] = json!(["stub/a"]);
    set(&org, &a, &uid, hours);
    org.set_clock(Some(SATURDAY_10));
    refused(&chat(&org, &u, "stub/a"), 403, "outside_hours");
    oai_refused(&v1_chat(&org, &key, "stub/a"), 403, "outside_hours");
    refused(&send(&org, &u, "stub/a"), 403, "outside_hours");
    refused(&send(&org, &u, "stub/b"), 403, "outside_hours");
    assert_eq!(stored(&org), (0, 0));
    assert!(stub.state().seen.is_empty());
}

#[test]
fn case_16_the_audit_names_the_fields_and_never_the_values() {
    let (org, _stub, a, _u, uid) = ready();
    let hidden = "stub/quietmodel-7e2b";
    set(
        &org,
        &a,
        &uid,
        json!({"models": [hidden], "dailyTokens": 4321}),
    );
    let audit = org.get("/api/org/v1/audit", Some(&a));
    let row = &audit.json()[0];
    assert_eq!(
        (&row["action"], &row["target"]),
        (
            &json!("limits.update"),
            &json!(format!("{uid} models,dailyTokens"))
        )
    );
    assert!(!audit.body.contains("quietmodel-7e2b") && !audit.body.contains("4321"));
    set(
        &org,
        &a,
        &uid,
        json!({"models": [hidden], "dailyTokens": 4321}),
    );
    set(&org, &a, &uid, json!({"models": [hidden]}));
    let audit = org.get("/api/org/v1/audit", Some(&a)).json();
    let updates: Vec<_> = audit
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["action"] == "limits.update")
        .map(|r| r["target"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        updates,
        [
            format!("{uid} dailyTokens"),
            format!("{uid} models,dailyTokens")
        ]
    );
    assert!(
        !org.get("/api/org/v1/audit", Some(&a))
            .body
            .contains("quietmodel")
    );
}

#[test]
fn case_17_the_previous_schema_gets_limits_and_keeps_its_rows() {
    let (org, _stub, a, u, uid) = ready();
    assert_eq!(chat(&org, &u, "stub/b").status, 200);
    let version = |org: &Org| -> i64 {
        org.db()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    };
    let current = version(&org);
    org.db()
        .execute_batch(&format!(
            "DROP TABLE limits; PRAGMA user_version = {};",
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
    set(&org, &a, &uid, json!({"models": ["stub/a"]}));
    refused(&chat(&org, &u, "stub/b"), 403, "model_not_allowed");
    assert_eq!(chat(&org, &u, "stub/a").status, 200);
}
