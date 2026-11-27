//! The chat proxy and its usage rows, against the real `fonsviva org
//! serve` and a stub source on 127.0.0.1.

mod org_support;

use org_support::stub::{Stub, inline_part};
use org_support::{
    Org, RESOLVE, Req, SOURCE_KEY as KEY, add_source, admin, dechunk, holds, member, ready,
    start_resolving,
};
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

fn chat_req<'a>(org: &'a Org, cookie: &str, body: Value) -> Req<'a> {
    let mut req = Req::new("POST", "/api/org/v1/chat/completions", org);
    req.headers.push(("X-Fonsviva-Request", "1".into()));
    req.headers.push(("Cookie", cookie.to_string()));
    req.body = Some(body.to_string());
    req
}

/// Reads an answer to its end, or to a broken connection.
fn read_raw(mut s: std::net::TcpStream) -> String {
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = s.read(&mut buf) {
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&got).to_string()
}

/// Reads a whole answer and returns its head and its body with the chunked
/// framing taken off.
fn read_all(s: std::net::TcpStream) -> (String, String) {
    let got = read_raw(s).into_bytes();
    let at = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&got[..at]).to_string();
    let body = String::from_utf8_lossy(&dechunk(&got[at + 4..])).to_string();
    (head, body)
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
    stub.state().hold = true;
    let body = json!({"model": "stub/tiny-1", "stream": true, "messages": []});
    let mut s = org.open(chat_req(&org, &u, body));
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while !holds(&got, "data: ") {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "the stream ended before its first chunk");
        got.extend_from_slice(&buf[..n]);
    }
    assert!(
        stub.state().finished.is_none(),
        "the first chunk arrived while the stub still held its last"
    );
    stub.state().released = true;
    loop {
        let n = s.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
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
fn a_stream_always_counts_usage_and_shows_it_only_when_asked() {
    let (org, stub, _, u) = ready();
    for (i, opts, shown) in [
        (0, None, false),
        (1, Some(json!({"include_usage": false})), false),
        (2, Some(json!({"include_usage": true})), true),
    ] {
        let mut body = json!({"model": "stub/tiny-1", "stream": true, "messages": []});
        if let Some(o) = &opts {
            body["stream_options"] = o.clone();
        }
        let (head, text) = read_all(org.open(chat_req(&org, &u, body)));
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(
            stub.state().seen[i].body["stream_options"],
            json!({"include_usage": true}),
            "{opts:?}"
        );
        assert_eq!(text.contains("\"usage\""), shown, "{opts:?}: {text}");
        assert!(
            text.contains("\"one \"") && text.contains("\"two\""),
            "{text}"
        );
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
        assert!(!text.contains("\n\n\n"), "{text}");
        let rows = wait_rows(&org, i + 1);
        assert_eq!(
            (rows[i].3, rows[i].4, rows[i].5),
            (200, Some(11), Some(7)),
            "{opts:?}"
        );
    }
}

#[test]
fn a_client_gone_before_the_headers_still_writes_a_row() {
    let (org, stub, _, u) = ready();
    stub.state().stall = true;
    let s = org.open(chat_req(&org, &u, json!({"model": "stub/tiny-1"})));
    let until = Instant::now() + Duration::from_secs(5);
    while stub.state().seen.is_empty() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(stub.state().seen.len(), 1);
    drop(s);
    let rows = wait_rows(&org, 1);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let sent = json!({"model": "stub/tiny-1"}).to_string().len() as i64;
    assert_eq!(
        (rows[0].3, rows[0].4, rows[0].5),
        (499, Some((sent + 3) / 4), Some(0)),
        "an estimate of what was sent, and nothing received"
    );
}

#[test]
fn a_stream_cut_after_the_headers_is_502_in_its_row() {
    let (org, stub, _, u) = ready();
    stub.state().cut = true;
    let body = json!({"model": "stub/tiny-1", "stream": true, "messages": []});
    let text = read_raw(org.open(chat_req(&org, &u, body.clone())));
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.contains("one ") && !text.contains("[DONE]"), "{text}");
    let rows = wait_rows(&org, 1);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let sent = body.to_string().len() as i64;
    assert_eq!(
        (rows[0].3, rows[0].4, rows[0].5),
        (502, Some((sent + 3) / 4), Some(1)),
        "the bytes it delivered are estimated"
    );
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
    let row = &wait_rows(&org, 1)[0];
    assert_eq!(row.3, 499, "the client left first");
    assert!(row.5.is_some_and(|c| c > 0), "{row:?}");
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
    let rows = wait_rows(&org, 1);
    assert_eq!((rows[0].3, rows[0].4, rows[0].5), (502, None, None));
}

#[test]
fn a_source_down_with_no_model_list_yet_is_502() {
    let (org, stub, _, u) = ready();
    assert!(stub.state().model_asks.is_empty());
    drop(stub);
    let r = chat(&org, &u, json!({"model": "stub/tiny-1"}));
    assert_eq!((r.status, r.json()), (502, json!({"error": "upstream"})));
    let rows = wait_rows(&org, 1);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0].2, "stub",
        "a model not yet listed is kept as its source"
    );
    assert_eq!((rows[0].3, rows[0].4, rows[0].5), (502, None, None));
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
    let rows = wait_rows(&org, 1);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].3, rows[0].4, rows[0].5), (502, None, None));
}

#[test]
fn a_chat_to_a_local_name_that_leaves_the_network_is_502() {
    let stub = Stub::start();
    let mut org = start_resolving(&format!("{RESOLVE};flip.test=127.0.0.1"));
    let (a, _) = admin(&org);
    let base = format!("http://flip.test:{}/v1", stub.port);
    let r = add_source(&org, &a, "flip", &base, Some("sk-flip"));
    assert_eq!((r.status, r.json()["local"].clone()), (200, json!(true)));
    let (u, _) = member(&org, &a, "user", "Bo");
    assert_eq!(chat(&org, &u, json!({"model": "flip/tiny-1"})).status, 200);
    assert_eq!(stub.state().seen.len(), 1);
    org.restart(&format!("{RESOLVE};flip.test=203.0.113.7"));
    let r = chat(&org, &u, json!({"model": "flip/tiny-1"}));
    assert_eq!((r.status, r.json()), (502, json!({"error": "upstream"})));
    assert_eq!(
        stub.state().seen.len(),
        1,
        "no chat request off the network"
    );
    let rows = wait_rows(&org, 2);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1].2, "flip");
    assert_eq!((rows[1].3, rows[1].4, rows[1].5), (502, None, None));
}

#[test]
fn a_failed_model_list_is_asked_again_for_chat() {
    let (org, stub, _, u) = ready();
    stub.state().models_fail = 1;
    assert_eq!(org.get("/api/org/v1/models", Some(&u)).json(), json!([]));
    std::thread::sleep(Duration::from_millis(2100));
    let r = chat(&org, &u, json!({"model": "stub/tiny-1"}));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(stub.state().seen.len(), 1);
    assert_eq!(stub.state().model_asks.len(), 2);
}

#[test]
fn an_upstream_auth_error_is_502_upstream_auth() {
    let (org, stub, _, u) = ready();
    for code in [401, 403] {
        stub.state().fail = Some((code, "{\"error\":\"stub-auth-secret\"}".into()));
        let r = chat(&org, &u, json!({"model": "stub/tiny-1"}));
        assert_eq!(
            (r.status, r.json()),
            (502, json!({"error": "upstream_auth"})),
            "{code}"
        );
        assert!(!r.body.contains("stub-auth-secret"));
    }
    let rows = wait_rows(&org, 2);
    assert_eq!(rows.iter().map(|r| r.3).collect::<Vec<_>>(), vec![401, 403]);
}

#[test]
fn an_overlong_model_is_400_with_no_row() {
    let (org, stub, _, u) = ready();
    drop(stub);
    let model = format!("stub/{}", "a".repeat((16 << 20) - 100));
    let r = chat(&org, &u, json!({"model": model}));
    assert_eq!((r.status, r.json()), (400, json!({"error": "bad_request"})));
    std::thread::sleep(Duration::from_millis(300));
    assert!(usage_rows(&org).is_empty());
    let at_limit = json!({"model": format!("stub/{}", "a".repeat(195))});
    assert_eq!(
        chat(&org, &u, at_limit).status,
        502,
        "200 bytes is the limit"
    );
    let rows = wait_rows(&org, 1);
    assert_eq!((rows.len(), rows[0].2.as_str()), (1, "stub"));
}

#[test]
fn a_stripped_stream_drops_usage_beside_choices() {
    let (org, stub, _, u) = ready();
    stub.state().inline_usage = true;
    for (i, asked) in [(0, false), (1, true)] {
        let body = json!({
            "model": "stub/tiny-1",
            "stream": true,
            "stream_options": {"include_usage": asked},
        });
        let (_, text) = read_all(org.open(chat_req(&org, &u, body)));
        for t in ["one ", "two"] {
            let line = format!("{}\n\n", inline_part(t));
            assert!(text.contains(&line), "{asked}: byte for byte: {text}");
        }
        assert!(text.contains("\"finish_reason\":\"stop\""), "{text}");
        assert_eq!(text.contains("\"prompt_tokens\":11"), asked, "{text}");
        let rows = wait_rows(&org, i + 1);
        assert_eq!((rows[i].4, rows[i].5), (Some(11), Some(7)), "{asked}");
    }
}

#[test]
fn a_source_refusing_its_key_on_the_list_is_upstream_auth() {
    let (org, stub, _, u) = ready();
    {
        let mut st = stub.state();
        st.models_fail = 100;
        st.models_code = 401;
        st.fail = Some((401, "{}".into()));
    }
    let r = chat(&org, &u, json!({"model": "stub/tiny-1"}));
    assert_eq!(
        (r.status, r.json()),
        (502, json!({"error": "upstream_auth"}))
    );
    assert!(stub.state().seen.is_empty());
    let rows = wait_rows(&org, 1);
    assert_eq!(
        (rows.len(), rows[0].2.as_str(), rows[0].3),
        (1, "stub", 502)
    );
}

#[test]
fn two_chats_on_one_failing_list_share_one_fetch() {
    let (org, stub, _, u) = ready();
    {
        let mut st = stub.state();
        st.models_fail = 100;
        st.models_delay = Duration::from_millis(500);
    }
    let codes: Vec<u16> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..2)
            .map(|_| sc.spawn(|| chat(&org, &u, json!({"model": "stub/tiny-1"})).status))
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(codes, vec![502, 502]);
    assert_eq!(stub.state().model_asks.len(), 1);
}

#[test]
fn a_burst_of_chats_after_a_failed_list_asks_no_list() {
    let (org, stub, _, u) = ready();
    stub.state().models_fail = 100;
    assert_eq!(org.get("/api/org/v1/models", Some(&u)).json(), json!([]));
    let t = Instant::now();
    for _ in 0..10 {
        let r = chat(&org, &u, json!({"model": "stub/tiny-1"}));
        assert_eq!(r.status, 502);
    }
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(stub.state().model_asks.len(), 1);
}

#[test]
fn a_client_gone_during_the_model_check_writes_no_row() {
    let (org, stub, _, u) = ready();
    stub.state().models_delay = Duration::from_secs(1);
    let s = org.open(chat_req(&org, &u, json!({"model": "stub/tiny-1"})));
    let until = Instant::now() + Duration::from_secs(5);
    while stub.state().model_asks.is_empty() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(stub.state().model_asks.len(), 1);
    drop(s);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(stub.state().seen.is_empty());
    assert!(usage_rows(&org).is_empty(), "{:?}", usage_rows(&org));
}
