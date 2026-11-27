//! Slice 7, first half: the org brain's tool loop on a thread send and the
//! artifacts it makes, over real HTTP with the stub as the source.

mod org_support;

use org_support::stub::Stub;
use org_support::{Org, RESOLVE, add_source, admin, holds, member, start};
use serde_json::{Value, json};
use std::time::Duration;

const MODEL: &str = "stub/tiny-1";
const HTML: &str = "<!doctype html><p>caf\u{e9}</p><script>document.title='x'</script>";

/// An org with an admin, a user and the stub as `stub`: the org, the stub,
/// the admin's and the user's cookies and the user's id.
fn ready() -> (Org, Stub, String, String, String) {
    let org = start();
    let stub = Stub::start();
    let (a, _) = admin(&org);
    let r = add_source(&org, &a, "stub", &stub.base(), None);
    assert_eq!(r.status, 200, "{}", r.body);
    let (u, id) = member(&org, &a, "user", "Bo");
    (org, stub, a, u, id)
}

fn settings(org: &Org, admin: &str, patch: Value) {
    let r = org.call("PATCH", "/api/org/v1/settings", Some(admin), patch);
    assert_eq!(r.status, 200, "{}", r.body);
}

fn script(stub: &Stub, steps: Vec<Value>) {
    stub.state().script = steps;
}

fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"id": id, "name": name, "args": args})
}

fn make(id: &str, title: &str, kind: &str, content: &str) -> Value {
    call(
        id,
        "make_artifact",
        json!({"title": title, "kind": kind, "content": content}),
    )
}

fn events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

fn send(org: &Org, cookie: &str, text: &str, tools: bool) -> Vec<Value> {
    let r = org.call(
        "POST",
        "/api/org/v1/thread/messages",
        Some(cookie),
        json!({"text": text, "model": MODEL, "tools": tools}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    events(&r.body)
}

fn kinds(ev: &[Value]) -> Vec<&str> {
    ev.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

/// The tool results the stub got in its `n`th chat ask (0 for the first),
/// parsed.
fn results(stub: &Stub, n: usize) -> Vec<Value> {
    stub.state().seen[n].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| serde_json::from_str(m["content"].as_str().unwrap()).unwrap())
        .collect()
}

fn artifacts(org: &Org, cookie: &str) -> Vec<Value> {
    let r = org.get("/api/org/v1/artifacts", Some(cookie));
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()["artifacts"].as_array().unwrap().clone()
}

fn count(org: &Org, table: &str) -> i64 {
    org.db()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// Makes one artifact through the model and returns its id.
fn made(org: &Org, stub: &Stub, cookie: &str, title: &str, kind: &str, content: &str) -> String {
    let at = stub.state().seen.len();
    let mut steps = vec![json!({}); at];
    steps.push(json!({"calls": [make("c1", title, kind, content)]}));
    steps.push(json!({"text": "done"}));
    script(stub, steps);
    let ev = send(org, cookie, "make it", true);
    let a = ev
        .iter()
        .find(|e| e["type"] == "artifact")
        .expect("artifact");
    a["id"].as_str().unwrap().to_string()
}

const EXTRA: [&str; 4] = [
    "content-type",
    "x-content-type-options",
    "cross-origin-resource-policy",
    "cache-control",
];

fn served(org: &Org, cookie: &str, id: &str, q: &str) -> org_support::Resp {
    org.get(
        &format!("/api/org/v1/artifacts/{id}/content{q}"),
        Some(cookie),
    )
}

fn extra_headers(r: &org_support::Resp, ctype: &str) {
    let want = [ctype, "nosniff", "same-origin", "no-store"];
    for (h, v) in EXTRA.iter().zip(want) {
        assert_eq!(r.header(h), Some(v), "{h}");
    }
    assert_eq!(r.header("content-disposition"), Some("inline"));
}

#[test]
fn case_1_a_send_without_tools_is_slice_3() {
    let (org, stub, _a, u, _) = ready();
    let ev = send(&org, &u, "hello", false);
    assert_eq!(kinds(&ev), ["start", "delta", "delta", "done"]);
    assert!(ev[3].get("steps").is_none(), "{ev:?}");
    let body = stub.state().seen[0].body.clone();
    let mut keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["messages", "model", "stream", "stream_options"]);
    assert_eq!(count(&org, "message_steps"), 0);
    let t = org.get("/api/org/v1/thread", Some(&u)).json();
    assert!(t["messages"][0].get("steps").is_none(), "{t}");
}

#[test]
fn case_2_tools_offer_exactly_the_two_artifact_tools() {
    let (org, stub, _a, u, _) = ready();
    send(&org, &u, "hello", true);
    let tools = stub.state().seen[0].body["tools"].clone();
    let names: Vec<_> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["type"].clone(), t["function"]["name"].clone()))
        .collect();
    assert_eq!(
        names,
        [
            (json!("function"), json!("make_artifact")),
            (json!("function"), json!("update_artifact")),
        ]
    );
}

#[test]
fn case_3_a_made_artifact_streams_its_events_and_ties_to_the_reply() {
    let (org, stub, _a, u, _) = ready();
    script(
        &stub,
        vec![
            json!({"calls": [make("c1", "Page", "html", HTML)]}),
            json!({"text": "Made it."}),
        ],
    );
    let ev = send(&org, &u, "make a page", true);
    assert_eq!(
        kinds(&ev),
        ["start", "tool", "tool_result", "artifact", "delta", "done"]
    );
    assert_eq!(ev[1]["name"], "make_artifact");
    assert_eq!(ev[1]["step"], 1);
    assert_eq!(ev[1]["args"], json!({"kind": "html"}));
    assert_eq!(ev[2]["ok"], true);
    let id = ev[3]["id"].as_str().unwrap();
    assert_eq!(
        ev[3],
        json!({"type": "artifact", "id": id, "title": "Page", "kind": "html", "version": 1})
    );
    let done = &ev[5];
    assert_eq!((&done["steps"], &done["status"]), (&json!(2), &json!(200)));
    let list = artifacts(&org, &u);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["version"], 1);
    assert_eq!(list[0]["messageSeq"], done["seq"]);
    assert_eq!(count(&org, "usage"), 2);
    assert_eq!(results(&stub, 1), [json!({"id": id, "version": 1})]);
}

#[test]
fn case_4_html_is_served_sandboxed_byte_for_byte() {
    let (org, stub, _a, u, _) = ready();
    let id = made(&org, &stub, &u, "Page", "html", HTML);
    let r = served(&org, &u, &id, "");
    assert_eq!(r.status, 200, "{}", r.body);
    extra_headers(&r, "text/html; charset=utf-8");
    assert_eq!(
        r.header("content-security-policy"),
        Some(
            "sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; font-src data:"
        )
    );
    assert_eq!(r.body.as_bytes(), HTML.as_bytes());
}

#[test]
fn case_5_markdown_and_code_are_plain_text() {
    let (org, stub, _a, u, _) = ready();
    for kind in ["markdown", "code"] {
        let id = made(&org, &stub, &u, "Doc", kind, "<b>not html</b>");
        let r = served(&org, &u, &id, "");
        assert_eq!(r.status, 200);
        extra_headers(&r, "text/plain; charset=utf-8");
        assert_eq!(r.header("content-security-policy"), None);
        assert_eq!(r.body, "<b>not html</b>");
    }
}

#[test]
fn case_6_updates_add_versions_each_read_as_written() {
    let (org, stub, _a, u, _) = ready();
    let id = made(&org, &stub, &u, "Doc", "markdown", "one");
    let up = |c: &str, t: Option<&str>| {
        let mut args = json!({"id": id, "content": c});
        if let Some(t) = t {
            args["title"] = t.into();
        }
        call("u", "update_artifact", args)
    };
    let at = stub.state().seen.len();
    let mut steps = vec![json!({}); at];
    steps.push(json!({"calls": [up("two", None)]}));
    steps.push(json!({"calls": [up("three", Some("Doc 3"))]}));
    steps.push(json!({"text": "ok"}));
    script(&stub, steps);
    let ev = send(&org, &u, "change it", true);
    let versions: Vec<_> = ev
        .iter()
        .filter(|e| e["type"] == "artifact")
        .map(|e| e["version"].clone())
        .collect();
    assert_eq!(versions, [json!(2), json!(3)]);
    let one = org
        .get(&format!("/api/org/v1/artifacts/{id}"), Some(&u))
        .json();
    assert_eq!(one["title"], "Doc 3");
    let v: Vec<_> = one["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| (v["version"].clone(), v["bytes"].clone()))
        .collect();
    assert_eq!(
        v,
        [
            (json!(1), json!(3)),
            (json!(2), json!(3)),
            (json!(3), json!(5))
        ]
    );
    for (q, want) in [("?version=1", "one"), ("?version=2", "two"), ("", "three")] {
        assert_eq!(served(&org, &u, &id, q).body, want, "{q}");
    }
    assert_eq!(served(&org, &u, &id, "?version=4").status, 404);
}

#[test]
fn case_7_bad_args_are_answered_and_the_loop_goes_on() {
    let (org, stub, a, u, _) = ready();
    let theirs = made(&org, &stub, &a, "Admin's", "markdown", "x");
    let big = "x".repeat((1 << 20) + 1);
    let at = stub.state().seen.len();
    let mut steps = vec![json!({}); at];
    steps.push(json!({"calls": [
        call("b1", "make_artifact", json!({"kind": "html", "content": "x"})),
        call("b2", "make_artifact", json!({"title": "t", "kind": "pdf", "content": "x"})),
        make("b3", "t", "markdown", &big),
        call("b4", "make_artifact", json!({"title": "t", "kind": "markdown", "language": "rust", "content": "x"})),
    ]}));
    steps.push(json!({"calls": [
        call("b5", "update_artifact", json!({"id": theirs, "content": "mine now"})),
        call("b6", "update_artifact", json!({"id": "art_0000000000000000", "content": "x"})),
        call("b7", "make_artifact", json!("not json")),
    ]}));
    steps.push(json!({"text": "sorry"}));
    script(&stub, steps);
    let ev = send(&org, &u, "try", true);
    assert_eq!(ev.last().unwrap()["steps"], 3);
    let e = |c: &str| json!({"error": c});
    assert_eq!(
        results(&stub, at + 1),
        [e("bad_args"), e("bad_args"), e("too_large"), e("bad_args")]
    );
    assert_eq!(
        results(&stub, at + 2)[4..],
        [e("not_found"), e("not_found"), e("bad_args")]
    );
    assert!(artifacts(&org, &u).is_empty());
    assert_eq!(served(&org, &a, &theirs, "").body, "x");
    assert_eq!(count(&org, "artifact_versions"), 1);
}

#[test]
fn case_8_a_tool_not_offered_is_not_offered() {
    let (org, stub, _a, u, _) = ready();
    script(
        &stub,
        vec![
            json!({"calls": [call("s1", "web_search", json!({"query": "rust"}))]}),
            json!({"text": "no search"}),
        ],
    );
    let ev = send(&org, &u, "search", true);
    assert_eq!(results(&stub, 1), [json!({"error": "not_offered"})]);
    let r = ev.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(
        (&r["ok"], &r["error"]),
        (&json!(false), &json!("not_offered"))
    );
}

#[test]
fn case_9_the_eighth_step_offers_no_tools() {
    let (org, stub, _a, u, _) = ready();
    let again = call(
        "m",
        "update_artifact",
        json!({"id": "art_x", "content": "x"}),
    );
    script(&stub, vec![json!({"calls": [again]}); 8]);
    let ev = send(&org, &u, "loop", true);
    assert_eq!(ev.last().unwrap()["steps"], 8);
    let seen = &stub.state().seen;
    assert_eq!(seen.len(), 8);
    for (i, s) in seen.iter().enumerate() {
        assert_eq!(s.body.get("tools").is_some(), i < 7, "step {}", i + 1);
    }
}

#[test]
fn case_10_a_step_runs_four_calls() {
    let (org, stub, _a, u, _) = ready();
    let calls: Vec<_> = (0..5)
        .map(|i| make(&format!("c{i}"), &format!("A{i}"), "markdown", "x"))
        .collect();
    script(
        &stub,
        vec![json!({"calls": calls}), json!({"text": "done"})],
    );
    send(&org, &u, "five", true);
    let got = results(&stub, 1);
    assert_eq!(got.len(), 5);
    assert!(got[..4].iter().all(|r| r["version"] == 1), "{got:?}");
    assert_eq!(got[4], json!({"error": "too_many_calls"}));
    assert_eq!(artifacts(&org, &u).len(), 4);
}

#[test]
fn case_11_with_threads_not_kept_nothing_is_stored() {
    let (org, stub, a, u, _) = ready();
    settings(&org, &a, json!({"keepThreads": false}));
    script(
        &stub,
        vec![
            json!({"calls": [
                make("c1", "Page", "html", HTML),
                call("c2", "update_artifact", json!({"id": "art_x", "content": "y"})),
            ]}),
            json!({"text": "here"}),
        ],
    );
    let ev = send(&org, &u, "page", true);
    let art = ev.iter().find(|e| e["type"] == "artifact").unwrap();
    assert_eq!(
        art,
        &json!({"type": "artifact", "id": null, "title": "Page", "kind": "html", "version": 1, "content": HTML})
    );
    assert_eq!(
        results(&stub, 1),
        [
            json!({"id": null, "kept": false}),
            json!({"error": "not_kept"})
        ]
    );
    for t in ["artifacts", "artifact_versions", "message_steps"] {
        assert_eq!(count(&org, t), 0, "{t}");
    }
}

#[test]
fn case_12_the_thread_shows_steps_without_results() {
    let (org, stub, _a, u, _) = ready();
    let id = made(&org, &stub, &u, "Page", "html", HTML);
    let r = org.get("/api/org/v1/thread", Some(&u));
    let reply = &r.json()["messages"][0];
    assert_eq!(reply["role"], "assistant");
    assert_eq!(
        reply["steps"],
        json!([{"name": "make_artifact", "args": {"kind": "html"}, "ok": true}])
    );
    assert!(!r.body.contains(&id), "{}", r.body);
    assert!(!r.body.contains("caf"), "{}", r.body);
}

#[test]
fn case_13_steps_go_with_their_reply_and_thread() {
    let (org, stub, _a, u, _) = ready();
    let missing = |m: &str| call("x", "update_artifact", json!({"id": m, "content": "x"}));
    script(
        &stub,
        vec![
            json!({"calls": [missing("MARKSTEPONE")]}),
            json!({"text": "first"}),
            json!({"calls": [missing("MARKSTEPTWO"), make("c", "T", "markdown", "x")]}),
            json!({"text": "second"}),
        ],
    );
    let one = send(&org, &u, "a", true);
    let two = send(&org, &u, "b", true);
    let art = two.iter().find(|e| e["type"] == "artifact").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(count(&org, "message_steps"), 3);
    let seq = one.last().unwrap()["seq"].as_i64().unwrap();
    let path = format!("/api/org/v1/thread/messages/{seq}");
    assert_eq!(org.call("DELETE", &path, Some(&u), json!({})).status, 204);
    assert_eq!(count(&org, "message_steps"), 2);
    assert!(!holds(&org.db_bytes(), "MARKSTEPONE"));
    assert!(holds(&org.db_bytes(), "MARKSTEPTWO"));
    let path = format!("/api/org/v1/artifacts/{art}");
    assert_eq!(org.call("DELETE", &path, Some(&u), json!({})).status, 204);
    let r = org.call("DELETE", "/api/org/v1/thread", Some(&u), json!({}));
    assert_eq!(r.status, 204);
    assert_eq!(count(&org, "message_steps"), 0);
    let bytes = org.db_bytes();
    assert!(!holds(&bytes, "MARKSTEPTWO"));
    assert!(!holds(&bytes, &art));
}

#[test]
fn case_14_an_artifact_delete_leaves_no_content_or_title() {
    let (org, stub, a, u, uid) = ready();
    settings(&org, &a, json!({"adminsReadThreads": true}));
    let id = made(&org, &stub, &u, "MARKTITLE14", "markdown", "MARKARTIFACT14");
    for m in ["MARKARTIFACT14", "MARKTITLE14"] {
        assert!(holds(&org.db_bytes(), m), "{m}");
    }
    let path = format!("/api/org/v1/artifacts/{id}");
    let r = org.call("DELETE", &path, Some(&a), json!({}));
    assert_eq!(r.status, 404);
    assert_eq!(org.call("DELETE", &path, Some(&u), json!({})).status, 204);
    let reads = [
        org.get("/api/org/v1/thread", Some(&u)),
        org.get(&format!("/api/org/v1/members/{uid}/thread"), Some(&a)),
    ];
    for r in &reads {
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("make_artifact"), "{}", r.body);
    }
    let bytes = org.db_bytes();
    for m in ["MARKARTIFACT14", "MARKTITLE14"] {
        assert!(!holds(&bytes, m), "{m} in org.db");
        for r in &reads {
            assert!(!r.body.contains(m), "{m} in {}", r.body);
        }
    }
    assert_eq!(org.get(&path, Some(&u)).status, 404);
    assert_eq!(count(&org, "artifact_versions"), 0);
}

#[test]
fn case_15_an_admin_reads_only_while_admins_read_threads() {
    let (org, stub, a, u, _) = ready();
    let id = made(&org, &stub, &u, "Doc", "markdown", "secret");
    let one = format!("/api/org/v1/artifacts/{id}");
    for path in [one.clone(), format!("{one}/content")] {
        let r = org.get(&path, Some(&a));
        assert_eq!(
            (r.status, r.json()),
            (403, json!({"error": "threads_private"}))
        );
    }
    settings(&org, &a, json!({"adminsReadThreads": true}));
    assert_eq!(org.get(&one, Some(&a)).status, 200);
    let r = served(&org, &a, &id, "");
    assert_eq!((r.status, r.body.as_str()), (200, "secret"));
    let log = org.get("/api/org/v1/audit", Some(&a)).json();
    let reads: Vec<_> = log
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "artifact.read")
        .map(|e| e["target"].clone())
        .collect();
    assert_eq!(reads, [json!(id), json!(id)]);
}

#[test]
fn case_16_another_member_gets_404_everywhere() {
    let (org, stub, a, u, _) = ready();
    let id = made(&org, &stub, &u, "Doc", "markdown", "x");
    let (other, _) = member(&org, &a, "full", "Cy");
    let gone = "art_ffffffffffffffff";
    for target in [id.as_str(), gone] {
        let one = format!("/api/org/v1/artifacts/{target}");
        let got = [
            org.get(&one, Some(&other)),
            org.get(&format!("{one}/content"), Some(&other)),
            org.call("DELETE", &one, Some(&other), json!({})),
        ];
        for r in got {
            assert_eq!((r.status, r.json()), (404, json!({"error": "not_found"})));
        }
    }
    assert!(artifacts(&org, &other).is_empty());
    assert_eq!(artifacts(&org, &u).len(), 1);
}

#[test]
fn case_17_a_daily_limit_between_steps_ends_the_send() {
    let (org, stub, a, u, _) = ready();
    settings(&org, &a, json!({"dailyTokens": 10}));
    script(
        &stub,
        vec![
            json!({"calls": [make("c1", "Kept", "markdown", "x")]}),
            json!({"text": "never"}),
        ],
    );
    let ev = send(&org, &u, "go", true);
    let done = ev.last().unwrap();
    assert_eq!(
        (&done["status"], &done["error"], &done["steps"]),
        (&json!(429), &json!("daily_limit"), &json!(1))
    );
    assert_eq!(stub.state().seen.len(), 1);
    let list = artifacts(&org, &u);
    assert_eq!(list.len(), 1);
    assert!(done["seq"].is_i64(), "{done}");
    assert_eq!(list[0]["messageSeq"], done["seq"]);
    assert_eq!(count(&org, "usage"), 1);
    let t = org.get("/api/org/v1/thread", Some(&u)).json();
    let reply = &t["messages"][0];
    assert_eq!((&reply["seq"], &reply["text"]), (&done["seq"], &json!("")));
    assert_eq!(
        reply["steps"],
        json!([{"name": "make_artifact", "args": {"kind": "markdown"}, "ok": true}])
    );
}

#[test]
fn case_18_no_marker_leaves_the_thread_and_its_artifacts() {
    let (mut org, stub, a, u, uid) = ready();
    settings(&org, &a, json!({"adminsReadThreads": true}));
    script(
        &stub,
        vec![
            json!({"calls": [
                make("c1", "MARKTITLE18", "markdown", "MARKCONTENT18"),
                call("c2", "make_artifact", json!({"title": "MARKARGS18", "kind": "pdf", "content": "x"})),
            ]}),
            json!({"text": "ok"}),
        ],
    );
    let r = org.call(
        "POST",
        "/api/org/v1/thread/messages",
        Some(&u),
        json!({"text": "mark", "model": MODEL, "tools": true}),
    );
    let ev = events(&r.body);
    let id = ev.iter().find(|e| e["type"] == "artifact").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(served(&org, &a, &id, "").status, 200);
    let mut bodies: Vec<(String, String)> = ev
        .iter()
        .filter(|e| e["type"] != "artifact")
        .map(|e| ("send".to_string(), e.to_string()))
        .collect();
    let gets = [
        "/api/org/v1/me",
        "/api/org/v1/members",
        "/api/org/v1/invites",
        "/api/org/v1/audit",
        "/api/org/v1/sources",
        "/api/org/v1/models",
        "/api/org/v1/settings",
        "/api/org/v1/keys",
        "/api/org/v1/usage",
        "/api/org/v1/usage/totals",
        "/api/org/v1/me/usage",
        "/api/org/v1/me/limits",
        "/api/org/v1/privacy",
        "/api/org/v1/compute",
        "/api/org/v1/me/compute",
        &format!("/api/org/v1/members/{uid}/limits"),
        &format!("/api/org/v1/artifacts/{id}/content?version=x"),
        "/api/org/v1/artifacts/art_ffffffffffffffff",
    ];
    for path in gets {
        for who in [&a, &u] {
            bodies.push((path.to_string(), org.get(path, Some(who)).body));
        }
    }
    let db = org.db();
    let tables: Vec<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let thread = [
        "messages",
        "message_steps",
        "artifacts",
        "artifact_versions",
    ];
    let tables: Vec<String> = tables
        .into_iter()
        .filter(|t| !thread.contains(&t.as_str()) && !t.starts_with("messages_fts"))
        .collect();
    assert!(tables.iter().any(|t| t == "org"), "{tables:?}");
    for table in &tables {
        let mut q = db.prepare(&format!("SELECT * FROM \"{table}\"")).unwrap();
        let n = q.column_count();
        let rows = q
            .query_map([], |r| {
                Ok((0..n)
                    .map(|i| format!("{:?}", r.get::<_, rusqlite::types::Value>(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .unwrap();
        let text = rows.map(Result::unwrap).collect::<Vec<_>>().join("\n");
        bodies.push((table.clone(), text));
    }
    drop(db);
    let audit = &bodies.iter().find(|(w, _)| w == "audit").unwrap().1;
    assert!(audit.contains("artifact.read"), "{audit}");
    bodies.push(("log".into(), org.stop()));
    for m in ["MARKTITLE18", "MARKCONTENT18", "MARKARGS18"] {
        for (what, text) in &bodies {
            assert!(!text.contains(m), "{m} in {what}");
        }
    }
}

#[test]
fn case_19_artifact_and_version_caps() {
    let (org, stub, _a, u, id) = ready();
    {
        let db = org.db();
        db.busy_timeout(Duration::from_secs(5)).unwrap();
        for i in 0..500 {
            db.execute(
                "INSERT INTO artifacts (id, member_id, title, kind, created_at, updated_at)
                 VALUES (?1, ?2, 't', 'markdown', 0, 0)",
                rusqlite::params![format!("art_seed{i}"), id],
            )
            .unwrap();
        }
        for v in 1..=20 {
            db.execute(
                "INSERT INTO artifact_versions (artifact_id, version, content, created_at)
                 VALUES ('art_seed0', ?1, 'x', 0)",
                [v],
            )
            .unwrap();
        }
    }
    script(
        &stub,
        vec![
            json!({"calls": [
                make("c1", "One more", "markdown", "x"),
                call("c2", "update_artifact", json!({"id": "art_seed0", "content": "x"})),
            ]}),
            json!({"text": "full"}),
        ],
    );
    send(&org, &u, "more", true);
    assert_eq!(
        results(&stub, 1),
        [
            json!({"error": "too_many_artifacts"}),
            json!({"error": "too_many_versions"})
        ]
    );
    assert_eq!(count(&org, "artifacts"), 500);
    assert_eq!(count(&org, "artifact_versions"), 20);
}

#[test]
fn case_28_an_org_of_the_previous_schema_migrates() {
    let (mut org, stub, _a, u, _) = ready();
    send(&org, &u, "before", false);
    let version = |org: &Org| -> i64 {
        org.db()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    };
    let current = version(&org);
    org.db()
        .execute_batch(&format!(
            "DROP TABLE artifact_versions; DROP TABLE artifacts; DROP TABLE message_steps; PRAGMA user_version = {};",
            current - 1
        ))
        .unwrap();
    let before = count(&org, "messages");
    org.restart(RESOLVE);
    assert_eq!(version(&org), current);
    assert_eq!(count(&org, "messages"), before);
    let at = stub.state().seen.len();
    let mut steps = vec![json!({}); at];
    steps.push(json!({"calls": [make("c1", "After", "markdown", "x")]}));
    steps.push(json!({"text": "ok"}));
    script(&stub, steps);
    let ev = send(&org, &u, "after", true);
    assert_eq!(ev.last().unwrap()["status"], 200);
    assert_eq!(artifacts(&org, &u).len(), 1);
    assert_eq!(count(&org, "message_steps"), 1);
}

#[test]
fn followup_1_html_frames_get_no_origin_navigation_or_popups() {
    let (org, stub, _a, u, _) = ready();
    let page = r#"<meta http-equiv="refresh" content="0;url=https://example.invalid/">"#;
    let id = made(&org, &stub, &u, "Page", "html", page);
    let r = served(&org, &u, &id, "");
    assert_eq!(r.body, page);
    let csp = r.header("content-security-policy").unwrap();
    let sandbox: Vec<&str> = csp
        .split(';')
        .map(str::trim)
        .find(|d| d.starts_with("sandbox"))
        .unwrap()
        .split_whitespace()
        .collect();
    assert_eq!(sandbox, ["sandbox", "allow-scripts"], "{csp}");
    for no in ["allow-same-origin", "allow-top-navigation", "allow-popups"] {
        assert!(!csp.contains(no), "{no} in {csp}");
    }
}

/// The calls the stub's `n`th chat ask carried back in its assistant
/// message, as (id, name, arguments).
fn sent_calls(stub: &Stub, n: usize) -> Vec<(Value, Value, Value)> {
    let m = &stub.state().seen[n].body["messages"];
    let a = m
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant");
    a.unwrap()["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let f = &c["function"];
            (c["id"].clone(), f["name"].clone(), f["arguments"].clone())
        })
        .collect()
}

#[test]
fn followup_3_a_repeated_id_and_name_make_one_call() {
    let (org, stub, _a, u, _) = ready();
    script(
        &stub,
        vec![
            json!({"calls": [make("c1", "Page", "markdown", "x")], "repeat": true, "chunk": 8}),
            json!({"text": "done"}),
        ],
    );
    let ev = send(&org, &u, "make", true);
    let tool = ev.iter().find(|e| e["type"] == "tool").unwrap();
    assert_eq!(tool["name"], "make_artifact");
    assert_eq!(sent_calls(&stub, 1)[0].0, "c1");
    assert_eq!(sent_calls(&stub, 1)[0].1, "make_artifact");
    assert_eq!(results(&stub, 1)[0]["version"], 1);
    assert_eq!(artifacts(&org, &u).len(), 1);
}

#[test]
fn followup_4_calls_with_no_index_in_one_delta_each_run() {
    let (org, stub, _a, u, _) = ready();
    script(
        &stub,
        vec![
            json!({"calls": [
                make("c0", "A0", "markdown", "x"),
                call("c1", "update_artifact", json!({"id": "art_x", "content": "y"})),
            ], "together": true}),
            json!({"text": "done"}),
        ],
    );
    send(&org, &u, "two", true);
    let calls: Vec<_> = sent_calls(&stub, 1)
        .into_iter()
        .map(|(id, name, _)| (id, name))
        .collect();
    assert_eq!(
        calls,
        [
            (json!("c0"), json!("make_artifact")),
            (json!("c1"), json!("update_artifact"))
        ]
    );
    let got = results(&stub, 1);
    assert_eq!(got[0]["version"], 1, "{got:?}");
    assert_eq!(got[1], json!({"error": "not_found"}));
    assert_eq!(artifacts(&org, &u).len(), 1);
}

#[test]
fn followup_5_args_over_the_limit_go_back_as_an_empty_object() {
    let (org, stub, _a, u, _) = ready();
    let big = "x".repeat(9 << 20);
    script(
        &stub,
        vec![
            json!({"calls": [make("c1", "Big", "markdown", &big)], "chunk": 256 << 10}),
            json!({"text": "ok"}),
        ],
    );
    let ev = send(&org, &u, "big", true);
    assert_eq!(
        sent_calls(&stub, 1),
        [(json!("c1"), json!("make_artifact"), json!("{}"))]
    );
    assert_eq!(results(&stub, 1), [json!({"error": "too_large"})]);
    let done = ev.last().unwrap();
    assert_eq!(
        (&done["type"], &done["status"]),
        (&json!("done"), &json!(200))
    );
    assert!(done.get("error").is_none(), "{done}");
}

#[test]
fn followup_6_step_texts_join_with_a_blank_line() {
    let (org, stub, _a, u, _) = ready();
    script(
        &stub,
        vec![
            json!({"text": "a", "calls": [make("c1", "T", "markdown", "x")]}),
            json!({"text": "b"}),
            json!({"calls": [make("c2", "T", "markdown", "x")]}),
            json!({"text": "c"}),
        ],
    );
    send(&org, &u, "one", true);
    send(&org, &u, "two", true);
    let t = org.get("/api/org/v1/thread", Some(&u)).json();
    let texts: Vec<_> = t["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .map(|m| m["text"].clone())
        .collect();
    assert_eq!(texts, [json!("c"), json!("a\n\nb")]);
    let step = &stub.state().seen[1].body["messages"];
    let a = step
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant");
    assert_eq!(a.unwrap()["content"], "a");
}

#[test]
fn followup_8_a_reply_and_its_steps_commit_together() {
    let (org, stub, _a, u, _) = ready();
    org.db()
        .execute_batch(
            "CREATE TRIGGER planted BEFORE INSERT ON message_steps
             BEGIN SELECT RAISE(ABORT, 'planted'); END;",
        )
        .unwrap();
    script(
        &stub,
        vec![
            json!({"calls": [make("c1", "T", "markdown", "x")]}),
            json!({"text": "MARKREPLY8"}),
        ],
    );
    let ev = send(&org, &u, "go", true);
    let done = ev.last().unwrap();
    assert_eq!(
        (&done["type"], &done["seq"]),
        (&json!("done"), &Value::Null)
    );
    let roles: Vec<String> = org
        .db()
        .prepare("SELECT role FROM messages")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(roles, ["user"]);
    assert_eq!(count(&org, "message_steps"), 0);
    let list = artifacts(&org, &u);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["messageSeq"], Value::Null);
}
