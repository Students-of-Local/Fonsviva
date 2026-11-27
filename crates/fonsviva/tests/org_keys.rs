//! Personal keys, the `/v1` door for harnesses, the daily limit and usage,
//! against the real `fonsviva org serve` and a stub source on 127.0.0.1.

mod org_support;

use org_support::stub::Stub;
use org_support::{Org, Req, Resp, add_source, admin, holds, member, start};
use serde_json::{Value, json};

/// An org with an admin and the stub added as `stub`.
fn ready() -> (Org, Stub, String) {
    let org = start();
    let stub = Stub::start();
    let (a, _) = admin(&org);
    let r = add_source(&org, &a, "stub", &stub.base(), Some("sk-stub-upstream"));
    assert_eq!(r.status, 200, "{}", r.body);
    (org, stub, a)
}

fn make_key(org: &Org, cookie: &str, name: &str) -> Resp {
    org.call(
        "POST",
        "/api/org/v1/keys",
        Some(cookie),
        json!({"name": name}),
    )
}

fn key_of(org: &Org, cookie: &str) -> (String, String) {
    let r = make_key(org, cookie, "laptop");
    assert_eq!(r.status, 200, "{}", r.body);
    let v = r.json();
    (
        v["id"].as_str().unwrap().to_string(),
        v["key"].as_str().unwrap().to_string(),
    )
}

fn v1_chat(org: &Org, key: Option<&str>, body: Value) -> Resp {
    org.bearer("POST", "/v1/chat/completions", key, Some(body.to_string()))
}

fn chat(org: &Org, cookie: &str, body: Value) -> Resp {
    org.call("POST", "/api/org/v1/chat/completions", Some(cookie), body)
}

fn is_openai_error(r: &Resp, status: u16, kind: &str) {
    assert_eq!(r.status, status, "{}", r.body);
    let v = r.json();
    assert_eq!(v["error"]["type"], kind, "{v}");
    assert!(v["error"]["message"].is_string(), "{v}");
}

fn ask() -> Value {
    json!({"model": "stub/tiny-1", "messages": [{"role": "user", "content": "hi"}]})
}

#[test]
fn case_13_a_personal_key_reaches_models_and_chat() {
    let (org, _stub, a) = ready();
    let (u, uid) = member(&org, &a, "user", "Bo");
    let (id, key) = key_of(&org, &u);
    assert!(id.starts_with("key_"));
    assert!(key.starts_with("fvk_") && key.len() == 68);
    assert!(key[4..].bytes().all(|b| b.is_ascii_hexdigit()));
    let models = org.bearer("GET", "/v1/models", Some(&key), None);
    assert_eq!(models.status, 200, "{}", models.body);
    assert_eq!(
        models.json(),
        json!({"object": "list", "data": [
            {"id": "stub/tiny-1", "object": "model", "owned_by": "stub"},
            {"id": "stub/org/big-2", "object": "model", "owned_by": "stub"},
        ]})
    );
    let mut r = Req::new("POST", "/v1/chat/completions", &org);
    r.headers.push(("Authorization", format!("Bearer {key}")));
    r.headers
        .push(("Origin", "https://elsewhere.example".into()));
    r.body = Some(ask().to_string());
    let r = org.send(r);
    assert_eq!(r.status, 200, "no CSRF header or Origin check: {}", r.body);
    assert_eq!(r.json()["choices"][0]["message"]["content"], "stub reply");
    let row: (String, Option<String>, String) = org
        .db()
        .query_row("SELECT member_id, key_id, model FROM usage", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(row, (uid, Some(id.clone()), "stub/tiny-1".into()));
    let listed = org.get("/api/org/v1/keys", Some(&u));
    assert_eq!(listed.status, 200);
    assert!(!listed.body.contains(&key[4..]));
    let k = &listed.json()[0];
    assert_eq!((&k["id"], &k["name"]), (&json!(id), &json!("laptop")));
    assert!(k["created"].is_string() && k["lastUsed"].is_string(), "{k}");
    assert!(
        org.get("/api/org/v1/audit", Some(&a))
            .json()
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["action"] != "key.create")
    );
    let db = org.db_bytes();
    assert!(holds(&db, &id), "the scan sees the key's row");
    assert!(!holds(&db, &key[4..]), "only the hash is stored");
    let mut org = org;
    assert!(!holds(org.stop().as_bytes(), &key[4..]));
}

#[test]
fn case_14_no_key_a_wrong_key_or_a_revoked_key_is_401() {
    let (org, _stub, a) = ready();
    let (u, _) = member(&org, &a, "user", "Bo");
    let (id, key) = key_of(&org, &u);
    let wrong = format!("fvk_{}", "0".repeat(64));
    for k in [
        None,
        Some(wrong.as_str()),
        Some("fvk_short"),
        Some("sk-other"),
    ] {
        is_openai_error(&v1_chat(&org, k, ask()), 401, "invalid_api_key");
        let r = org.bearer("GET", "/v1/models", k, None);
        is_openai_error(&r, 401, "invalid_api_key");
    }
    let mut r = Req::new("POST", "/v1/chat/completions", &org);
    r.headers.push(("Cookie", u.clone()));
    r.body = Some(ask().to_string());
    is_openai_error(&org.send(r), 401, "invalid_api_key");
    assert_eq!(v1_chat(&org, Some(&key), ask()).status, 200);
    let del = org.call(
        "DELETE",
        &format!("/api/org/v1/keys/{id}"),
        Some(&u),
        json!({}),
    );
    assert_eq!(del.status, 200);
    is_openai_error(&v1_chat(&org, Some(&key), ask()), 401, "invalid_api_key");
    assert_eq!(org.get("/api/org/v1/keys", Some(&u)).json(), json!([]));
    let mut r = Req::new("GET", "/v1/models", &org);
    r.host = Some("evil.example".into());
    r.headers.push(("Authorization", format!("Bearer {key}")));
    is_openai_error(&org.send(r), 421, "misdirected");
    let r = org.bearer("POST", "/v1/embeddings", Some(&key), Some("{}".into()));
    is_openai_error(&r, 404, "not_found");
    is_openai_error(
        &v1_chat(
            &org,
            Some(&key_of(&org, &u).1),
            json!({"model": "stub/none"}),
        ),
        404,
        "unknown_model",
    );
}

#[test]
fn case_15_disabling_the_member_ends_their_keys() {
    let (org, _stub, a) = ready();
    let (u, uid) = member(&org, &a, "user", "Bo");
    let (_, key) = key_of(&org, &u);
    assert_eq!(v1_chat(&org, Some(&key), ask()).status, 200);
    let p = org.call(
        "PATCH",
        &format!("/api/org/v1/members/{uid}"),
        Some(&a),
        json!({"role": "student"}),
    );
    assert_eq!(p.status, 200);
    is_openai_error(&v1_chat(&org, Some(&key), ask()), 403, "students_off");
    assert_eq!(
        org.bearer("GET", "/v1/models", Some(&key), None).json()["data"],
        json!([])
    );
    let p = org.call(
        "PATCH",
        &format!("/api/org/v1/members/{uid}"),
        Some(&a),
        json!({"disabled": true}),
    );
    assert_eq!(p.status, 200);
    is_openai_error(&v1_chat(&org, Some(&key), ask()), 401, "invalid_api_key");
    let left: i64 = org
        .db()
        .query_row(
            "SELECT COUNT(*) FROM keys WHERE member_id = ?1",
            [&uid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0, "disabling deletes the keys");
}

#[test]
fn case_16_a_member_cannot_delete_another_members_key() {
    let (org, _stub, a) = ready();
    let (u, _) = member(&org, &a, "user", "Bo");
    let (v, _) = member(&org, &a, "user", "Cy");
    let (id, key) = key_of(&org, &u);
    let r = org.call(
        "DELETE",
        &format!("/api/org/v1/keys/{id}"),
        Some(&v),
        json!({}),
    );
    assert_eq!((r.status, r.json()), (404, json!({"error": "not_found"})));
    let r = org.call(
        "DELETE",
        &format!("/api/org/v1/keys/{id}"),
        Some(&a),
        json!({}),
    );
    assert_eq!(
        r.status, 404,
        "an admin's own key list doesn't hold it either"
    );
    assert_eq!(org.get("/api/org/v1/keys", Some(&v)).json(), json!([]));
    assert_eq!(v1_chat(&org, Some(&key), ask()).status, 200);
}

#[test]
fn case_17_the_21st_key_is_refused() {
    let (org, _stub, a) = ready();
    let (u, _) = member(&org, &a, "user", "Bo");
    let mut ids = vec![];
    for i in 0..20 {
        let r = make_key(&org, &u, &format!("k{i}"));
        assert_eq!(r.status, 200, "key {i}");
        ids.push(r.json()["id"].as_str().unwrap().to_string());
    }
    let r = make_key(&org, &u, "k20");
    assert_eq!(
        (r.status, r.json()),
        (409, json!({"error": "too_many_keys"}))
    );
    assert_eq!(
        org.get("/api/org/v1/keys", Some(&u))
            .json()
            .as_array()
            .unwrap()
            .len(),
        20
    );
    let del = org.call(
        "DELETE",
        &format!("/api/org/v1/keys/{}", ids[0]),
        Some(&u),
        json!({}),
    );
    assert_eq!(del.status, 200);
    assert_eq!(make_key(&org, &u, "k20").status, 200);
    let (v, _) = member(&org, &a, "user", "Cy");
    assert_eq!(
        make_key(&org, &v, "own").status,
        200,
        "the limit is per member"
    );
    for bad in [
        json!({"name": ""}),
        json!({"name": "  "}),
        json!({"name": "x".repeat(101)}),
        json!({}),
    ] {
        assert_eq!(
            org.call("POST", "/api/org/v1/keys", Some(&v), bad).status,
            400
        );
    }
}

#[test]
fn case_18_the_daily_limit_stops_users_but_not_full_members_or_admins() {
    let (org, stub, a) = ready();
    {
        let mut s = stub.state();
        s.prompt_tokens = 50;
        s.completion_tokens = 30;
    }
    for bad in [
        json!({"dailyTokens": 0}),
        json!({"dailyTokens": 1_000_000_001u64}),
        json!({"dailyTokens": "100"}),
        json!({"dailyTokens": 1.5}),
    ] {
        assert_eq!(
            org.call("PATCH", "/api/org/v1/settings", Some(&a), bad.clone())
                .status,
            400,
            "{bad}"
        );
    }
    let p = org.call(
        "PATCH",
        "/api/org/v1/settings",
        Some(&a),
        json!({"dailyTokens": 100}),
    );
    assert_eq!(
        (p.status, p.json()),
        (200, json!({"studentsAllowed": false, "dailyTokens": 100}))
    );
    let row = &org.get("/api/org/v1/audit", Some(&a)).json()[0];
    assert_eq!(
        (&row["action"], &row["target"]),
        (&json!("settings.update"), &json!("dailyTokens"))
    );
    let (u, _) = member(&org, &a, "user", "Bo");
    let (_, key) = key_of(&org, &u);
    assert_eq!(chat(&org, &u, ask()).status, 200);
    assert_eq!(v1_chat(&org, Some(&key), ask()).status, 200);
    let r = chat(&org, &u, ask());
    assert_eq!((r.status, r.json()), (429, json!({"error": "daily_limit"})));
    is_openai_error(&v1_chat(&org, Some(&key), ask()), 429, "daily_limit");
    let (f, _) = member(&org, &a, "full", "Cy");
    for who in [&f, &a] {
        for _ in 0..3 {
            assert_eq!(chat(&org, who, ask()).status, 200);
        }
    }
    let p = org.call(
        "PATCH",
        "/api/org/v1/settings",
        Some(&a),
        json!({"dailyTokens": null}),
    );
    assert_eq!(p.json()["dailyTokens"], Value::Null);
    assert_eq!(chat(&org, &u, ask()).status, 200);
    assert_eq!(
        org.get("/api/org/v1/settings", Some(&a)).json(),
        json!({"studentsAllowed": false, "dailyTokens": null})
    );
}

#[test]
fn case_19_usage_shows_counts_and_no_text_is_kept() {
    let (org, _stub, a) = ready();
    let (u, uid) = member(&org, &a, "user", "Bo");
    let (v, vid) = member(&org, &a, "user", "Cy");
    let marker = "PROMPT-MARKER-7d3e1b";
    let said = json!({"model": "stub/tiny-1", "messages": [{"role": "user", "content": marker}]});
    for _ in 0..2 {
        assert_eq!(chat(&org, &u, said.clone()).status, 200);
    }
    let (_, key) = key_of(&org, &v);
    assert_eq!(v1_chat(&org, Some(&key), said.clone()).status, 200);
    let mut stream = said.clone();
    stream["stream"] = json!(true);
    assert_eq!(chat(&org, &v, stream).status, 200);
    let today = org
        .db()
        .query_row("SELECT date('now')", [], |r| r.get::<_, String>(0))
        .unwrap();
    let mine = org.get("/api/org/v1/me/usage", Some(&u));
    assert_eq!(mine.status, 200);
    assert_eq!(
        mine.json(),
        json!([{"day": today, "member": uid, "model": "stub/tiny-1", "requests": 2, "promptTokens": 22, "completionTokens": 14}])
    );
    let all = org.get("/api/org/v1/usage?days=1", Some(&a));
    assert_eq!(all.status, 200);
    let mut rows = all.json().as_array().unwrap().clone();
    rows.sort_by_key(|r| r["member"].as_str().unwrap().to_string());
    let mut want = vec![
        json!({"day": today, "member": uid, "model": "stub/tiny-1", "requests": 2, "promptTokens": 22, "completionTokens": 14}),
        json!({"day": today, "member": vid, "model": "stub/tiny-1", "requests": 2, "promptTokens": 22, "completionTokens": 14}),
    ];
    want.sort_by_key(|r| r["member"].as_str().unwrap().to_string());
    assert_eq!(rows, want);
    assert_eq!(org.get("/api/org/v1/usage", Some(&u)).status, 403);
    for q in ["days=0", "days=91", "days=x", "days=-1"] {
        assert_eq!(
            org.get(&format!("/api/org/v1/usage?{q}"), Some(&a)).status,
            400,
            "{q}"
        );
        assert_eq!(
            org.get(&format!("/api/org/v1/me/usage?{q}"), Some(&u))
                .status,
            400,
            "{q}"
        );
    }
    assert_eq!(org.get("/api/org/v1/usage?days=90", Some(&a)).status, 200);
    org.db()
        .execute(
            "UPDATE usage SET at = at - 3 * 86400 WHERE member_id = ?1",
            [&vid],
        )
        .unwrap();
    let recent = org.get("/api/org/v1/usage?days=3", Some(&a)).json();
    assert_eq!(
        recent.as_array().unwrap().len(),
        1,
        "rows older than the window are left out"
    );
    assert_eq!(
        org.get("/api/org/v1/usage?days=4", Some(&a))
            .json()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let db = org.db_bytes();
    assert!(holds(&db, &vid), "the scan sees the usage rows");
    assert!(!holds(&db, marker), "no prompt text in org.db");
    let mut org = org;
    let log = org.stop();
    assert!(log.contains("Fonsviva org server for"), "{log}");
    assert!(!holds(log.as_bytes(), marker));
}

/// The usage rows as (status, key id), waiting up to 2 s for `n` of them.
fn key_rows(org: &Org, n: usize) -> Vec<(i64, Option<String>)> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let db = org.db();
        let mut q = db
            .prepare("SELECT status, key_id FROM usage ORDER BY seq")
            .unwrap();
        let rows: Vec<_> = q
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        if rows.len() >= n || std::time::Instant::now() > until {
            return rows;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn key_chat_req<'a>(org: &'a Org, key: &str, body: Value) -> Req<'a> {
    let mut r = Req::new("POST", "/v1/chat/completions", org);
    r.headers.push(("Authorization", format!("Bearer {key}")));
    r.body = Some(body.to_string());
    r
}

#[test]
fn a_key_chat_whose_client_leaves_early_writes_a_499_row_with_the_key() {
    let (org, stub, a) = ready();
    let (u, _) = member(&org, &a, "user", "Bo");
    let (id, key) = key_of(&org, &u);
    stub.state().stall = true;
    let s = org.open(key_chat_req(&org, &key, ask()));
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while stub.state().seen.is_empty() && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(stub.state().seen.len(), 1);
    drop(s);
    assert_eq!(key_rows(&org, 1), [(499, Some(id))]);
}

#[test]
fn a_key_chat_cut_upstream_writes_a_502_row_with_the_key() {
    let (org, stub, a) = ready();
    let (u, _) = member(&org, &a, "user", "Bo");
    let (id, key) = key_of(&org, &u);
    stub.state().cut = true;
    let mut body = ask();
    body["stream"] = json!(true);
    let mut s = org.open(key_chat_req(&org, &key, body));
    let mut sink = Vec::new();
    let _ = std::io::Read::read_to_end(&mut s, &mut sink);
    assert_eq!(key_rows(&org, 1), [(502, Some(id))]);
}

#[test]
fn a_schema_3_store_gets_the_keys_table_and_keeps_its_usage() {
    let (org, _stub, a) = ready();
    let (u, uid) = member(&org, &a, "user", "Bo");
    assert_eq!(chat(&org, &u, ask()).status, 200);
    org.db()
        .execute_batch("DROP TABLE keys; PRAGMA user_version = 3;")
        .unwrap();
    let out = org_support::fonsviva(org.scratch.path())
        .args(["org", "admin-link", "--data"])
        .arg(&org.data)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let v: i64 = org
        .db()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 4);
    let kept: (String, Option<String>, i64) = org
        .db()
        .query_row("SELECT member_id, key_id, status FROM usage", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(kept, (uid, None, 200));
    let (id, key) = key_of(&org, &u);
    assert_eq!(v1_chat(&org, Some(&key), ask()).status, 200);
    assert_eq!(key_rows(&org, 2)[1], (200, Some(id)));
}
