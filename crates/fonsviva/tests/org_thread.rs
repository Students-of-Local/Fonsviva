//! Slice 3, first half: each member's thread on the org server, read,
//! searched and deleted over real HTTP. Messages are seeded straight into
//! `messages` on a connection set up as the store's own, as the send route
//! will store them, so its triggers keep the search index as the server
//! does.

mod org_support;

use org_support::{Org, RESOLVE, Resp, admin, holds, member, start};
use serde_json::{Value, json};

/// Stores a message in the member's thread and returns its seq.
fn seed(org: &Org, member: &str, role: &str, text: &str) -> i64 {
    let db = org.db();
    db.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    db.pragma_update(None, "secure_delete", true).unwrap();
    let status = (role == "assistant").then_some(200);
    db.execute(
        "INSERT INTO messages (member_id, role, text, model, at, status)
         VALUES (?1, ?2, ?3, 'stub/m', strftime('%s', 'now'), ?4)",
        rusqlite::params![member, role, text, status],
    )
    .unwrap();
    db.last_insert_rowid()
}

/// `s` as a query value.
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn search(org: &Org, cookie: &str, q: &str) -> Resp {
    org.get(
        &format!("/api/org/v1/thread/search?q={}", enc(q)),
        Some(cookie),
    )
}

/// The seqs a search finds, after checking it answered 200.
fn found(org: &Org, cookie: &str, q: &str) -> Vec<i64> {
    let r = search(org, cookie, q);
    assert_eq!(r.status, 200, "{q:?}: {}", r.body);
    seqs(&r.json()["results"])
}

fn seqs(rows: &Value) -> Vec<i64> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|m| m["seq"].as_i64().unwrap())
        .collect()
}

fn thread(org: &Org, cookie: &str) -> Value {
    let r = org.get("/api/org/v1/thread", Some(cookie));
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()
}

fn delete(org: &Org, cookie: &str, path: &str) -> u16 {
    org.call("DELETE", path, Some(cookie), json!({})).status
}

fn audit(org: &Org, admin: &str) -> Value {
    org.get("/api/org/v1/audit", Some(admin)).json()
}

#[test]
fn case_10_a_search_finds_the_one_message_with_its_word() {
    let org = start();
    let (a, aid) = admin(&org);
    seed(&org, &aid, "user", "the quick brown fox");
    let hit = seed(&org, &aid, "assistant", "see [note]: a pelican glides by");
    seed(&org, &aid, "user", "lazy dog sleeps");
    let r = search(&org, &a, "Pelican");
    assert_eq!(r.status, 200, "{}", r.body);
    let rows = r.json()["results"].clone();
    assert_eq!(seqs(&rows), [hit]);
    assert_eq!(rows[0]["role"], "assistant");
    let snippet = rows[0]["snippet"].as_str().unwrap();
    assert_eq!(snippet, "see [note]: a \u{E000}pelican\u{E001} glides by");
    assert!(rows[0]["at"].as_str().is_some());
}

#[test]
fn case_11_fts_syntax_in_a_search_is_only_text() {
    let org = start();
    let (a, aid) = admin(&org);
    let quote = seed(&org, &aid, "user", "she said \"hi\" there");
    let near = seed(&org, &aid, "user", "NEAR(a b) is query syntax");
    let column = seed(&org, &aid, "user", "text: plain words");
    seed(&org, &aid, "user", "a glob * star");
    seed(&org, &aid, "user", "hidden treasure");
    for (q, want) in [
        ("\"", vec![]),
        ("\"hi", vec![quote]),
        ("hi*", vec![quote]),
        ("*", vec![]),
        ("NEAR(", vec![near]),
        ("NEAR(a b)", vec![near]),
        ("text:", vec![column]),
        ("text:plain", vec![column]),
        ("\"plain text:", vec![column]),
    ] {
        assert_eq!(found(&org, &a, q), want, "{q:?}");
    }
    for bad in [
        "".to_string(),
        "x".repeat(201),
        " ".into(),
        "\u{3000}\t".into(),
    ] {
        assert_eq!(search(&org, &a, &bad).status, 400, "{} chars", bad.len());
    }
    assert_eq!(found(&org, &a, &"x".repeat(200)), Vec::<i64>::new());
}

#[test]
fn case_12_a_member_never_sees_another_members_thread() {
    let org = start();
    let (a, aid) = admin(&org);
    let (b, bid) = member(&org, &a, "user", "Bo");
    let theirs = seed(&org, &aid, "user", "the zebra crossing");
    let own = seed(&org, &bid, "user", "a bee in the garden");
    assert_eq!(found(&org, &a, "zebra"), [theirs]);
    assert_eq!(found(&org, &b, "zebra"), Vec::<i64>::new());
    assert_eq!(found(&org, &b, "the"), [own]);
    let t = thread(&org, &b);
    assert_eq!(seqs(&t["messages"]), [own]);
    assert_eq!((&t["stored"], &t["more"]), (&json!(true), &json!(false)));
}

#[test]
fn case_13_a_deleted_message_leaves_search_and_another_members_is_404() {
    let org = start();
    let (a, aid) = admin(&org);
    let (b, bid) = member(&org, &a, "user", "Bo");
    let gone = seed(&org, &aid, "user", "walrus one");
    let kept = seed(&org, &aid, "assistant", "walrus two");
    let other = seed(&org, &bid, "user", "walrus of Bo");
    let path = |s: i64| format!("/api/org/v1/thread/messages/{s}");
    assert_eq!(delete(&org, &a, &path(gone)), 204);
    assert_eq!(found(&org, &a, "walrus"), [kept]);
    assert_eq!(found(&org, &a, "one"), Vec::<i64>::new());
    assert_eq!(seqs(&thread(&org, &a)["messages"]), [kept]);
    assert_eq!(delete(&org, &a, &path(gone)), 404);
    assert_eq!(delete(&org, &a, &path(other)), 404);
    assert_eq!(delete(&org, &a, "/api/org/v1/thread/messages/x"), 404);
    assert_eq!(found(&org, &b, "walrus"), [other]);
    assert!(
        !audit(&org, &a)
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["action"].as_str().unwrap().starts_with("thread")),
        "a member's own delete is not audited"
    );
}

#[test]
fn case_14_deleting_the_thread_leaves_its_text_nowhere_in_org_db() {
    let org = start();
    let (a, aid) = admin(&org);
    let (b, bid) = member(&org, &a, "user", "Bo");
    let marker = "kumquatmarker7f3a";
    for i in 0..40 {
        seed(&org, &aid, "user", &format!("{marker} question {i}"));
        seed(
            &org,
            &aid,
            "assistant",
            &format!("an answer about {marker} {i}"),
        );
    }
    let left = seed(&org, &bid, "user", "Bo keeps this");
    assert!(
        holds(&org.db_bytes(), marker),
        "the marker is in the thread"
    );
    assert_eq!(delete(&org, &a, "/api/org/v1/thread"), 204);
    let t = thread(&org, &a);
    assert_eq!(seqs(&t["messages"]), Vec::<i64>::new());
    assert_eq!(found(&org, &a, marker), Vec::<i64>::new());
    for f in ["org.db", "org.db-wal"] {
        let bytes = std::fs::read(org.data.join(f)).unwrap_or_default();
        assert!(!holds(&bytes, marker), "the marker is still in {f}");
    }
    assert_eq!(seqs(&thread(&org, &b)["messages"]), [left]);
    assert_eq!(found(&org, &b, "keeps"), [left]);
}

#[test]
fn case_16_admins_read_a_thread_only_when_the_org_allows_it() {
    let org = start();
    let (a, _) = admin(&org);
    let (u, uid) = member(&org, &a, "user", "Bo");
    let seq = seed(&org, &uid, "user", "my own plan");
    let read = |id: &str| org.get(&format!("/api/org/v1/members/{id}/thread"), Some(&a));
    assert_eq!(
        org.get("/api/org/v1/settings", Some(&a)).json(),
        json!({"studentsAllowed": false, "dailyTokens": null, "keepThreads": true, "adminsReadThreads": false, "electricityPerKwh": "0.15", "timezone": "UTC", "searchUrl": null, "searchNotice": null})
    );
    let privacy = json!({"keepThreads": true, "adminsReadThreads": false, "search": false});
    assert_eq!(org.get("/api/org/v1/privacy", Some(&u)).json(), privacy);
    let r = read(&uid);
    assert_eq!(
        (r.status, r.json()),
        (403, json!({"error": "threads_private"}))
    );
    let p = org.call(
        "PATCH",
        "/api/org/v1/settings",
        Some(&a),
        json!({"adminsReadThreads": true}),
    );
    assert_eq!(
        (p.status, &p.json()["adminsReadThreads"]),
        (200, &json!(true))
    );
    let row = &audit(&org, &a)[0];
    assert_eq!(
        (&row["action"], &row["target"]),
        (&json!("settings.update"), &json!("adminsReadThreads"))
    );
    assert_eq!(
        org.get("/api/org/v1/privacy", Some(&u)).json(),
        json!({"keepThreads": true, "adminsReadThreads": true, "search": false})
    );
    let r = read(&uid);
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(seqs(&r.json()["messages"]), [seq]);
    assert_eq!(r.json()["messages"][0]["text"], "my own plan");
    let row = &audit(&org, &a)[0];
    assert_eq!(
        (&row["action"], &row["target"]),
        (&json!("thread.read"), &json!(uid))
    );
    assert_eq!(read("m_0000000000000000").status, 404);
    let reads = audit(&org, &a).as_array().unwrap().len();
    for bad in ["limit=0", "before=x"] {
        let r = org.get(&format!("/api/org/v1/members/{uid}/thread?{bad}"), Some(&a));
        assert_eq!(r.status, 400, "{bad}");
    }
    assert_eq!(
        audit(&org, &a).as_array().unwrap().len(),
        reads,
        "a refused read writes no thread.read"
    );
    for bad in [
        json!({"keepThreads": "no"}),
        json!({"adminsReadThreads": 1}),
    ] {
        let r = org.call("PATCH", "/api/org/v1/settings", Some(&a), bad);
        assert_eq!(r.status, 400);
    }
}

#[test]
fn keep_threads_off_shows_nothing_and_deletes_nothing() {
    let org = start();
    let (a, aid) = admin(&org);
    let seq = seed(&org, &aid, "user", "an otter swims");
    let off = org.call(
        "PATCH",
        "/api/org/v1/settings",
        Some(&a),
        json!({"keepThreads": false}),
    );
    assert_eq!(
        (off.status, &off.json()["keepThreads"]),
        (200, &json!(false))
    );
    assert_eq!(
        thread(&org, &a),
        json!({"messages": [], "stored": false, "more": false})
    );
    let r = search(&org, &a, "otter");
    assert_eq!(
        (r.status, r.json()),
        (200, json!({"results": [], "stored": false}))
    );
    let row = &audit(&org, &a)[0];
    assert_eq!(row["target"], "keepThreads");
    org.call(
        "PATCH",
        "/api/org/v1/settings",
        Some(&a),
        json!({"keepThreads": true}),
    );
    assert_eq!(seqs(&thread(&org, &a)["messages"]), [seq]);
}

#[test]
fn a_thread_reads_newest_first_a_page_at_a_time() {
    let org = start();
    let (a, aid) = admin(&org);
    let all: Vec<i64> = (0..5)
        .map(|i| seed(&org, &aid, "user", &format!("note {i}")))
        .collect();
    let page = |q: &str| {
        let r = org.get(&format!("/api/org/v1/thread?{q}"), Some(&a));
        assert_eq!(r.status, 200, "{q}: {}", r.body);
        let j = r.json();
        (seqs(&j["messages"]), j["more"].as_bool().unwrap())
    };
    assert_eq!(page("limit=2"), (vec![all[4], all[3]], true));
    assert_eq!(
        page(&format!("limit=2&before={}", all[3])),
        (vec![all[2], all[1]], true)
    );
    assert_eq!(
        page(&format!("limit=2&before={}", all[1])),
        (vec![all[0]], false)
    );
    let first = &thread(&org, &a)["messages"][4];
    assert_eq!(
        (
            &first["role"],
            &first["text"],
            &first["model"],
            &first["status"]
        ),
        (
            &json!("user"),
            &json!("note 0"),
            &json!("stub/m"),
            &Value::Null
        )
    );
    for bad in ["limit=0", "limit=101", "before=0", "before=x"] {
        let r = org.get(&format!("/api/org/v1/thread?{bad}"), Some(&a));
        assert_eq!(r.status, 400, "{bad}");
    }
}

#[test]
fn case_17_a_user_cannot_read_another_members_thread() {
    let org = start();
    let (a, aid) = admin(&org);
    seed(&org, &aid, "user", "the admin's own words");
    org.call(
        "PATCH",
        "/api/org/v1/settings",
        Some(&a),
        json!({"adminsReadThreads": true}),
    );
    for role in ["user", "full", "student"] {
        let (c, _) = member(&org, &a, role, "Bo");
        let r = org.get(&format!("/api/org/v1/members/{aid}/thread"), Some(&c));
        assert_eq!(
            (r.status, r.json()),
            (403, json!({"error": "forbidden"})),
            "{role}"
        );
        assert!(!r.body.contains("own words"));
    }
}

#[test]
fn case_18_the_previous_schema_gets_threads_and_keeps_its_rows() {
    let org = start();
    let (a, aid) = admin(&org);
    let (u, _) = member(&org, &a, "user", "Bo");
    let version = |org: &Org| -> i64 {
        org.db()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    };
    let current = version(&org);
    org.db()
        .execute_batch(&format!(
            "DROP TABLE artifact_versions; DROP TABLE artifacts; DROP TABLE message_steps; DROP INDEX usage_compute; ALTER TABLE usage DROP COLUMN compute_id; DROP TABLE compute_results; DROP TABLE compute_prompts; DROP TABLE compute; DROP TABLE limits; ALTER TABLE usage DROP COLUMN cost_nanos; ALTER TABLE usage DROP COLUMN cost_known; ALTER TABLE sources DROP COLUMN pricing; DROP TABLE messages_fts; DROP TABLE messages; PRAGMA user_version = {};",
            current - 5
        ))
        .unwrap();
    let rows = |org: &Org| -> (i64, i64) {
        org.db()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM members), (SELECT COUNT(*) FROM audit)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
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
    let after = rows(&org);
    assert_eq!((after.0, after.1), (before.0, before.1 + 1));
    assert_eq!(org.get("/api/org/v1/me", Some(&u)).status, 200);
    let seq = seed(&org, &aid, "user", "a heron after the move");
    assert_eq!(found(&org, &a, "heron"), [seq]);
    assert_eq!(seqs(&thread(&org, &a)["messages"]), [seq]);
}

/// True when the marker is in org.db or its write-ahead log.
fn on_disk(org: &Org, marker: &str) -> bool {
    ["org.db", "org.db-wal"]
        .iter()
        .any(|f| holds(&std::fs::read(org.data.join(f)).unwrap_or_default(), marker))
}

#[test]
fn a_deleted_message_leaves_its_text_nowhere_in_org_db() {
    let org = start();
    let (a, aid) = admin(&org);
    let marker = "quincemarker2b9d";
    for i in 0..40 {
        seed(&org, &aid, "user", &format!("note {i} about the weather"));
    }
    let gone = seed(&org, &aid, "user", &format!("{marker} is private"));
    assert!(on_disk(&org, marker), "the marker is in the thread");
    let path = format!("/api/org/v1/thread/messages/{gone}");
    assert_eq!(delete(&org, &a, &path), 204);
    assert!(
        !on_disk(&org, marker),
        "the deleted message is still on disk"
    );
    assert_eq!(found(&org, &a, "weather").len(), 20);
}

#[test]
fn a_delete_while_a_reader_holds_the_log_is_503_until_it_lets_go() {
    let org = start();
    let (a, aid) = admin(&org);
    let marker = "loquatmarker5c1e";
    seed(&org, &aid, "user", &format!("{marker} once"));
    let reader = org.db();
    reader.execute_batch("BEGIN").unwrap();
    let n: i64 = reader
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    let r = org.call(
        "DELETE",
        "/api/org/v1/thread/messages/999",
        Some(&a),
        json!({}),
    );
    assert_eq!(r.status, 404, "nothing deleted and nothing pending is 404");
    let r = org.call("DELETE", "/api/org/v1/thread", Some(&a), json!({}));
    assert_eq!(r.status, 503, "a held log answers busy");
    assert_eq!(r.json(), json!({"error": "busy"}));
    assert_eq!(r.header("retry-after"), Some("1"));
    assert_eq!(seqs(&thread(&org, &a)["messages"]), Vec::<i64>::new());
    let r = org.call(
        "DELETE",
        "/api/org/v1/thread/messages/999",
        Some(&a),
        json!({}),
    );
    assert_eq!(r.status, 503, "its own pending cleanup still held is busy");
    let (b, _) = member(&org, &a, "user", "Bo");
    let r = org.call(
        "DELETE",
        "/api/org/v1/thread/messages/999",
        Some(&b),
        json!({}),
    );
    assert_eq!(r.status, 404, "another member's missing seq is never busy");
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    assert_eq!(delete(&org, &a, "/api/org/v1/thread"), 204);
    assert!(!on_disk(&org, marker), "the marker is still on disk");
}

#[test]
fn deleting_one_thread_among_5000_messages_clears_it() {
    let org = start();
    let (a, aid) = admin(&org);
    let (_, b) = member(&org, &a, "user", "Bo");
    let (_, c) = member(&org, &a, "user", "Cy");
    let mut db = org.db();
    db.pragma_update(None, "secure_delete", true).unwrap();
    let tx = db.transaction().unwrap();
    for i in 0..5000 {
        let who = [&aid, &b, &c][i % 3];
        tx.execute(
            "INSERT INTO messages (member_id, role, text, model, at, status)
             VALUES (?1, 'user', ?2, 'stub/m', 0, NULL)",
            rusqlite::params![who, format!("message {i} on the river and the hill")],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    assert_eq!(delete(&org, &a, "/api/org/v1/thread"), 204);
    assert_eq!(found(&org, &a, "river"), Vec::<i64>::new());
}

/// The admin's `thread.read` audit rows.
fn reads(org: &Org, admin: &str) -> usize {
    let rows = audit(org, admin);
    let rows = rows.as_array().unwrap();
    rows.iter().filter(|r| r["action"] == "thread.read").count()
}

#[test]
fn an_admin_read_that_shows_no_kept_thread_is_not_audited() {
    let org = start();
    let (a, _) = admin(&org);
    let (_, uid) = member(&org, &a, "user", "Bo");
    seed(&org, &uid, "user", "my own plan");
    let set = |v: Value| {
        let r = org.call("PATCH", "/api/org/v1/settings", Some(&a), v);
        assert_eq!(r.status, 200, "{}", r.body);
    };
    set(json!({"adminsReadThreads": true, "keepThreads": false}));
    let read = || org.get(&format!("/api/org/v1/members/{uid}/thread"), Some(&a));
    let r = read();
    assert_eq!((r.status, &r.json()["stored"]), (200, &json!(false)));
    assert_eq!(reads(&org, &a), 0);
    let r = org.get("/api/org/v1/members/no-such-member/thread", Some(&a));
    assert_eq!((r.status, r.json()), (404, json!({"error": "not_found"})));

    set(json!({"keepThreads": true}));
    org.db()
        .execute(
            "UPDATE messages SET at = 'not a time' WHERE member_id = ?1",
            [&uid],
        )
        .unwrap();
    assert_eq!(read().status, 500);
    assert_eq!(reads(&org, &a), 0, "a read that failed is not audited");
}

#[test]
fn a_retried_message_delete_finishes_its_cleanup() {
    let org = start();
    let (a, aid) = admin(&org);
    let marker = "medlarmarker7d2f";
    seed(&org, &aid, "user", &format!("{marker} once"));
    let seq = seqs(&thread(&org, &a)["messages"])[0];
    let path = format!("/api/org/v1/thread/messages/{seq}");
    let reader = org.db();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(delete(&org, &a, &path), 503);
    assert_eq!(delete(&org, &a, &path), 503, "the log is still held");
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    assert_eq!(delete(&org, &a, &path), 404);
    assert!(!on_disk(&org, marker), "the retry left the marker on disk");
    assert_eq!(delete(&org, &a, &path), 404);
}

#[test]
fn a_cleanup_left_busy_survives_a_restart() {
    let mut org = start();
    let (a, aid) = admin(&org);
    let marker = "sorbmarker3a9b";
    seed(&org, &aid, "user", &format!("{marker} once"));
    let seq = seqs(&thread(&org, &a)["messages"])[0];
    let path = format!("/api/org/v1/thread/messages/{seq}");
    let reader = org.db();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(delete(&org, &a, &path), 503);
    org.restart(RESOLVE);
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    assert_eq!(delete(&org, &a, &path), 404);
    assert!(!on_disk(&org, marker), "the retry left the marker on disk");
}

#[test]
fn a_cleanup_left_busy_runs_when_the_server_starts() {
    let mut org = start();
    let (a, aid) = admin(&org);
    let marker = "sorbmarker7c41";
    seed(&org, &aid, "user", &format!("{marker} once"));
    let seq = seqs(&thread(&org, &a)["messages"])[0];
    let reader = org.db();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    let path = format!("/api/org/v1/thread/messages/{seq}");
    assert_eq!(delete(&org, &a, &path), 503);
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    org.restart(RESOLVE);
    let t = std::time::Instant::now();
    while on_disk(&org, marker) {
        assert!(
            t.elapsed() < std::time::Duration::from_secs(10),
            "the server left a pending cleanup undone"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
