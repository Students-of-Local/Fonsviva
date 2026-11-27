//! Joining, sessions and the request guard, against the real
//! `fonsviva org serve`.

mod org_support;

use org_support::{Req, start, token_of};
use serde_json::json;

#[test]
fn the_admin_link_joins_as_admin_with_a_strict_cookie() {
    let org = start();
    assert!(org.admin_link.starts_with("https://org.example/join/"));
    let r = org.join(&token_of(&org.admin_link), "Ada");
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["role"], "admin");
    let cookie = r.header("set-cookie").unwrap().to_string();
    let parts: Vec<&str> = cookie.split("; ").collect();
    for flag in [
        "HttpOnly",
        "SameSite=Lax",
        "Path=/",
        "Secure",
        "Max-Age=2592000",
    ] {
        assert!(parts.contains(&flag), "{flag} missing from {cookie}");
    }
    let me = org.get("/api/org/v1/me", Some(&r.session()));
    assert_eq!(me.status, 200);
    assert_eq!(me.json()["role"], "admin");
    assert_eq!(me.json()["name"], "Ada");
}

#[test]
fn the_join_page_is_a_form() {
    let org = start();
    let path = format!("/join/{}", token_of(&org.admin_link));
    let r = org.send(Req::new("GET", &path, &org));
    assert_eq!(r.status, 200);
    assert!(r.body.contains("<form"));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let r = org.send(Req::new("GET", "/join/not-a-token", &org));
    assert_eq!(r.status, 410);
}

#[test]
fn an_admin_link_works_once() {
    let org = start();
    let t = token_of(&org.admin_link);
    assert_eq!(org.join(&t, "Ada").status, 200);
    let again = org.join(&t, "Eve");
    assert_eq!(again.status, 410);
    assert_eq!(again.json(), json!({"error": "gone"}));
}

#[test]
fn admin_link_revokes_unused_older_links() {
    let org = start();
    let out = org_support::fonsviva(org.scratch.path())
        .args(["org", "admin-link", "--data"])
        .arg(&org.data)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let newer = String::from_utf8(out.stdout).unwrap();
    assert_eq!(org.join(&token_of(&org.admin_link), "Old").status, 410);
    let r = org.join(&token_of(&newer), "New");
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["role"], "admin");
}

#[test]
fn me_without_a_session_is_401() {
    let org = start();
    let r = org.get("/api/org/v1/me", None);
    assert_eq!(r.status, 401);
    assert_eq!(r.json(), json!({"error": "unauthorized"}));
    let r = org.get(
        "/api/org/v1/me",
        Some(&format!("fv_session={}", "0".repeat(64))),
    );
    assert_eq!(r.status, 401);
}

#[test]
fn a_post_without_the_request_header_is_403() {
    let org = start();
    let t = token_of(&org.admin_link);
    let mut r = Req::new("POST", "/api/org/v1/join", &org);
    r.body = Some(json!({"token": t, "name": "Ada"}).to_string());
    let r = org.send(r);
    assert_eq!(r.status, 403);
    assert_eq!(r.json(), json!({"error": "forbidden"}));
    assert_eq!(
        org.join(&t, "Ada").status,
        200,
        "the refused POST used nothing"
    );
}

#[test]
fn a_post_from_a_foreign_origin_is_403() {
    let org = start();
    let t = token_of(&org.admin_link);
    let with_origin = |origin: &str| {
        let mut r = Req::new("POST", "/api/org/v1/join", &org);
        r.headers.push(("X-Fonsviva-Request", "1".into()));
        r.headers.push(("Origin", origin.into()));
        r.body = Some(json!({"token": t, "name": "Ada"}).to_string());
        org.send(r)
    };
    assert_eq!(with_origin("https://evil.example").status, 403);
    assert_eq!(with_origin("https://org.example").status, 200);
}

#[test]
fn a_foreign_host_is_421() {
    let org = start();
    for host in [
        Some("evil.example".to_string()),
        Some("org.example.evil".into()),
        None,
    ] {
        let mut r = Req::new("GET", "/api/org/v1/me", &org);
        r.host = host;
        assert_eq!(org.send(r).status, 421);
    }
    let mut r = Req::new("GET", "/api/org/v1/me", &org);
    r.host = Some("org.example".into());
    assert_eq!(org.send(r).status, 401, "the org URL's own host passes");
    let mut r = Req::new("GET", "/api/org/v1/me", &org);
    r.host = Some(format!("localhost:{}", org.port));
    assert_eq!(
        org.send(r).status,
        401,
        "loopback passes on a loopback listener"
    );
}

#[test]
fn eleven_bad_tokens_from_one_address_end_in_429() {
    let org = start();
    for i in 0..10 {
        let r = org.join(&format!("{i:064x}"), "Mallory");
        assert_eq!(r.status, 410, "attempt {}", i + 1);
    }
    let r = org.join(&"f".repeat(64), "Mallory");
    assert_eq!(r.status, 429);
    assert_eq!(
        org.join(&token_of(&org.admin_link), "Ada").status,
        429,
        "a good link waits out the window too"
    );
}

#[test]
fn serve_refuses_a_public_address_without_public() {
    let org = start();
    let out = org_support::fonsviva(org.scratch.path())
        .args(["org", "serve", "--listen", "0.0.0.0:0", "--data"])
        .arg(&org.data)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

#[test]
fn an_expired_link_is_410() {
    let org = start();
    org.db()
        .execute("UPDATE invites SET expires_at = 1", [])
        .unwrap();
    assert_eq!(org.join(&token_of(&org.admin_link), "Ada").status, 410);
}

#[test]
fn a_session_ends_30_days_after_last_use_and_at_logout() {
    let org = start();
    let r = org.join(&token_of(&org.admin_link), "Ada");
    let cookie = r.session();
    assert_eq!(org.get("/api/org/v1/me", Some(&cookie)).status, 200);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let db = org.db();
    db.execute("UPDATE sessions SET last_used = ?1", [now - 29 * 86400])
        .unwrap();
    assert_eq!(org.get("/api/org/v1/me", Some(&cookie)).status, 200);
    let used: i64 = db
        .query_row("SELECT last_used FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert!(used >= now, "a request marks the session used");
    db.execute("UPDATE sessions SET last_used = ?1", [now - 30 * 86400])
        .unwrap();
    assert_eq!(org.get("/api/org/v1/me", Some(&cookie)).status, 401);

    let out = org_support::fonsviva(org.scratch.path())
        .args(["org", "admin-link", "--data"])
        .arg(&org.data)
        .output()
        .unwrap();
    let cookie = org
        .join(&token_of(&String::from_utf8(out.stdout).unwrap()), "Ada")
        .session();
    let out = org.call("POST", "/api/org/v1/logout", Some(&cookie), json!({}));
    assert_eq!(out.status, 200);
    assert!(out.header("set-cookie").unwrap().contains("Max-Age=0"));
    assert_eq!(org.get("/api/org/v1/me", Some(&cookie)).status, 401);
}

#[test]
fn no_token_reaches_the_store_or_the_log() {
    let org = start();
    let t = token_of(&org.admin_link);
    let r = org.join(&t, "Ada");
    let session = r.session().trim_start_matches("fv_session=").to_string();
    drop(org.db());
    let mut raw = Vec::new();
    for f in ["org.db", "org.db-wal"] {
        if let Ok(b) = std::fs::read(org.data.join(f)) {
            raw.extend(b);
        }
    }
    let raw = String::from_utf8_lossy(&raw);
    assert!(!raw.contains(&t) && !raw.contains(&session));
    let log = org.stderr.lock().unwrap().clone();
    assert!(!log.contains(&t) && !log.contains(&session), "{log}");
}
