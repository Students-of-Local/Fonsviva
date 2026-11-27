//! Source pricing, the cost of each usage row and the totals admins read,
//! against the real `fonsviva org serve` and a stub source on 127.0.0.1.
//! Every expected cost is in nano-USD, worked out by hand.

mod org_support;

use org_support::stub::Stub;
use org_support::{Org, Resp, add_source, admin, member, start};
use serde_json::{Value, json};

/// An org with an admin, the stub added as `stub` with models `a` and `b`
/// reporting 1000 prompt and 500 completion tokens, and the source's id.
fn ready() -> (Org, Stub, String, String) {
    let org = start();
    let stub = Stub::start();
    {
        let mut s = stub.state();
        s.models = vec!["a".into(), "b".into()];
        s.prompt_tokens = 1000;
        s.completion_tokens = 500;
    }
    let (a, _) = admin(&org);
    let r = add_source(&org, &a, "stub", &stub.base(), None);
    assert_eq!(r.status, 200, "{}", r.body);
    let id = r.json()["id"].as_str().unwrap().to_string();
    (org, stub, a, id)
}

fn price(org: &Org, admin: &str, id: &str, pricing: Value) -> Resp {
    org.call(
        "PATCH",
        &format!("/api/org/v1/sources/{id}"),
        Some(admin),
        json!({ "pricing": pricing }),
    )
}

fn priced(org: &Org, admin: &str, id: &str, pricing: Value) {
    let r = price(org, admin, id, pricing);
    assert_eq!(r.status, 200, "{}", r.body);
}

fn ask(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "abcdefgh"}]})
}

fn chat(org: &Org, cookie: &str, model: &str) {
    let r = org.call(
        "POST",
        "/api/org/v1/chat/completions",
        Some(cookie),
        ask(model),
    );
    assert_eq!(r.status, 200, "{}", r.body);
}

/// Each usage row, oldest first: model, cost, known, ms and estimated.
fn rows(org: &Org) -> Vec<(String, i64, bool, i64, bool)> {
    let db = org.db();
    let mut q = db
        .prepare("SELECT model, cost_nanos, cost_known, ms, estimated FROM usage ORDER BY seq")
        .unwrap();
    q.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn costs(org: &Org) -> Vec<(i64, bool)> {
    rows(org).into_iter().map(|r| (r.1, r.2)).collect()
}

fn source_pricing(org: &Org, admin: &str) -> Value {
    org.get("/api/org/v1/sources", Some(admin)).json()[0]["pricing"].clone()
}

fn totals(org: &Org, admin: &str, by: &str) -> Value {
    let r = org.get(
        &format!("/api/org/v1/usage/totals?days=1&by={by}"),
        Some(admin),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()
}

const PRICES: &str = r#"{"promptPerMillion": "2", "completionPerMillion": "8"}"#;

fn prices() -> Value {
    serde_json::from_str(PRICES).unwrap()
}

#[test]
fn case_1_a_row_costs_its_tokens_at_the_source_prices() {
    let (org, _stub, a, id) = ready();
    priced(&org, &a, &id, prices());
    assert_eq!(
        source_pricing(&org, &a),
        json!({"promptPerMillion": "2", "completionPerMillion": "8", "watts": null, "models": {}})
    );
    chat(&org, &a, "stub/a");
    assert_eq!(costs(&org), [(6_000_000, true)]);
    let u = org.get("/api/org/v1/usage?days=1", Some(&a)).json();
    assert_eq!(
        (&u[0]["costNanos"], &u[0]["costKnown"]),
        (&json!(6_000_000), &json!(true))
    );
}

#[test]
fn case_2_a_model_override_wins_field_by_field() {
    let (org, _stub, a, id) = ready();
    let mut p = prices();
    p["models"] = json!({"b": {"completionPerMillion": "1"}});
    priced(&org, &a, &id, p);
    chat(&org, &a, "stub/b");
    chat(&org, &a, "stub/a");
    assert_eq!(costs(&org), [(2_500_000, true), (6_000_000, true)]);
}

#[test]
fn case_3_watts_add_the_power_the_request_took() {
    let (org, stub, a, id) = ready();
    stub.state().chat_delay = std::time::Duration::from_millis(300);
    let mut p = prices();
    p["watts"] = json!(300);
    priced(&org, &a, &id, p);
    let s = org.call(
        "PATCH",
        "/api/org/v1/settings",
        Some(&a),
        json!({"electricityPerKwh": "0.15"}),
    );
    assert_eq!(s.status, 200, "{}", s.body);
    assert_eq!(s.json()["electricityPerKwh"], "0.15");
    chat(&org, &a, "stub/a");
    let (_, cost, known, ms, _) = rows(&org).remove(0);
    assert!(ms >= 300, "{ms}");
    assert_eq!(cost, 6_000_000 + 300 * ms * 150_000 / 3_600_000);
    assert!(known);
}

#[test]
fn case_4_a_source_with_no_pricing_costs_nothing_unknown() {
    let (org, _stub, a, id) = ready();
    chat(&org, &a, "stub/a");
    priced(&org, &a, &id, prices());
    priced(&org, &a, &id, Value::Null);
    assert_eq!(source_pricing(&org, &a), Value::Null);
    chat(&org, &a, "stub/a");
    assert_eq!(costs(&org), [(0, false), (0, false)]);
    let u = org.get("/api/org/v1/usage?days=1", Some(&a)).json();
    assert_eq!(
        (&u[0]["costNanos"], &u[0]["costKnown"]),
        (&json!(0), &json!(false))
    );
}

#[test]
fn case_5_a_price_change_never_rewrites_an_old_row() {
    let (org, _stub, a, id) = ready();
    priced(&org, &a, &id, prices());
    chat(&org, &a, "stub/a");
    priced(
        &org,
        &a,
        &id,
        json!({"promptPerMillion": "1", "completionPerMillion": "1"}),
    );
    chat(&org, &a, "stub/a");
    assert_eq!(costs(&org), [(6_000_000, true), (1_500_000, true)]);
}

#[test]
fn case_6_an_estimated_row_costs_its_estimate() {
    let (org, stub, a, id) = ready();
    stub.state().no_usage = true;
    priced(&org, &a, &id, prices());
    chat(&org, &a, "stub/a");
    let (_, cost, known, _, estimated) = rows(&org).remove(0);
    assert_eq!((cost, known, estimated), (28_000, true, true));
    let u = org.get("/api/org/v1/usage?days=1", Some(&a)).json();
    assert_eq!(u[0]["estimated"], true);
}

#[test]
fn case_7_totals_by_member_and_model_sum_the_rows() {
    let (org, _stub, a, id) = ready();
    let mut p = prices();
    p["models"] = json!({"b": {"completionPerMillion": "1"}});
    priced(&org, &a, &id, p);
    let (bo, bo_id) = member(&org, &a, "user", "Bo");
    let (cy, cy_id) = member(&org, &a, "user", "Cy");
    chat(&org, &bo, "stub/a");
    chat(&org, &cy, "stub/b");
    chat(&org, &cy, "stub/a");
    let by = totals(&org, &a, "member");
    let mut want = vec![
        json!({"key": bo_id, "name": "Bo", "requests": 1, "promptTokens": 1000, "completionTokens": 500, "costNanos": 6_000_000, "costKnown": true, "estimated": false}),
        json!({"key": cy_id, "name": "Cy", "requests": 2, "promptTokens": 2000, "completionTokens": 1000, "costNanos": 8_500_000, "costKnown": true, "estimated": false}),
    ];
    want.sort_by_key(|v| v["key"].as_str().unwrap().to_string());
    assert_eq!(by["rows"], json!(want));
    let total = json!({"requests": 3, "promptTokens": 3000, "completionTokens": 1500, "costNanos": 14_500_000, "costKnown": true, "estimated": false});
    assert_eq!(by["total"], total);
    let by = totals(&org, &a, "model");
    assert_eq!(
        by["rows"],
        json!([
            {"key": "stub/a", "requests": 2, "promptTokens": 2000, "completionTokens": 1000, "costNanos": 12_000_000, "costKnown": true, "estimated": false},
            {"key": "stub/b", "requests": 1, "promptTokens": 1000, "completionTokens": 500, "costNanos": 2_500_000, "costKnown": true, "estimated": false},
        ])
    );
    assert_eq!(by["total"], total);
}

#[test]
fn case_8_one_unpriced_row_makes_the_total_unknown() {
    let (org, stub, a, id) = ready();
    let r = add_source(&org, &a, "free", &stub.base(), None);
    assert_eq!(r.status, 200, "{}", r.body);
    priced(&org, &a, &id, prices());
    chat(&org, &a, "stub/a");
    chat(&org, &a, "free/a");
    let by = totals(&org, &a, "model");
    let known: Vec<_> = by["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["key"].clone(),
                r["costNanos"].clone(),
                r["costKnown"].clone(),
            )
        })
        .collect();
    assert_eq!(
        known,
        [
            (json!("free/a"), json!(0), json!(false)),
            (json!("stub/a"), json!(6_000_000), json!(true)),
        ]
    );
    assert_eq!(by["total"]["costKnown"], false);
    assert_eq!(by["total"]["costNanos"], 6_000_000);
    assert_eq!(totals(&org, &a, "member")["total"]["costKnown"], false);
}

#[test]
fn case_9_a_thread_send_and_the_v1_door_carry_cost() {
    let (org, _stub, a, id) = ready();
    priced(&org, &a, &id, prices());
    let (u, _) = member(&org, &a, "user", "Bo");
    let r = org.call(
        "POST",
        "/api/org/v1/thread/messages",
        Some(&u),
        json!({"text": "hello", "model": "stub/a"}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains(r#""type":"done""#), "{}", r.body);
    let k = org.call("POST", "/api/org/v1/keys", Some(&u), json!({"name": "cli"}));
    let key = k.json()["key"].as_str().unwrap().to_string();
    let v1 = org.bearer(
        "POST",
        "/v1/chat/completions",
        Some(&key),
        Some(ask("stub/b").to_string()),
    );
    assert_eq!(v1.status, 200, "{}", v1.body);
    assert_eq!(costs(&org), [(6_000_000, true), (6_000_000, true)]);
}

#[test]
fn case_10_bad_shapes_are_refused_and_change_nothing() {
    let (org, _stub, a, id) = ready();
    priced(&org, &a, &id, prices());
    let many: serde_json::Map<String, Value> = (0..501)
        .map(|i| (format!("m{i}"), json!({"promptPerMillion": "1"})))
        .collect();
    let bad = [
        json!({"pricing": {"promptPerMillion": "-1"}}),
        json!({"pricing": {"promptPerMillion": "10000.000001"}}),
        json!({"pricing": {"promptPerMillion": "0.0000001"}}),
        json!({"pricing": {"promptPerMillion": "1e3"}}),
        json!({"pricing": {"promptPerMillion": 2}}),
        json!({"pricing": {"completionPerMillion": " 2"}}),
        json!({"pricing": {"watts": 0}}),
        json!({"pricing": {"watts": 100_001}}),
        json!({"pricing": {"watts": "300"}}),
        json!({"pricing": {"watts": 1.5}}),
        json!({"pricing": {"models": many}}),
        json!({"pricing": {"models": {"b": {"promptPerMillion": "x"}}}}),
        json!({"pricing": {"models": {"b": {"watts": 3}}}}),
        json!({"pricing": {"models": {"": {"promptPerMillion": "1"}}}}),
        json!({"pricing": {"tier": "gold"}}),
        json!({"pricing": "2"}),
        json!({"pricing": prices(), "name": "x"}),
        json!({}),
    ];
    for b in bad {
        let r = org.call(
            "PATCH",
            &format!("/api/org/v1/sources/{id}"),
            Some(&a),
            b.clone(),
        );
        assert_eq!(
            (r.status, r.json()["error"].clone()),
            (400, json!("bad_request")),
            "{b}"
        );
    }
    let mut most: serde_json::Map<String, Value> = serde_json::Map::new();
    for i in 0..500 {
        most.insert(format!("m{i}"), json!({"promptPerMillion": "1"}));
    }
    let r = price(&org, &a, "src_nobody", prices());
    assert_eq!(r.status, 404, "{}", r.body);
    assert_eq!(
        source_pricing(&org, &a),
        json!({"promptPerMillion": "2", "completionPerMillion": "8", "watts": null, "models": {}})
    );
    for kwh in [
        json!("1e3"),
        json!("100.000001"),
        json!(0.15),
        json!("-1"),
        json!("0.1234567"),
    ] {
        let r = org.call(
            "PATCH",
            "/api/org/v1/settings",
            Some(&a),
            json!({"electricityPerKwh": kwh}),
        );
        assert_eq!(r.status, 400, "{kwh}");
    }
    let s = org.get("/api/org/v1/settings", Some(&a)).json();
    assert_eq!(s["electricityPerKwh"], "0.15");
    let r = price(&org, &a, &id, json!({"watts": 100_000, "models": most}));
    assert_eq!(r.status, 200, "{}", r.body);
}

#[test]
fn case_11_a_user_can_not_price_or_read_totals() {
    let (org, _stub, a, id) = ready();
    let (u, _) = member(&org, &a, "user", "Bo");
    assert_eq!(price(&org, &u, &id, prices()).status, 403);
    for by in ["member", "model"] {
        let r = org.get(&format!("/api/org/v1/usage/totals?by={by}"), Some(&u));
        assert_eq!(r.status, 403, "{by}");
    }
    assert_eq!(source_pricing(&org, &a), Value::Null);
    for q in [
        "by=day",
        "by=",
        "days=1",
        "by=member&days=0",
        "by=member&days=367",
    ] {
        let r = org.get(&format!("/api/org/v1/usage/totals?{q}"), Some(&a));
        assert_eq!(r.status, 400, "{q}");
    }
    let r = org.get("/api/org/v1/usage/totals?by=member&days=366", Some(&a));
    assert_eq!(r.status, 200, "{}", r.body);
}

#[test]
fn case_12_a_member_reads_only_their_own_costs() {
    let (org, _stub, a, id) = ready();
    priced(&org, &a, &id, prices());
    let (bo, bo_id) = member(&org, &a, "user", "Bo");
    let (cy, _) = member(&org, &a, "user", "Cy");
    chat(&org, &bo, "stub/a");
    chat(&org, &cy, "stub/a");
    chat(&org, &cy, "stub/a");
    let mine = org.get("/api/org/v1/me/usage?days=1", Some(&bo)).json();
    let mine = mine.as_array().unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0]["member"], bo_id);
    assert_eq!(
        (&mine[0]["costNanos"], &mine[0]["costKnown"]),
        (&json!(6_000_000), &json!(true))
    );
    let all = org.get("/api/org/v1/usage?days=1", Some(&a)).json();
    let sum: i64 = all
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["costNanos"].as_i64().unwrap())
        .sum();
    assert_eq!(sum, 18_000_000);
}

#[test]
fn case_13_the_audit_names_fields_and_never_prices_or_models() {
    let (org, _stub, a, id) = ready();
    priced(
        &org,
        &a,
        &id,
        json!({"promptPerMillion": "7.654321", "models": {"zz-model-q": {"completionPerMillion": "3.141592"}}}),
    );
    priced(
        &org,
        &a,
        &id,
        json!({"promptPerMillion": "7.654321", "models": {"zz-model-q": {"completionPerMillion": "3.141592"}}}),
    );
    priced(
        &org,
        &a,
        &id,
        json!({"promptPerMillion": "7.654321", "watts": 450}),
    );
    let r = org.get("/api/org/v1/audit", Some(&a));
    let rows: Vec<_> = r
        .json()
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "source.update")
        .map(|e| e["target"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        rows,
        [
            format!("{id}:pricing.watts,pricing.models"),
            format!("{id}:pricing.promptPerMillion,pricing.models"),
        ]
    );
    for secret in ["7.654321", "3.141592", "zz-model-q"] {
        assert!(!r.body.contains(secret), "{secret} in {}", r.body);
    }
}

#[test]
fn case_14_a_schema_6_store_reads_old_rows_as_unknown() {
    let (mut org, _stub, a, id) = ready();
    chat(&org, &a, "stub/a");
    org.db()
        .execute_batch(
            "DROP TABLE limits; ALTER TABLE usage DROP COLUMN cost_nanos; ALTER TABLE usage DROP COLUMN cost_known;
             ALTER TABLE sources DROP COLUMN pricing; PRAGMA user_version = 6;",
        )
        .unwrap();
    org.restart(org_support::RESOLVE);
    let v: i64 = org
        .db()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 8);
    let u = org.get("/api/org/v1/usage?days=1", Some(&a)).json();
    assert_eq!(
        (&u[0]["costNanos"], &u[0]["costKnown"]),
        (&json!(0), &json!(false))
    );
    priced(&org, &a, &id, prices());
    chat(&org, &a, "stub/a");
    assert_eq!(costs(&org), [(0, false), (6_000_000, true)]);
}

#[test]
fn case_15_extreme_inputs_are_exact() {
    let (org, stub, a, id) = ready();
    {
        let mut s = stub.state();
        s.prompt_tokens = 1_000_000_000;
        s.completion_tokens = 1_000_000_000;
        s.chat_delay = std::time::Duration::from_millis(50);
    }
    priced(
        &org,
        &a,
        &id,
        json!({"promptPerMillion": "10000", "completionPerMillion": "10000", "watts": 100_000}),
    );
    let s = org.call(
        "PATCH",
        "/api/org/v1/settings",
        Some(&a),
        json!({"electricityPerKwh": "100"}),
    );
    assert_eq!(s.json()["electricityPerKwh"], "100");
    chat(&org, &a, "stub/a");
    let (_, cost, known, ms, _) = rows(&org).remove(0);
    assert_eq!(
        cost,
        20_000_000_000_000_000 + 100_000 * ms * 100_000_000 / 3_600_000
    );
    assert!(known);
    let by = totals(&org, &a, "model");
    assert_eq!(by["total"]["costNanos"], json!(cost));
}
