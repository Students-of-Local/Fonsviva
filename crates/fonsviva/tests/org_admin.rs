//! Members, invites and the audit log, against the real `fonsviva org serve`.

mod org_support;

use org_support::{Org, start, token_of};
use serde_json::{Value, json};

/// Joins with the admin link and returns the admin's cookie and id.
fn admin(org: &Org) -> (String, String) {
    let r = org.join(&token_of(&org.admin_link), "Ada");
    assert_eq!(r.status, 200);
    (r.session(), r.json()["id"].as_str().unwrap().to_string())
}

fn invite(org: &Org, cookie: &str, role: &str, uses: u32) -> Value {
    let r = org.call(
        "POST",
        "/api/org/v1/invites",
        Some(cookie),
        json!({"role": role, "expiresHours": 24, "uses": uses}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()
}

fn patch(org: &Org, cookie: &str, id: &str, body: Value) -> org_support::Resp {
    org.call(
        "PATCH",
        &format!("/api/org/v1/members/{id}"),
        Some(cookie),
        body,
    )
}

#[test]
fn an_invite_with_two_uses_takes_two_people() {
    let org = start();
    let (a, _) = admin(&org);
    let inv = invite(&org, &a, "user", 2);
    let link = inv["link"].as_str().unwrap();
    assert!(link.starts_with("https://org.example/join/"));
    let t = token_of(link);
    for name in ["Bo", "Cy"] {
        let r = org.join(&t, name);
        assert_eq!(r.status, 200);
        assert_eq!(r.json()["role"], "user");
    }
    assert_eq!(org.join(&t, "Di").status, 410);
    let members = org.get("/api/org/v1/members", Some(&a)).json();
    let names: Vec<&str> = members
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Ada", "Bo", "Cy"]);
    for m in members.as_array().unwrap() {
        assert!(m["created"].is_string() && m["disabled"] == false);
    }
    let invites = org.get("/api/org/v1/invites", Some(&a));
    assert!(!invites.body.contains(&t), "invites never show a token");
    let row = invites
        .json()
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == inv["id"])
        .cloned()
        .unwrap();
    assert_eq!(
        (row["uses"].clone(), row["used"].clone()),
        (json!(2), json!(2))
    );
}

#[test]
fn a_user_gets_403_on_admin_routes() {
    let org = start();
    let (a, admin_id) = admin(&org);
    let u = org
        .join(
            &token_of(invite(&org, &a, "user", 1)["link"].as_str().unwrap()),
            "Bo",
        )
        .session();
    for path in [
        "/api/org/v1/members",
        "/api/org/v1/invites",
        "/api/org/v1/audit",
    ] {
        let r = org.get(path, Some(&u));
        assert_eq!(r.status, 403, "{path}");
        assert_eq!(r.json(), json!({"error": "forbidden"}));
    }
    let r = org.call(
        "POST",
        "/api/org/v1/invites",
        Some(&u),
        json!({"role": "admin", "expiresHours": 1, "uses": 1}),
    );
    assert_eq!(r.status, 403);
    assert_eq!(
        patch(&org, &u, &admin_id, json!({"disabled": true})).status,
        403
    );
    assert_eq!(org.get("/api/org/v1/me", Some(&u)).json()["role"], "user");
}

#[test]
fn a_disabled_user_is_signed_out() {
    let org = start();
    let (a, _) = admin(&org);
    let r = org.join(
        &token_of(invite(&org, &a, "user", 1)["link"].as_str().unwrap()),
        "Bo",
    );
    let (u, uid) = (r.session(), r.json()["id"].as_str().unwrap().to_string());
    assert_eq!(org.get("/api/org/v1/me", Some(&u)).status, 200);
    assert_eq!(patch(&org, &a, &uid, json!({"disabled": true})).status, 200);
    assert_eq!(org.get("/api/org/v1/me", Some(&u)).status, 401);
    let left: i64 = org
        .db()
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE member_id = ?1",
            [&uid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0, "disabling deletes the sessions");
}

#[test]
fn a_role_change_counts_on_the_next_request() {
    let org = start();
    let (a, _) = admin(&org);
    let r = org.join(
        &token_of(invite(&org, &a, "student", 1)["link"].as_str().unwrap()),
        "Bo",
    );
    let (u, uid) = (r.session(), r.json()["id"].as_str().unwrap().to_string());
    assert_eq!(patch(&org, &a, &uid, json!({"role": "full"})).status, 200);
    assert_eq!(org.get("/api/org/v1/me", Some(&u)).json()["role"], "full");
    assert_eq!(patch(&org, &a, &uid, json!({"role": "admin"})).status, 200);
    assert_eq!(org.get("/api/org/v1/members", Some(&u)).status, 200);
    assert_eq!(patch(&org, &a, &uid, json!({"role": "user"})).status, 200);
    assert_eq!(org.get("/api/org/v1/members", Some(&u)).status, 403);
    assert_eq!(patch(&org, &a, &uid, json!({"role": "owner"})).status, 400);
    assert_eq!(patch(&org, &a, &uid, json!({})).status, 400);
    assert_eq!(
        patch(&org, &a, "m_nobody", json!({"role": "user"})).status,
        404
    );
}

#[test]
fn the_only_admin_cannot_step_down() {
    let org = start();
    let (a, aid) = admin(&org);
    for body in [json!({"role": "user"}), json!({"disabled": true})] {
        let r = patch(&org, &a, &aid, body);
        assert_eq!(r.status, 409);
        assert_eq!(r.json(), json!({"error": "last_admin"}));
    }
    assert_eq!(org.get("/api/org/v1/me", Some(&a)).json()["role"], "admin");
}

#[test]
fn a_second_admin_lets_the_first_step_down() {
    let org = start();
    let (a, aid) = admin(&org);
    let b = org.join(
        &token_of(invite(&org, &a, "user", 1)["link"].as_str().unwrap()),
        "Bo",
    );
    let (bc, bid) = (b.session(), b.json()["id"].as_str().unwrap().to_string());
    assert_eq!(patch(&org, &a, &bid, json!({"role": "admin"})).status, 200);
    assert_eq!(patch(&org, &a, &aid, json!({"role": "user"})).status, 200);
    assert_eq!(org.get("/api/org/v1/me", Some(&a)).json()["role"], "user");
    assert_eq!(patch(&org, &bc, &bid, json!({"role": "user"})).status, 409);
}

#[test]
fn invites_outside_the_limits_are_400() {
    let org = start();
    let (a, _) = admin(&org);
    for body in [
        json!({"role": "user", "expiresHours": 0, "uses": 1}),
        json!({"role": "user", "expiresHours": 721, "uses": 1}),
        json!({"role": "user", "expiresHours": 1, "uses": 0}),
        json!({"role": "user", "expiresHours": 1, "uses": 501}),
        json!({"role": "user", "expiresHours": 1, "uses": -1}),
        json!({"role": "owner", "expiresHours": 1, "uses": 1}),
        json!({"role": "user", "uses": 1}),
    ] {
        let r = org.call("POST", "/api/org/v1/invites", Some(&a), body.clone());
        assert_eq!(r.status, 400, "{body}");
        assert_eq!(r.json(), json!({"error": "bad_request"}));
    }
    for body in [
        json!({"role": "user", "expiresHours": 720, "uses": 500}),
        json!({"role": "admin", "expiresHours": 1, "uses": 1}),
    ] {
        let r = org.call("POST", "/api/org/v1/invites", Some(&a), body.clone());
        assert_eq!(r.status, 200, "{body}");
    }
}

#[test]
fn a_revoked_or_expired_invite_is_410() {
    let org = start();
    let (a, _) = admin(&org);
    let revoked = invite(&org, &a, "user", 5);
    let id = revoked["id"].as_str().unwrap();
    let r = org.call(
        "DELETE",
        &format!("/api/org/v1/invites/{id}"),
        Some(&a),
        json!({}),
    );
    assert_eq!(r.status, 200);
    assert_eq!(
        org.join(&token_of(revoked["link"].as_str().unwrap()), "Bo")
            .status,
        410
    );
    let r = org.call(
        "DELETE",
        "/api/org/v1/invites/inv_nobody",
        Some(&a),
        json!({}),
    );
    assert_eq!(r.status, 404);

    let expired = invite(&org, &a, "user", 5);
    org.db()
        .execute(
            "UPDATE invites SET expires_at = 1 WHERE id = ?1",
            [expired["id"].as_str().unwrap()],
        )
        .unwrap();
    let r = org.join(&token_of(expired["link"].as_str().unwrap()), "Bo");
    assert_eq!(r.status, 410);
    assert_eq!(r.json(), json!({"error": "gone"}));
}

#[test]
fn the_audit_log_lists_admin_actions_without_tokens() {
    let org = start();
    let (a, aid) = admin(&org);
    let inv = invite(&org, &a, "user", 2);
    let t = token_of(inv["link"].as_str().unwrap());
    let bo = org.join(&t, "Bo");
    let bid = bo.json()["id"].as_str().unwrap().to_string();
    let cy = org.join(&t, "Cy").json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(patch(&org, &a, &cy, json!({"disabled": true})).status, 200);
    assert_eq!(patch(&org, &a, &bid, json!({"role": "admin"})).status, 200);
    assert_eq!(patch(&org, &a, &aid, json!({"role": "user"})).status, 200);
    let log = org.get("/api/org/v1/audit", Some(&bo.session()));
    assert_eq!(log.status, 200);
    let rows: Vec<(String, String, String)> = log
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            assert!(r["at"].is_string());
            (
                r["admin"].as_str().unwrap().into(),
                r["action"].as_str().unwrap().into(),
                r["target"].as_str().unwrap().into(),
            )
        })
        .collect();
    let inv_id = inv["id"].as_str().unwrap().to_string();
    let want = [
        (aid.clone(), "member.role=user".to_string(), aid.clone()),
        (aid.clone(), "member.role=admin".into(), bid.clone()),
        (aid.clone(), "member.disable".into(), cy.clone()),
        (aid.clone(), "invite.create".into(), inv_id),
    ];
    assert_eq!(rows[..4], want);
    assert_eq!(rows[4].1, "admin_link.create");
    assert_eq!(rows[4].0, "cli");
    assert_eq!(rows.len(), 5);
    let admin_token = token_of(&org.admin_link);
    let sessions = [bo.session(), a.clone()];
    for secret in [t.as_str(), admin_token.as_str()]
        .into_iter()
        .chain(sessions.iter().map(|s| s.trim_start_matches("fv_session=")))
    {
        assert!(!log.body.contains(secret));
        assert!(!org.stderr.lock().unwrap().contains(secret));
    }
}
