//! Slice 3, second half: sending a message in a member's thread over real
//! HTTP, with the stub as the source.

mod org_support;

use org_support::stub::Stub;
use org_support::{Org, Req, Resp, add_source, admin, holds, member, start};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const MODEL: &str = "stub/tiny-1";

fn ready() -> (Org, Stub, String) {
    let org = start();
    let stub = Stub::start();
    let (a, _) = admin(&org);
    let r = add_source(&org, &a, "stub", &stub.base(), None);
    assert_eq!(r.status, 200, "{}", r.body);
    (org, stub, a)
}

fn settings(org: &Org, admin: &str, patch: Value) {
    let r = org.call("PATCH", "/api/org/v1/settings", Some(admin), patch);
    assert_eq!(r.status, 200, "{}", r.body);
}

fn send(org: &Org, cookie: &str, body: Value) -> Resp {
    org.call("POST", "/api/org/v1/thread/messages", Some(cookie), body)
}

fn say(org: &Org, cookie: &str, text: &str) -> Vec<Value> {
    let r = send(org, cookie, json!({"text": text, "model": MODEL}));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.header("content-type"), Some("text/event-stream"));
    events(&r.body)
}

fn events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

fn thread(org: &Org, cookie: &str) -> Value {
    let r = org.get("/api/org/v1/thread", Some(cookie));
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()
}

/// (role, text, status) of the member's messages, newest first.
fn messages(org: &Org, cookie: &str) -> Vec<(String, String, Option<i64>)> {
    thread(org, cookie)["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let s = |k: &str| m[k].as_str().unwrap().to_string();
            (s("role"), s("text"), m["status"].as_i64())
        })
        .collect()
}

fn seed(org: &Org, member: &str, role: &str, text: &str) {
    let db = org.db();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db.execute(
        "INSERT INTO messages (member_id, role, text, model, at, status)
         VALUES (?1, ?2, ?3, 'stub/tiny-1', 0, NULL)",
        rusqlite::params![member, role, text],
    )
    .unwrap();
}

/// The `messages` the stub got in its `n`th chat, as (role, content).
fn upstream(stub: &Stub, n: usize) -> Vec<(String, String)> {
    stub.state().seen[n].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let s = |k: &str| m[k].as_str().unwrap().to_string();
            (s("role"), s("content"))
        })
        .collect()
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(r, t)| (r.to_string(), t.to_string()))
        .collect()
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn case_1_a_send_streams_its_reply_and_both_join_the_thread() {
    let (org, _stub, a) = ready();
    let ev = say(&org, &a, "hello");
    assert_eq!(ev[0]["type"], "start");
    let seq = ev[0]["seq"].as_i64().unwrap();
    let deltas: Vec<_> = ev.iter().filter(|e| e["type"] == "delta").collect();
    assert_eq!(deltas.len(), 2, "{ev:?}");
    let text: String = deltas.iter().map(|e| e["text"].as_str().unwrap()).collect();
    assert_eq!(text, "one two");
    let done = ev.last().unwrap();
    assert_eq!(
        (&done["type"], &done["status"]),
        (&json!("done"), &json!(200))
    );
    assert!(done["seq"].as_i64().unwrap() > seq);
    assert_eq!(
        messages(&org, &a),
        [
            ("assistant".into(), "one two".into(), Some(200)),
            ("user".into(), "hello".into(), None),
        ]
    );
}

#[test]
fn case_2_a_send_carries_the_thread_and_asks_for_usage() {
    let (org, stub, a) = ready();
    say(&org, &a, "first");
    let r = send(
        &org,
        &a,
        json!({"text": "second", "model": MODEL, "history": [{"role": "user", "text": "ignored"}]}),
    );
    assert_eq!(r.status, 200);
    let body = stub.state().seen[1].body.clone();
    let mut keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["messages", "model", "stream", "stream_options"]);
    assert_eq!(
        (&body["model"], &body["stream"]),
        (&json!("tiny-1"), &json!(true))
    );
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    let db = org.db();
    let mut q = db
        .prepare("SELECT prompt_tokens, completion_tokens, estimated FROM usage")
        .unwrap();
    let rows: Vec<(i64, i64, bool)> = q
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows, [(11, 7, false), (11, 7, false)]);
    assert_eq!(
        upstream(&stub, 1),
        pairs(&[
            ("user", "first"),
            ("assistant", "one two"),
            ("user", "second")
        ])
    );
}

#[test]
fn case_3_the_context_keeps_the_newest_100_and_200_kib() {
    let (org, stub, a) = ready();
    let (_, id) = whoami(&org, &a);
    for i in 0..150 {
        seed(&org, &id, ["user", "assistant"][i % 2], &format!("m{i}"));
    }
    say(&org, &a, "new");
    let got = upstream(&stub, 0);
    assert_eq!(got.len(), 101);
    assert_eq!(got[0].1, "m50");
    assert_eq!(got[99].1, "m149");
    assert_eq!(got[100].1, "new");

    let (org, stub, a) = ready();
    let (_, id) = whoami(&org, &a);
    for i in 0..10 {
        seed(
            &org,
            &id,
            ["user", "assistant"][i % 2],
            &format!("{i}{}", "x".repeat(30 << 10)),
        );
    }
    say(&org, &a, "new");
    let got = upstream(&stub, 0);
    let earlier = &got[..got.len() - 1];
    let bytes: usize = earlier.iter().map(|(_, t)| t.len()).sum();
    assert!(bytes <= 200 << 10, "{bytes}");
    assert_eq!(earlier.len(), 6);
    assert!(earlier[0].1.starts_with('4') && earlier[5].1.starts_with('9'));
}

fn whoami(org: &Org, cookie: &str) -> (String, String) {
    let me = org.get("/api/org/v1/me", Some(cookie)).json();
    (cookie.to_string(), me["id"].as_str().unwrap().to_string())
}

#[test]
fn case_5_a_client_that_leaves_keeps_the_reply_so_far_as_499() {
    let (org, stub, a) = ready();
    stub.state().drip = true;
    let mut r = Req::new("POST", "/api/org/v1/thread/messages", &org);
    r.headers.push(("X-Fonsviva-Request", "1".into()));
    r.headers.push(("Cookie", a.clone()));
    r.body = Some(json!({"text": "hello", "model": MODEL}).to_string());
    let mut s = org.open(r);
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while !holds(&got, "drip") {
        let n = std::io::Read::read(&mut s, &mut buf).unwrap();
        assert!(n > 0, "the stream ended before its reply");
        got.extend_from_slice(&buf[..n]);
    }
    drop(s);
    let left = Instant::now();
    until("the stub to see the close", || {
        stub.state().closed.is_some()
    });
    let closed = stub.state().closed.unwrap();
    assert!(closed.duration_since(left) < Duration::from_secs(2));
    until("the reply", || messages(&org, &a).len() == 2);
    let (role, text, status) = messages(&org, &a).remove(0);
    assert_eq!((role.as_str(), status), ("assistant", Some(499)));
    assert!(text.starts_with("drip"), "{text}");
}

#[test]
fn case_6_a_source_error_ends_with_its_status_and_keeps_the_message() {
    let (org, stub, a) = ready();
    stub.state().fail = Some((500, "{}".into()));
    let ev = say(&org, &a, "hello");
    assert_eq!(ev.len(), 2, "{ev:?}");
    assert_eq!(
        (&ev[1]["type"], &ev[1]["status"]),
        (&json!("done"), &json!(500))
    );
    assert_eq!(ev[1]["seq"], Value::Null);
    assert_eq!(ev[1]["error"], "upstream");
    assert_eq!(messages(&org, &a), [("user".into(), "hello".into(), None)]);
    let status: i64 = org
        .db()
        .query_row("SELECT status FROM usage", [], |r| r.get(0))
        .unwrap();
    assert_eq!(status, 500);
}

#[test]
fn case_7_a_second_send_while_one_runs_is_busy() {
    let (org, stub, a) = ready();
    stub.state().hold = true;
    std::thread::scope(|sc| {
        let first = sc.spawn(|| say(&org, &a, "one"));
        until("the first send upstream", || !stub.state().seen.is_empty());
        let r = send(&org, &a, json!({"text": "two", "model": MODEL}));
        assert_eq!((r.status, r.json()), (409, json!({"error": "busy"})));
        stub.state().released = true;
        assert_eq!(first.join().unwrap().last().unwrap()["status"], 200);
    });
    assert_eq!(stub.state().seen.len(), 1);
    assert_eq!(say(&org, &a, "three").last().unwrap()["status"], 200);
}

#[test]
fn case_8_a_send_is_refused_for_students_off_and_unknown_models() {
    let (org, stub, a) = ready();
    settings(&org, &a, json!({"studentsAllowed": false}));
    let (s, _) = member(&org, &a, "student", "Sy");
    let r = send(&org, &s, json!({"text": "hi", "model": MODEL}));
    assert_eq!(
        (r.status, r.json()),
        (403, json!({"error": "students_off"}))
    );
    let r = send(&org, &a, json!({"text": "hi", "model": "stub/none"}));
    assert_eq!(
        (r.status, r.json()),
        (404, json!({"error": "unknown_model"}))
    );
    assert!(messages(&org, &s).is_empty() && messages(&org, &a).is_empty());
    assert!(stub.state().seen.is_empty());
}

#[test]
fn case_9_a_member_at_the_daily_limit_appends_nothing() {
    let (org, _stub, a) = ready();
    settings(&org, &a, json!({"dailyTokens": 10}));
    let (u, _) = member(&org, &a, "user", "Bo");
    say(&org, &u, "first");
    let r = send(&org, &u, json!({"text": "second", "model": MODEL}));
    assert_eq!((r.status, r.json()), (429, json!({"error": "daily_limit"})));
    assert!(r.header("retry-after").is_some());
    assert_eq!(messages(&org, &u).len(), 2);
}

#[test]
fn case_15_with_threads_not_kept_a_send_uses_history_and_stores_nothing() {
    let (org, stub, a) = ready();
    settings(&org, &a, json!({"keepThreads": false}));
    let history =
        json!([{"role": "user", "text": "earlier"}, {"role": "assistant", "text": "reply"}]);
    let r = send(
        &org,
        &a,
        json!({"text": "now", "model": MODEL, "history": history}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let ev = events(&r.body);
    assert_eq!(
        (&ev[0]["seq"], &ev.last().unwrap()["seq"]),
        (&Value::Null, &Value::Null)
    );
    assert_eq!(ev.last().unwrap()["status"], 200);
    assert_eq!(
        upstream(&stub, 0),
        pairs(&[("user", "earlier"), ("assistant", "reply"), ("user", "now")])
    );
    let n: i64 = org
        .db()
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(thread(&org, &a)["stored"], false);
    let bad = json!({"text": "now", "model": MODEL, "history": [{"role": "system", "text": "x"}]});
    assert_eq!(send(&org, &a, bad).status, 400);
}

#[test]
fn case_19_a_sent_marker_is_never_in_usage_audit_or_the_log() {
    let (mut org, stub, a) = ready();
    let marker = "zq-marker-19-vellum";
    stub.state().reply = Some(format!("{marker} "));
    say(&org, &a, marker);
    stub.state().fail = Some((500, format!("{{\"error\":\"{marker}\"}}")));
    say(&org, &a, marker);
    let big = format!("{marker}{}", "x".repeat(300 << 10));
    assert_eq!(
        send(&org, &a, json!({"text": big, "model": MODEL})).status,
        413
    );
    let db = org.db();
    for table in ["usage", "audit"] {
        let mut q = db.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let cols = q.column_count();
        let rows: Vec<String> = q
            .query_map([], |r| {
                Ok((0..cols)
                    .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(!rows.is_empty() || table == "audit");
        assert!(
            rows.iter().all(|r| !r.contains(marker)),
            "{table}: {rows:?}"
        );
    }
    assert!(messages(&org, &a).iter().any(|m| m.1.contains(marker)));
    let log = org.stop();
    assert!(!log.contains(marker), "{log}");
}

#[test]
fn a_source_down_while_its_models_are_listed_is_502_and_stores_nothing() {
    let (org, stub, a) = ready();
    stub.state().models_fail = 1;
    let r = send(&org, &a, json!({"text": "hello", "model": MODEL}));
    assert_eq!((r.status, r.json()), (502, json!({"error": "upstream"})));
    assert!(messages(&org, &a).is_empty());
}

#[test]
fn a_refused_source_key_ends_with_its_code() {
    let (org, stub, a) = ready();
    stub.state().fail = Some((401, "{}".into()));
    let ev = say(&org, &a, "hello");
    let done = ev.last().unwrap();
    assert_eq!(
        (&done["status"], &done["error"]),
        (&json!(502), &json!("upstream_auth"))
    );
    assert_eq!(messages(&org, &a), [("user".into(), "hello".into(), None)]);
}

#[test]
fn a_last_line_without_a_newline_is_kept_and_an_overlong_one_counted() {
    let (org, stub, a) = ready();
    stub.state().tail =
        Some(r#"data: {"choices":[{"index":0,"delta":{"content":" end"}}]}"#.into());
    say(&org, &a, "hello");
    assert_eq!(messages(&org, &a)[0].1, "one two end");

    let (mut org, stub, a) = ready();
    stub.state().reply = Some("x".repeat(1_200_000));
    say(&org, &a, "hello");
    assert_eq!(messages(&org, &a)[0].1, "two");
    let log = org.stop();
    assert!(log.contains("not relayed (12"), "{log}");
}

#[test]
fn threads_turned_off_during_a_send_keep_its_reply_out() {
    let (org, stub, a) = ready();
    stub.state().hold = true;
    std::thread::scope(|sc| {
        let first = sc.spawn(|| say(&org, &a, "hello"));
        until("the send upstream", || !stub.state().seen.is_empty());
        settings(&org, &a, json!({"keepThreads": false}));
        stub.state().released = true;
        let ev = first.join().unwrap();
        assert_eq!(ev.last().unwrap()["seq"], Value::Null);
    });
    let n: i64 = org
        .db()
        .query_row(
            "SELECT count(*) FROM messages WHERE role = 'assistant'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn a_sent_mark_never_reaches_the_source() {
    let (org, stub, a) = ready();
    say(&org, &a, &format!("a{}", "\u{E000}\u{E001}".repeat(1000)));
    assert_eq!(upstream(&stub, 0), pairs(&[("user", "a")]));
}

#[test]
fn a_user_message_that_fails_to_store_is_a_plain_500() {
    let (org, stub, a) = ready();
    org.db()
        .execute_batch(
            "CREATE TRIGGER no BEFORE INSERT ON messages BEGIN SELECT RAISE(ABORT, 'no'); END;",
        )
        .unwrap();
    let r = send(&org, &a, json!({"text": "hello", "model": MODEL}));
    assert_eq!((r.status, r.json()), (500, json!({"error": "internal"})));
    assert!(stub.state().seen.is_empty());
}

#[test]
fn a_cut_stream_keeps_its_reply_as_502_upstream() {
    let (org, stub, a) = ready();
    stub.state().cut = true;
    let done = say(&org, &a, "hello").pop().unwrap();
    assert_eq!(
        (&done["status"], &done["error"]),
        (&json!(502), &json!("upstream"))
    );
    assert_eq!(
        messages(&org, &a)[0],
        ("assistant".into(), "one ".into(), Some(502))
    );
}

#[test]
fn a_thread_deleted_during_a_send_keeps_no_reply() {
    let (org, stub, a) = ready();
    stub.state().hold = true;
    std::thread::scope(|sc| {
        let first = sc.spawn(|| say(&org, &a, "hello"));
        until("the send upstream", || !stub.state().seen.is_empty());
        let r = org.call("DELETE", "/api/org/v1/thread", Some(&a), json!({}));
        assert_eq!(r.status, 204, "{}", r.body);
        stub.state().released = true;
        assert_eq!(first.join().unwrap().last().unwrap()["seq"], Value::Null);
    });
    assert!(messages(&org, &a).is_empty());
}

#[test]
fn a_send_over_1_mib_is_413() {
    let (org, stub, a) = ready();
    let r = send(
        &org,
        &a,
        json!({"text": "hi", "model": MODEL, "pad": "x".repeat(2 << 20)}),
    );
    assert_eq!(r.status, 413);
    assert!(stub.state().seen.is_empty());
}

#[test]
fn a_send_after_a_failed_one_keeps_roles_alternating() {
    let (org, stub, a) = ready();
    say(&org, &a, "first");
    stub.state().fail = Some((500, "{}".into()));
    say(&org, &a, "lost");
    stub.state().fail = None;
    say(&org, &a, "again");
    assert_eq!(
        upstream(&stub, 2),
        pairs(&[
            ("user", "first"),
            ("assistant", "one two"),
            ("user", "again")
        ])
    );
    assert_eq!(messages(&org, &a).len(), 5);
}
