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

/// An OpenAI-shaped error: our code in `code`, OpenAI's category in `type`.
fn is_openai_error(r: &Resp, status: u16, code: &str) {
    assert_eq!(r.status, status, "{}", r.body);
    let v = r.json();
    let e = &v["error"];
    assert_eq!(e["code"], code, "{v}");
    assert!(e["message"].is_string(), "{v}");
    assert_eq!(e["param"], Value::Null, "{v}");
    assert!(e.as_object().unwrap().contains_key("param"), "{v}");
    let kind = match code {
        "daily_limit" => "insufficient_quota",
        "too_many_in_flight" => "rate_limit_error",
        "upstream" | "upstream_auth" | "internal" => "server_error",
        _ => "invalid_request_error",
    };
    assert_eq!(e["type"], kind, "{v}");
}

/// The seconds a daily limit answer says to wait: until the next UTC day.
fn waits_for_midnight(r: &Resp) {
    let secs: u64 = r
        .header("retry-after")
        .expect("Retry-After")
        .parse()
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let left = 86_400 - now % 86_400;
    assert!(
        secs.abs_diff(left) <= 5 && (1..=86_400).contains(&secs),
        "{secs} {left}"
    );
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
        (
            200,
            json!({"studentsAllowed": false, "dailyTokens": 100, "keepThreads": true, "adminsReadThreads": false, "electricityPerKwh": "0.15"})
        )
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
    waits_for_midnight(&r);
    let r = v1_chat(&org, Some(&key), ask());
    is_openai_error(&r, 429, "daily_limit");
    waits_for_midnight(&r);
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
        json!({"studentsAllowed": false, "dailyTokens": null, "keepThreads": true, "adminsReadThreads": false, "electricityPerKwh": "0.15"})
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
        json!([{"day": today, "member": uid, "model": "stub/tiny-1", "requests": 2, "promptTokens": 22, "completionTokens": 14, "estimated": false, "costNanos": "0", "costKnown": false}])
    );
    let all = org.get("/api/org/v1/usage?days=1", Some(&a));
    assert_eq!(all.status, 200);
    let mut rows = all.json().as_array().unwrap().clone();
    rows.sort_by_key(|r| r["member"].as_str().unwrap().to_string());
    let mut want = vec![
        json!({"day": today, "member": uid, "model": "stub/tiny-1", "requests": 2, "promptTokens": 22, "completionTokens": 14, "estimated": false, "costNanos": "0", "costKnown": false}),
        json!({"day": today, "member": vid, "model": "stub/tiny-1", "requests": 2, "promptTokens": 22, "completionTokens": 14, "estimated": false, "costNanos": "0", "costKnown": false}),
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
        .execute_batch(
            "DROP TABLE limits; ALTER TABLE usage DROP COLUMN cost_nanos; ALTER TABLE usage DROP COLUMN cost_known; ALTER TABLE sources DROP COLUMN pricing; DROP TABLE messages_fts; DROP TABLE messages; DROP TABLE keys; ALTER TABLE usage DROP COLUMN estimated; PRAGMA user_version = 3;",
        )
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
    assert_eq!(v, 8);
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

fn limit(org: &Org, admin: &str, tokens: Value) {
    let p = org.call(
        "PATCH",
        "/api/org/v1/settings",
        Some(admin),
        json!({ "dailyTokens": tokens }),
    );
    assert_eq!(p.status, 200, "{}", p.body);
}

/// Sends `n` key chats at once and returns their answers.
fn at_once(org: &Org, key: &str, n: usize) -> Vec<Resp> {
    std::thread::scope(|sc| {
        let hs: Vec<_> = (0..n)
            .map(|_| sc.spawn(|| v1_chat(org, Some(key), ask())))
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

#[test]
fn a_limited_member_has_at_most_four_chats_in_flight() {
    let (org, stub, a) = ready();
    stub.state().chat_delay = std::time::Duration::from_secs(1);
    limit(&org, &a, json!(1_000_000_000));
    let (u, _) = member(&org, &a, "user", "Bo");
    let (_, key) = key_of(&org, &u);
    let answers = at_once(&org, &key, 20);
    let ok = answers.iter().filter(|r| r.status == 200).count();
    assert!((1..=4).contains(&ok), "{ok} admitted");
    for r in answers.iter().filter(|r| r.status != 200) {
        is_openai_error(r, 429, "too_many_in_flight");
        assert_eq!(r.header("retry-after"), Some("1"));
    }

    let (f, _) = member(&org, &a, "full", "Cy");
    for who in [&f, &a] {
        let (_, k) = key_of(&org, who);
        assert!(at_once(&org, &k, 20).iter().all(|r| r.status == 200));
    }

    limit(&org, &a, json!(1_000_000_000));
    stub.state().chat_delay = std::time::Duration::ZERO;
    stub.state().fail = Some((500, "{}".into()));
    for _ in 0..6 {
        assert_eq!(v1_chat(&org, Some(&key), ask()).status, 500);
    }
    stub.state().fail = None;
    let unknown = json!({"model": "stub/none", "messages": []});
    for _ in 0..6 {
        is_openai_error(
            &v1_chat(&org, Some(&key), unknown.clone()),
            404,
            "unknown_model",
        );
    }
    assert_eq!(
        v1_chat(&org, Some(&key), ask()).status,
        200,
        "every path gave its place back"
    );
}

#[test]
fn the_bearer_scheme_is_read_without_case() {
    let (org, _stub, a) = ready();
    let (_, key) = key_of(&org, &a);
    for auth in [
        format!("bearer {key}"),
        format!("BEARER  {key}"),
        format!("Bearer\t{key}"),
    ] {
        let mut r = Req::new("GET", "/v1/models", &org);
        r.headers.push(("Authorization", auth.clone()));
        assert_eq!(org.send(r).status, 200, "{auth:?}");
    }
    let mut r = Req::new("GET", "/v1/models", &org);
    r.headers.push(("Authorization", format!("Basic {key}")));
    is_openai_error(&org.send(r), 401, "invalid_api_key");
}

#[test]
fn a_key_opens_no_cookie_route() {
    let (org, _stub, a) = ready();
    let (u, _) = member(&org, &a, "user", "Bo");
    let (_, user_key) = key_of(&org, &u);
    let (_, admin_key) = key_of(&org, &a);
    for (method, path, key) in [
        ("POST", "/api/org/v1/keys", &user_key),
        ("GET", "/api/org/v1/keys", &user_key),
        ("GET", "/api/org/v1/me/usage", &user_key),
        ("GET", "/api/org/v1/members", &admin_key),
    ] {
        let mut r = Req::new(method, path, &org);
        r.headers.push(("Authorization", format!("Bearer {key}")));
        r.headers.push(("X-Fonsviva-Request", "1".into()));
        if method == "POST" {
            r.body = Some(json!({"name": "more"}).to_string());
        }
        let r = org.send(r);
        assert_eq!(
            (r.status, r.json()),
            (401, json!({"error": "unauthorized"})),
            "{method} {path}"
        );
    }
}

/// Sends `n` key chats asking for `max_tokens` at once.
fn at_once_asking(org: &Org, key: &str, n: usize, max_tokens: u64) -> Vec<Resp> {
    let mut body = ask();
    body["max_tokens"] = json!(max_tokens);
    std::thread::scope(|sc| {
        let hs: Vec<_> = (0..n)
            .map(|_| {
                let body = body.clone();
                sc.spawn(move || v1_chat(org, Some(key), body))
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

/// (status, prompt, completion, estimated) of usage rows.
type Rows = Vec<(i64, Option<i64>, Option<i64>, bool)>;

/// (status, prompt, completion, estimated) of each usage row.
fn counted(org: &Org) -> Rows {
    let db = org.db();
    let mut q = db
        .prepare(
            "SELECT status, prompt_tokens, completion_tokens, estimated FROM usage ORDER BY seq",
        )
        .unwrap();
    q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn tokens(text: &str) -> i64 {
    text.len().div_ceil(4) as i64
}

#[test]
fn a_stream_without_usage_is_charged_its_estimate() {
    let (org, stub, a) = ready();
    {
        let mut s = stub.state();
        s.no_usage = true;
        s.reply = Some("x".repeat(400));
    }
    limit(&org, &a, json!(100));
    let (u, _) = member(&org, &a, "user", "Bo");
    let mut body = ask();
    body["stream"] = json!(true);
    let r = chat(&org, &u, body.clone());
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(stub.state().seen[0].body.get("max_tokens").is_none());
    let rows = counted(&org);
    assert_eq!(
        rows,
        [(200, Some(tokens("hi")), Some(101), true)],
        "400 + 3 bytes of streamed content"
    );
    let r = chat(&org, &u, ask());
    assert_eq!((r.status, r.json()), (429, json!({"error": "daily_limit"})));
    let mine = org.get("/api/org/v1/me/usage", Some(&u)).json();
    assert_eq!(mine[0]["estimated"], true, "{mine}");
}

#[test]
fn an_answer_without_usage_is_charged_its_estimate() {
    let (org, stub, a) = ready();
    {
        let mut s = stub.state();
        s.no_usage = true;
        s.reply = Some("x".repeat(400));
    }
    limit(&org, &a, json!(100));
    let (u, _) = member(&org, &a, "user", "Bo");
    assert_eq!(chat(&org, &u, ask()).status, 200);
    assert_eq!(counted(&org), [(200, Some(tokens("hi")), Some(100), true)]);
    let r = chat(&org, &u, ask());
    assert_eq!((r.status, r.json()), (429, json!({"error": "daily_limit"})));
    let mine = org.get("/api/org/v1/me/usage", Some(&u)).json();
    assert_eq!(mine[0]["estimated"], true, "{mine}");
}

#[test]
fn a_stream_left_before_its_usage_is_499_with_an_estimate() {
    let (org, stub, a) = ready();
    {
        let mut s = stub.state();
        s.hold = true;
        s.reply = Some("x".repeat(400));
    }
    let (u, _) = member(&org, &a, "user", "Bo");
    let rows = left_after(&org, &u, "two");
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (status, _, completion, estimated) = rows[0];
    assert_eq!((status, estimated), (499, true));
    assert!(completion.is_some_and(|c| c >= 100), "{rows:?}");
}

#[test]
fn held_charges_keep_requests_at_once_within_the_limit() {
    let (org, stub, a) = ready();
    stub.state().prompt_tokens = 990;
    stub.state().completion_tokens = 0;
    limit(&org, &a, json!(1000));
    let (u, _) = member(&org, &a, "user", "Bo");
    let (_, key) = key_of(&org, &u);
    let mut small = ask();
    small["max_tokens"] = json!(10);
    assert_eq!(v1_chat(&org, Some(&key), small).status, 200);
    {
        let mut s = stub.state();
        s.prompt_tokens = 1;
        s.completion_tokens = 1;
        s.chat_delay = std::time::Duration::from_secs(2);
    }
    let answers = at_once_asking(&org, &key, 4, 500);
    let ok = answers.iter().filter(|r| r.status == 200).count();
    assert_eq!(ok, 1, "990 used: one more holds the last 10");
    for r in answers.iter().filter(|r| r.status != 200) {
        is_openai_error(r, 429, "too_many_in_flight");
        assert_eq!(r.header("retry-after"), Some("1"));
    }

    let (v, _) = member(&org, &a, "user", "Cy");
    let (_, key) = key_of(&org, &v);
    let answers = at_once_asking(&org, &key, 4, 400);
    let ok = answers.iter().filter(|r| r.status == 200).count();
    assert_eq!(ok, 3, "401, 401 and the last 198 of 1000");
    for r in answers.iter().filter(|r| r.status != 200) {
        is_openai_error(r, 429, "too_many_in_flight");
    }
    let seen = &stub.state().seen;
    assert!(
        seen[1..]
            .iter()
            .all(|s| [500, 400].contains(&s.body["max_tokens"].as_u64().unwrap())),
        "max_tokens is sent as asked"
    );
}

#[test]
fn a_schema_4_store_gets_the_estimated_column() {
    let (org, _stub, a) = ready();
    let (u, _) = member(&org, &a, "user", "Bo");
    assert_eq!(chat(&org, &u, ask()).status, 200);
    org.db()
        .execute_batch("DROP TABLE limits; ALTER TABLE usage DROP COLUMN cost_nanos; ALTER TABLE usage DROP COLUMN cost_known; ALTER TABLE sources DROP COLUMN pricing; DROP TABLE messages_fts; DROP TABLE messages; ALTER TABLE usage DROP COLUMN estimated; PRAGMA user_version = 4;")
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
    assert_eq!(v, 8);
    assert_eq!(counted(&org), [(200, Some(11), Some(7), false)]);
}

#[test]
fn a_held_charge_alone_never_reads_as_the_day_spent() {
    let (org, stub, a) = ready();
    limit(&org, &a, json!(100_000));
    let (u, _) = member(&org, &a, "user", "Bo");
    let (_, key) = key_of(&org, &u);
    stub.state().chat_delay = std::time::Duration::from_secs(2);
    let mut big = ask();
    big["max_tokens"] = json!(128_000);
    std::thread::scope(|sc| {
        let first = sc.spawn(|| v1_chat(&org, Some(&key), big));
        let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while stub.state().seen.is_empty() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let r = v1_chat(&org, Some(&key), ask());
        is_openai_error(&r, 429, "too_many_in_flight");
        assert_eq!(r.header("retry-after"), Some("1"));
        assert_eq!(first.join().unwrap().status, 200);
    });
}

/// The one usage row of a stream read to its end with the stub as `set`
/// leaves it.
fn streamed(set: impl FnOnce(&mut org_support::stub::StubState)) -> (Value, Rows) {
    let (org, stub, a) = ready();
    set(&mut stub.state());
    let (u, _) = member(&org, &a, "user", "Bo");
    let mut body = ask();
    body["stream"] = json!(true);
    chat(&org, &u, body.clone());
    (body, counted_soon(&org))
}

#[test]
fn a_stream_of_tool_calls_or_reasoning_is_charged_its_estimate() {
    let x = "x".repeat(400);
    for delta in [
        json!({"role": "assistant", "tool_calls": [{"index": 0, "id": "", "type": "", "function": {"name": "", "arguments": x}}]}),
        json!({"role": "assistant", "reasoning_content": x}),
    ] {
        let (_, rows) = streamed(|s| {
            s.no_usage = true;
            s.delta = Some(delta.clone());
        });
        assert_eq!(
            rows,
            [(200, Some(tokens("hi")), Some(101), true)],
            "{delta}"
        );
    }
}

#[test]
fn a_stream_line_over_the_line_limit_counts_toward_its_estimate() {
    let (_, rows) = streamed(|s| {
        s.no_usage = true;
        s.reply = Some("x".repeat(1_200_000));
    });
    assert!(rows[0].2.is_some_and(|c| c >= 300_000), "{rows:?}");
}

#[test]
fn a_count_the_usage_leaves_out_is_estimated_alone() {
    let (_, rows) = streamed(|s| {
        s.partial_usage = true;
        s.reply = Some("x".repeat(400));
    });
    assert_eq!(rows, [(200, Some(11), Some(101), true)]);
}

#[test]
fn a_stream_left_after_its_done_keeps_its_status() {
    let (org, stub, a) = ready();
    stub.state().linger = true;
    let (u, _) = member(&org, &a, "user", "Bo");
    let rows = left_after(&org, &u, "[DONE]");
    stub.state().released = true;
    assert_eq!(rows, [(200, Some(11), Some(7), false)]);
}

/// Streams a chat, leaves once `needle` arrives, and returns the rows.
fn left_after(org: &Org, cookie: &str, needle: &str) -> Rows {
    let mut r = Req::new("POST", "/api/org/v1/chat/completions", org);
    r.headers.push(("X-Fonsviva-Request", "1".into()));
    r.headers.push(("Cookie", cookie.into()));
    let mut body = ask();
    body["stream"] = json!(true);
    r.body = Some(body.to_string());
    let mut s = org.open(r);
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while !holds(&got, needle) {
        let n = std::io::Read::read(&mut s, &mut buf).unwrap();
        assert!(n > 0, "the stream ended before {needle}");
        got.extend_from_slice(&buf[..n]);
    }
    drop(s);
    counted_soon(org)
}

/// The usage rows, once there is one or 3 s have passed.
fn counted_soon(org: &Org) -> Rows {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while counted(org).is_empty() && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    counted(org)
}

#[test]
fn an_error_page_from_the_source_is_not_charged() {
    let (org, stub, a) = ready();
    stub.state().fail = Some((503, format!("<html>{}</html>", "x".repeat(4000))));
    let (u, _) = member(&org, &a, "user", "Bo");
    assert_eq!(chat(&org, &u, ask()).status, 503);
    assert_eq!(counted_soon(&org), [(503, None, None, false)]);
}

#[test]
fn an_image_is_charged_a_fixed_amount_not_its_bytes() {
    let (org, stub, a) = ready();
    stub.state().no_usage = true;
    limit(&org, &a, json!(100_000));
    let (u, _) = member(&org, &a, "user", "Bo");
    let png = format!("data:image/png;base64,{}", "A".repeat(1_400_000));
    let mut body = ask();
    body["messages"] = json!([{"role": "user", "content": [
        {"type": "text", "text": "what is this"},
        {"type": "image_url", "image_url": {"url": png}},
    ]}]);
    for _ in 0..2 {
        assert_eq!(chat(&org, &u, body.clone()).status, 200);
    }
    let rows = counted(&org);
    let text = json!({"type": "text", "text": "what is this"}).to_string();
    assert_eq!(rows[0].1, Some(tokens(&text) + 85), "{rows:?}");
}

#[test]
fn a_stream_with_usage_on_each_chunk_left_midway_is_499() {
    let (org, stub, a) = ready();
    {
        let mut s = stub.state();
        s.usage_each = true;
        s.hold = true;
    }
    let (u, _) = member(&org, &a, "user", "Bo");
    let rows = left_after(&org, &u, "two");
    stub.state().released = true;
    assert_eq!(rows[0].0, 499, "{rows:?}");
}

#[test]
fn four_large_asks_at_once_let_one_in() {
    let (org, stub, a) = ready();
    limit(&org, &a, json!(100_000));
    let (u, _) = member(&org, &a, "user", "Bo");
    let (_, key) = key_of(&org, &u);
    stub.state().chat_delay = std::time::Duration::from_secs(2);
    let answers = at_once_asking(&org, &key, 4, 128_000);
    let ok = answers.iter().filter(|r| r.status == 200).count();
    assert_eq!(ok, 1, "the first holds all of the limit");
    for r in answers.iter().filter(|r| r.status != 200) {
        is_openai_error(r, 429, "too_many_in_flight");
        assert_eq!(r.header("retry-after"), Some("1"));
    }
    let seen = &stub.state().seen;
    assert!(seen.iter().all(|s| s.body["max_tokens"] == 128_000));
}

#[test]
fn tools_and_files_are_charged_with_no_usage() {
    let (org, stub, a) = ready();
    stub.state().no_usage = true;
    let (u, _) = member(&org, &a, "user", "Bo");
    let tools = json!([{"type": "function", "function": {"name": "f", "parameters": {"description": "x".repeat(40_000)}}}]);
    let mut body = ask();
    body["tools"] = tools.clone();
    assert_eq!(chat(&org, &u, body).status, 200);
    let file = json!({"type": "file", "file": {"file_data": "A".repeat(1_600_000)}});
    let mut body = ask();
    body["messages"][0]["content"] = json!([file]);
    assert_eq!(chat(&org, &u, body).status, 200);
    let rows = counted(&org);
    assert_eq!(rows[0].1, Some(tokens(&format!("hi{tools}"))), "{rows:?}");
    assert_eq!(
        rows[1].1,
        Some(file.to_string().len() as i64 / 16),
        "{rows:?}"
    );
}

#[test]
fn a_stream_cut_after_its_role_chunk_is_502_with_its_prompt() {
    let (_, rows) = streamed(|s| {
        s.no_usage = true;
        s.cut = true;
        s.delta = Some(json!({"role": "assistant"}));
    });
    assert_eq!(rows, [(502, Some(tokens("hi")), Some(0), true)]);
}
