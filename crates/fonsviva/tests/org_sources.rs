//! Model sources, their sealed keys, the model list and org settings,
//! against the real `fonsviva org serve` and a stub source on 127.0.0.1.

mod org_support;

use org_support::stub::Stub;
use org_support::{
    Org, RESOLVE, SOURCE_KEY, add_source, admin, dechunk, holds, member, ready, start,
    start_resolving,
};
use serde_json::{Value, json};

fn audit(org: &Org, cookie: &str) -> Value {
    org.get("/api/org/v1/audit", Some(cookie)).json()
}

fn source(org: &Org, admin: &str, name: &str) -> Value {
    let list = org.get("/api/org/v1/sources", Some(admin)).json();
    list.as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap()
        .clone()
}

/// A loopback address nothing listens on.
fn closed_base() -> String {
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://127.0.0.1:{}/v1", dead.local_addr().unwrap().port())
}

#[test]
fn case_1_a_local_source_gets_the_local_notice() {
    let org = start();
    let stub = Stub::start();
    let (a, aid) = admin(&org);
    let r = add_source(&org, &a, "stub", &stub.base(), None);
    assert_eq!(r.status, 200, "{}", r.body);
    let v = r.json();
    assert!(v["id"].as_str().unwrap().starts_with("src_"));
    assert_eq!(v["name"], "stub");
    assert_eq!(v["baseUrl"], stub.base());
    assert_eq!(v["local"], true);
    assert_eq!(
        v["notice"],
        "Requests go to 127.0.0.1, an address on your org's own network."
    );
    let row = &audit(&org, &a)[0];
    assert_eq!(
        (&row["admin"], &row["action"], &row["target"]),
        (&json!(aid), &json!("source.add"), &json!("stub"))
    );
}

#[test]
fn case_2_a_cloud_source_gets_the_cloud_notice() {
    let org = start();
    let (a, _) = admin(&org);
    let r = add_source(
        &org,
        &a,
        "cloud",
        "https://api.example.com/v1",
        Some("sk-none"),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["local"], false);
    assert_eq!(
        r.json()["notice"],
        "Members' messages to these models are sent to api.example.com, outside your org's machines. That service's own terms apply to what it keeps."
    );
}

#[test]
fn case_3_the_source_key_is_never_shown_stored_or_logged() {
    let (org, stub, a, u) = ready();
    assert_eq!(
        add_source(&org, &a, "dead", &closed_base(), Some(SOURCE_KEY)).status,
        200
    );
    assert_eq!(org.get("/api/org/v1/models", Some(&u)).status, 200);
    assert_eq!(
        stub.state().model_asks[0]["authorization"],
        format!("Bearer {SOURCE_KEY}"),
        "the stub got the key, so it was sealed and opened again"
    );
    let sources = org.get("/api/org/v1/sources", Some(&a));
    assert_eq!(sources.status, 200);
    let list = sources.json();
    assert_eq!(list.as_array().unwrap().len(), 2);
    for s in list.as_array().unwrap() {
        assert!(s.get("apiKey").is_none());
        for k in ["id", "name", "baseUrl", "local", "created", "lastError"] {
            assert!(s.get(k).is_some(), "{k} missing from {s}");
        }
    }
    assert!(!sources.body.contains(SOURCE_KEY));
    assert!(
        !org.get("/api/org/v1/audit", Some(&a))
            .body
            .contains(SOURCE_KEY)
    );
    let db = org.db_bytes();
    assert!(holds(&db, &stub.base()), "the scan sees the source's row");
    assert!(!holds(&db, SOURCE_KEY), "the key is stored only sealed");
    let mut org = org;
    let log = org.stop();
    assert!(log.contains("Fonsviva org server for"), "{log}");
    assert!(!holds(log.as_bytes(), SOURCE_KEY));
}

#[test]
fn case_4_a_user_cannot_add_a_source() {
    let (org, stub, _, u) = ready();
    let r = add_source(&org, &u, "mine", &stub.base(), None);
    assert_eq!(r.status, 403);
    assert_eq!(r.json(), json!({"error": "forbidden"}));
    assert_eq!(org.get("/api/org/v1/sources", Some(&u)).status, 403);
    let id: String = org
        .db()
        .query_row("SELECT id FROM sources", [], |r| r.get(0))
        .unwrap();
    let r = org.call(
        "DELETE",
        &format!("/api/org/v1/sources/{id}"),
        Some(&u),
        json!({}),
    );
    assert_eq!(r.status, 403);
}

#[test]
fn case_5_bad_sources_are_refused() {
    let (org, stub, a, _) = ready();
    let base = stub.base();
    for (name, url, status, code) in [
        ("Stub", base.clone(), 400, "bad_request"),
        ("", base.clone(), 400, "bad_request"),
        ("a_b", base.clone(), 400, "bad_request"),
        (&"x".repeat(33), base.clone(), 400, "bad_request"),
        ("stub", base.clone(), 409, "name_taken"),
        ("q", format!("{base}?x=1"), 400, "bad_request"),
        ("f", format!("{base}#top"), 400, "bad_request"),
        (
            "u",
            "http://me:pw@127.0.0.1:1/v1".into(),
            400,
            "bad_request",
        ),
        ("ftp", "ftp://127.0.0.1/v1".into(), 400, "bad_request"),
        (
            "plain",
            "http://api.example.com/v1".into(),
            400,
            "bad_request",
        ),
        (
            "tail",
            "http://nobody.tail0000.ts.net/v1".into(),
            400,
            "bad_request",
        ),
    ] {
        let r = add_source(&org, &a, name, &url, None);
        assert_eq!(
            (r.status, r.json()),
            (status, json!({"error": code})),
            "{name} {url}"
        );
    }
    let long = "x".repeat(4097);
    for key in ["", "sk bad", "sk\nx", "sk\u{e9}", long.as_str()] {
        let r = add_source(&org, &a, "keyed", &base, Some(key));
        assert_eq!(
            (r.status, r.json()),
            (400, json!({"error": "bad_request"})),
            "apiKey {key:?}"
        );
    }
    assert_eq!(
        add_source(&org, &a, "keyed", &base, Some(&"x".repeat(4096))).status,
        200
    );
    assert_eq!(
        add_source(&org, &a, &"x".repeat(32), &base, None).status,
        200
    );
    let id = source(&org, &a, "stub")["id"].as_str().unwrap().to_string();
    let del = |id: &str| {
        org.call(
            "DELETE",
            &format!("/api/org/v1/sources/{id}"),
            Some(&a),
            json!({}),
        )
    };
    assert_eq!(del(&id).status, 200);
    assert_eq!(del(&id).status, 404);
    assert_eq!(audit(&org, &a)[0]["action"], "source.remove");
    assert_eq!(audit(&org, &a)[0]["target"], "stub");
    assert_eq!(add_source(&org, &a, "stub", &base, None).status, 200);
}

#[test]
fn case_6_a_user_sees_the_stubs_models() {
    let (org, stub, _, u) = ready();
    let r = org.get("/api/org/v1/models", Some(&u));
    assert_eq!(r.status, 200);
    assert_eq!(
        r.json(),
        json!([
            {"id": "stub/tiny-1", "source": "stub", "model": "tiny-1"},
            {"id": "stub/org/big-2", "source": "stub", "model": "org/big-2"},
        ])
    );
    assert_eq!(stub.state().model_asks.len(), 1);
    assert_eq!(org.get("/api/org/v1/models", Some(&u)).status, 200);
    assert_eq!(stub.state().model_asks.len(), 1, "the list is cached");
}

#[test]
fn case_7_a_source_that_refuses_is_left_out() {
    let (org, _stub, a, u) = ready();
    assert_eq!(
        add_source(&org, &a, "dead", &closed_base(), None).status,
        200
    );
    let ids: Vec<Value> = org
        .get("/api/org/v1/models", Some(&u))
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].clone())
        .collect();
    assert_eq!(ids, [json!("stub/tiny-1"), json!("stub/org/big-2")]);
    assert_eq!(source(&org, &a, "dead")["lastError"], "can't connect");
    assert_eq!(source(&org, &a, "stub")["lastError"], Value::Null);
}

#[test]
fn case_12_students_see_models_only_when_allowed() {
    let (org, _stub, a, _) = ready();
    let (s, _) = member(&org, &a, "student", "Cy");
    assert_eq!(org.get("/api/org/v1/models", Some(&s)).json(), json!([]));
    assert_eq!(
        org.get("/api/org/v1/settings", Some(&a)).json(),
        json!({"studentsAllowed": false})
    );
    let p = org.call(
        "PATCH",
        "/api/org/v1/settings",
        Some(&a),
        json!({"studentsAllowed": true}),
    );
    assert_eq!(
        (p.status, p.json()),
        (200, json!({"studentsAllowed": true}))
    );
    let listed = org.get("/api/org/v1/models", Some(&s)).json();
    assert_eq!(listed.as_array().unwrap().len(), 2);
    let row = &audit(&org, &a)[0];
    assert_eq!(
        (&row["action"], &row["target"]),
        (&json!("settings.update"), &json!("studentsAllowed"))
    );
    let n = audit(&org, &a).as_array().unwrap().len();
    let again = org.call(
        "PATCH",
        "/api/org/v1/settings",
        Some(&a),
        json!({"studentsAllowed": true}),
    );
    assert_eq!(again.status, 200);
    assert_eq!(
        audit(&org, &a).as_array().unwrap().len(),
        n,
        "no change, no audit row"
    );
    for bad in [json!({}), json!({"studentsAllowed": "yes"})] {
        assert_eq!(
            org.call("PATCH", "/api/org/v1/settings", Some(&a), bad)
                .status,
            400
        );
    }
    let by_student = org.call(
        "PATCH",
        "/api/org/v1/settings",
        Some(&s),
        json!({"studentsAllowed": false}),
    );
    assert_eq!(by_student.status, 403);
}

#[test]
fn a_missing_secrets_key_fails_the_source_and_sends_nothing() {
    let (org, stub, a, u) = ready();
    std::fs::remove_file(org.data.join("secrets.key")).unwrap();
    assert_eq!(org.get("/api/org/v1/models", Some(&u)).json(), json!([]));
    assert_eq!(source(&org, &a, "stub")["lastError"], "cannot read its key");
    assert!(
        stub.state().model_asks.is_empty(),
        "no request without the key"
    );
}

#[test]
fn a_sealed_key_moved_to_another_source_fails_to_open() {
    let org = start();
    let (one, two) = (Stub::start(), Stub::start());
    let (a, _) = admin(&org);
    assert_eq!(
        add_source(&org, &a, "one", &one.base(), Some("sk-one")).status,
        200
    );
    assert_eq!(
        add_source(&org, &a, "two", &two.base(), Some("sk-two")).status,
        200
    );
    org.db()
        .execute(
            "UPDATE sources SET api_key = (SELECT api_key FROM sources WHERE name = 'one') WHERE name = 'two'",
            [],
        )
        .unwrap();
    let ids: Vec<Value> = org
        .get("/api/org/v1/models", Some(&a))
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].clone())
        .collect();
    assert_eq!(ids, [json!("one/tiny-1"), json!("one/org/big-2")]);
    assert_eq!(source(&org, &a, "two")["lastError"], "cannot read its key");
    assert!(two.state().model_asks.is_empty());
    assert_eq!(one.state().model_asks[0]["authorization"], "Bearer sk-one");
}

#[test]
fn a_schema_1_store_is_moved_to_the_new_schema() {
    let org = start();
    let (a, _) = admin(&org);
    org.db()
        .execute_batch("DROP TABLE usage; DROP TABLE sources; PRAGMA user_version = 1;")
        .unwrap();
    std::fs::remove_file(org.data.join("secrets.key")).unwrap();
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
    assert_eq!(v, 3);
    assert_eq!(org.get("/api/org/v1/sources", Some(&a)).json(), json!([]));
    let r = add_source(&org, &a, "late", "http://127.0.0.1:9/v1", Some("sk-late"));
    assert_eq!(
        r.status, 200,
        "an older data dir gets its secrets.key on first use"
    );
    assert_eq!(
        std::fs::read(org.data.join("secrets.key")).unwrap().len(),
        32
    );
}

#[test]
fn a_name_is_local_only_when_every_address_is_private() {
    let org = start();
    let (a, _) = admin(&org);
    for (name, url, local) in [
        ("loop", "http://loop.test:8080/v1", true),
        ("lan", "http://gpu.lan.test/v1", true),
        ("named", "http://localhost:9/v1", true),
        ("mixed", "https://mixed.test/v1", false),
        ("tail", "https://nobody.tail0000.ts.net/v1", false),
        ("printer", "https://printer.local/v1", false),
    ] {
        let r = add_source(&org, &a, name, url, None);
        assert_eq!(r.status, 200, "{url} {}", r.body);
        assert_eq!(r.json()["local"], local, "{url}");
        let host = url::Url::parse(url)
            .unwrap()
            .host_str()
            .unwrap()
            .to_string();
        let notice = r.json()["notice"].as_str().unwrap().to_string();
        assert_eq!(
            notice == format!("Requests go to {host}, an address on your org's own network."),
            local,
            "{notice}"
        );
    }
    for url in ["http://mixed.test/v1", "http://printer.local/v1"] {
        assert_eq!(
            add_source(&org, &a, "plain", url, None).status,
            400,
            "{url}"
        );
    }
}

#[test]
fn a_keyed_source_is_refused_while_sealed_keys_lost_their_key() {
    let (org, stub, a, _) = ready();
    let file = org.data.join("secrets.key");
    std::fs::remove_file(&file).unwrap();
    let r = add_source(&org, &a, "more", &stub.base(), Some("sk-more"));
    assert_eq!(
        (r.status, r.json()),
        (409, json!({"error": "secrets_key_missing"}))
    );
    assert!(!file.exists(), "no new key over the sealed ones");
    assert_eq!(add_source(&org, &a, "open", &stub.base(), None).status, 200);
    assert!(!file.exists());
}

#[test]
fn the_sources_list_reports_a_lost_key_before_any_model_list() {
    let (org, stub, a, _) = ready();
    std::fs::remove_file(org.data.join("secrets.key")).unwrap();
    assert_eq!(source(&org, &a, "stub")["lastError"], "cannot read its key");
    assert!(stub.state().model_asks.is_empty());
}

#[test]
fn a_restored_key_clears_the_error_at_once() {
    let (org, _stub, a, u) = ready();
    let file = org.data.join("secrets.key");
    let key = std::fs::read(&file).unwrap();
    std::fs::remove_file(&file).unwrap();
    assert_eq!(org.get("/api/org/v1/models", Some(&u)).json(), json!([]));
    assert_eq!(source(&org, &a, "stub")["lastError"], "cannot read its key");
    std::fs::write(&file, &key).unwrap();
    assert_eq!(source(&org, &a, "stub")["lastError"], Value::Null);
    let n = org.get("/api/org/v1/models", Some(&u)).json();
    assert_eq!(n.as_array().unwrap().len(), 2, "{n}");
}

#[test]
fn the_test_client_joins_chunks_as_bytes() {
    let body = b"3\r\n\xc3\xa9\xc3\r\n1\r\n\xa9\r\n0\r\n\r\n";
    assert_eq!(String::from_utf8(dechunk(body)).unwrap(), "\u{e9}\u{e9}");
}

#[test]
fn a_local_name_that_leaves_the_network_is_refused() {
    let stub = Stub::start();
    let mut org = start_resolving(&format!("{RESOLVE};flip.test=10.0.0.1"));
    let (a, _) = admin(&org);
    let base = format!("http://flip.test:{}/v1", stub.port);
    let r = add_source(&org, &a, "flip", &base, Some("sk-flip"));
    assert_eq!((r.status, r.json()["local"].clone()), (200, json!(true)));
    let ids = |org: &Org| org.get("/api/org/v1/models", Some(&a)).json();

    org.restart(&format!("{RESOLVE};flip.test=127.0.0.1"));
    assert_eq!(
        ids(&org).as_array().unwrap().len(),
        2,
        "on the network it works"
    );
    assert_eq!(stub.state().model_asks.len(), 1);
    assert_eq!(source(&org, &a, "flip")["lastError"], Value::Null);

    for map in ["127.0.0.1,203.0.113.7", "203.0.113.7"] {
        org.restart(&format!("{RESOLVE};flip.test={map}"));
        assert_eq!(ids(&org), json!([]), "{map}");
        assert_eq!(stub.state().model_asks.len(), 1, "{map}: nothing sent");
        assert_eq!(
            source(&org, &a, "flip")["lastError"],
            "left the org's network",
            "{map}"
        );
    }
}

#[test]
fn thirty_members_at_once_cause_one_model_request() {
    let (org, stub, a, u) = ready();
    stub.state().models_delay = std::time::Duration::from_millis(300);
    let lists: Vec<Value> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..30)
            .map(|i| {
                let (org, who) = (&org, if i % 2 == 0 { &a } else { &u });
                sc.spawn(move || org.get("/api/org/v1/models", Some(who)).json())
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(
        lists.iter().all(|l| l.as_array().unwrap().len() == 2),
        "{lists:?}"
    );
    assert_eq!(stub.state().model_asks.len(), 1);
}

#[test]
fn a_malformed_secrets_key_is_refused_with_its_reason() {
    let (org, stub, a, _) = ready();
    let file = org.data.join("secrets.key");
    std::fs::write(&file, b"short").unwrap();
    let r = add_source(&org, &a, "more", &stub.base(), Some("sk-more"));
    assert_eq!(
        (r.status, r.json()),
        (409, json!({"error": "secrets_key_missing"}))
    );

    let org = start();
    let (a, _) = admin(&org);
    let file = org.data.join("secrets.key");
    std::fs::write(&file, b"short").unwrap();
    let r = add_source(&org, &a, "first", &stub.base(), Some("sk-first"));
    assert_eq!(
        (r.status, r.json()),
        (409, json!({"error": "secrets_key_bad"}))
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"short");
    assert_eq!(add_source(&org, &a, "open", &stub.base(), None).status, 200);
}
