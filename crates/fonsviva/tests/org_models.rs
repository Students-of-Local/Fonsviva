//! The chat proxy and its usage rows, against the real `fonsviva org
//! serve` and a stub source on 127.0.0.1.

mod org_support;

use org_support::{Org, Req, SOURCE_KEY as KEY, holds, member, ready};
use serde_json::{Value, json};
use std::io::Read;
use std::time::{Duration, Instant};

/// A usage row: member, key id, model, status, prompt and completion tokens.
type Row = (
    String,
    Option<String>,
    String,
    i64,
    Option<i64>,
    Option<i64>,
);

fn chat(org: &Org, cookie: &str, body: Value) -> org_support::Resp {
    org.call("POST", "/api/org/v1/chat/completions", Some(cookie), body)
}

fn usage_rows(org: &Org) -> Vec<Row> {
    let db = org.db();
    let mut q = db
        .prepare("SELECT member_id, key_id, model, status, prompt_tokens, completion_tokens FROM usage ORDER BY seq")
        .unwrap();
    q.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// Waits up to 2 s for `n` usage rows: a stream's row is written as it ends.
fn wait_rows(org: &Org, n: usize) -> Vec<Row> {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let rows = usage_rows(org);
        if rows.len() >= n || Instant::now() > until {
            return rows;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn case_8_a_chat_reaches_the_source_unchanged_but_for_the_model() {
    let (org, stub, _, u) = ready();
    let sent = json!({
        "model": "stub/org/big-2",
        "messages": [{"role": "user", "content": "hello"}],
        "temperature": 0.25,
        "max_tokens": 9,
        "tools": [{"type": "function", "function": {"name": "f", "parameters": {}}}],
        "x_extra": {"nested": [1, 2, 3]},
    });
    let r = chat(&org, &u, sent.clone());
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["choices"][0]["message"]["content"], "stub reply");
    assert!(
        r.header("content-type")
            .unwrap()
            .starts_with("application/json")
    );
    let st = stub.state();
    let got = &st.seen[0];
    let mut want = sent;
    want["model"] = json!("org/big-2");
    assert_eq!(got.body, want);
    assert_eq!(got.headers["authorization"], format!("Bearer {KEY}"));
    drop(st);
    let rows = wait_rows(&org, 1);
    assert_eq!(rows[0].2, "stub/org/big-2");
    assert_eq!((rows[0].3, rows[0].4, rows[0].5), (200, Some(11), Some(7)));
    assert_eq!(rows[0].1, None);
}

#[test]
fn case_9_a_stream_arrives_as_it_is_sent() {
    let (org, stub, _, u) = ready();
    let mut req = Req::new("POST", "/api/org/v1/chat/completions", &org);
    req.headers.push(("X-Fonsviva-Request", "1".into()));
    req.headers.push(("Cookie", u.clone()));
    req.body = Some(json!({"model": "stub/tiny-1", "stream": true, "messages": []}).to_string());
    let t = Instant::now();
    let mut s = org.open(req);
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    let mut first = None;
    loop {
        let n = s.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
        if first.is_none() && holds(&got, "data: ") {
            first = Some(t.elapsed());
        }
    }
    let first = first.expect("an SSE chunk");
    assert!(
        first < Duration::from_millis(500),
        "first chunk after {first:?}"
    );
    assert!(t.elapsed() >= Duration::from_secs(1));
    let text = String::from_utf8(got).unwrap();
    assert!(text.contains("content-type: text/event-stream"), "{text}");
    assert!(
        !text.contains("x-stub-secret"),
        "no other upstream header is copied"
    );
    assert!(text.contains("one ") && text.contains("[DONE]"));
    assert_eq!(
        stub.state().seen[0].body["stream_options"],
        json!({"include_usage": true})
    );
    let rows = wait_rows(&org, 1);
    assert_eq!((rows[0].3, rows[0].4, rows[0].5), (200, Some(11), Some(7)));
}

#[test]
fn case_10_a_client_that_leaves_drops_the_upstream() {
    let (org, stub, _, u) = ready();
    stub.state().drip = true;
    let mut req = Req::new("POST", "/api/org/v1/chat/completions", &org);
    req.headers.push(("X-Fonsviva-Request", "1".into()));
    req.headers.push(("Cookie", u.clone()));
    req.body = Some(json!({"model": "stub/tiny-1", "stream": true}).to_string());
    let mut s = org.open(req);
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while !holds(&got, "drip") {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0);
        got.extend_from_slice(&buf[..n]);
    }
    let left = Instant::now();
    drop(s);
    let until = left + Duration::from_secs(2);
    while stub.state().closed.is_none() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    let closed = stub
        .state()
        .closed
        .expect("the stub saw its connection close within 2 s");
    assert!(closed.duration_since(left) < Duration::from_secs(2));
    assert_eq!(wait_rows(&org, 1)[0].3, 200);
}

#[test]
fn case_11_bad_chat_requests() {
    let (org, stub, _, u) = ready();
    for (body, status, code) in [
        (json!({"model": "stub/nope"}), 404, "unknown_model"),
        (json!({"model": "other/tiny-1"}), 404, "unknown_model"),
        (json!({"model": "tiny-1"}), 404, "unknown_model"),
        (json!({"messages": []}), 400, "bad_request"),
        (json!({"model": 7}), 400, "bad_request"),
        (json!(["stub/tiny-1"]), 400, "bad_request"),
    ] {
        let r = chat(&org, &u, body.clone());
        assert_eq!(
            (r.status, r.json()),
            (status, json!({"error": code})),
            "{body}"
        );
    }
    let mut req = Req::new("POST", "/api/org/v1/chat/completions", &org);
    req.headers.push(("X-Fonsviva-Request", "1".into()));
    req.headers.push(("Cookie", u.clone()));
    req.body = Some("{not json".into());
    let r = org.send(req);
    assert_eq!((r.status, r.json()), (400, json!({"error": "bad_request"})));
    assert!(stub.state().seen.is_empty());
    assert!(usage_rows(&org).is_empty());
}

#[test]
fn case_12_students_chat_only_when_allowed() {
    let (org, stub, a, _) = ready();
    let (s, _) = member(&org, &a, "student", "Cy");
    let body = json!({"model": "stub/tiny-1"});
    let r = chat(&org, &s, body.clone());
    assert_eq!(
        (r.status, r.json()),
        (403, json!({"error": "students_off"}))
    );
    assert!(stub.state().seen.is_empty());
    let p = org.call(
        "PATCH",
        "/api/org/v1/settings",
        Some(&a),
        json!({"studentsAllowed": true}),
    );
    assert_eq!(p.status, 200);
    assert_eq!(chat(&org, &s, body).status, 200);
    assert_eq!(stub.state().seen.len(), 1);
}

#[test]
fn case_20_an_upstream_error_passes_through() {
    let (org, stub, _, u) = ready();
    stub.state().fail = Some((500, "{\"error\":{\"message\":\"stub broke\"}}".into()));
    let r = chat(&org, &u, json!({"model": "stub/tiny-1"}));
    assert_eq!(r.status, 500);
    assert_eq!(r.body, "{\"error\":{\"message\":\"stub broke\"}}");
    let rows = wait_rows(&org, 1);
    assert_eq!((rows[0].3, rows[0].4, rows[0].5), (500, None, None));
}

#[test]
fn an_unreachable_source_is_502() {
    let (org, stub, _, u) = ready();
    assert_eq!(org.get("/api/org/v1/models", Some(&u)).status, 200);
    drop(stub);
    let r = chat(&org, &u, json!({"model": "stub/tiny-1"}));
    assert_eq!((r.status, r.json()), (502, json!({"error": "upstream"})));
}

#[test]
fn a_missing_secrets_key_is_502_and_sends_nothing() {
    let (org, stub, _, u) = ready();
    assert_eq!(
        org.get("/api/org/v1/models", Some(&u))
            .json()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    std::fs::remove_file(org.data.join("secrets.key")).unwrap();
    let r = chat(&org, &u, json!({"model": "stub/tiny-1", "messages": []}));
    assert_eq!((r.status, r.json()), (502, json!({"error": "upstream"})));
    assert!(
        stub.state().seen.is_empty(),
        "no chat request without the key"
    );
    assert_eq!(stub.state().model_asks.len(), 1);
}
