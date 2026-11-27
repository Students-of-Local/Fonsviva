//! Slice 6: the compute runner over real HTTP, with the stub as the source
//! and the server's clock moved through a file while it runs: start in the
//! window, the budget, the window's end, cancel, restart and usage.

mod org_support;

use fonsviva_core::util::rfc3339;
use org_support::stub::Stub;
use org_support::{Org, Resp, add_source, admin, member, start};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const HOUR: i64 = 3600;
/// Monday 2030-01-07 22:00 UTC: tonight's window opens.
const T: i64 = 1_894_053_600;

fn at(t: i64) -> String {
    rfc3339(t as u64)
}

struct Run {
    org: Org,
    stub: Stub,
    admin: String,
    user: String,
    user_id: String,
    source: String,
}

/// An org at 21:00 with the stub listing `a` and `b`, an admin and a user.
fn ready() -> Run {
    let org = start();
    org.move_clock(Some(&at(T - HOUR)));
    let stub = Stub::start();
    stub.state().models = vec!["a".into(), "b".into()];
    let (admin, _) = admin(&org);
    let r = add_source(&org, &admin, "stub", &stub.base(), None);
    assert_eq!(r.status, 200, "{}", r.body);
    let source = r.json()["id"].as_str().unwrap().to_string();
    let (user, user_id) = member(&org, &admin, "user", "Bo");
    Run {
        org,
        stub,
        admin,
        user,
        user_id,
        source,
    }
}

/// Files a request for 22:00 to 06:00 and returns its id.
fn file(org: &Org, cookie: &str, prompts: &[&str], budget: u64) -> String {
    let body = json!({
        "model": "stub/a",
        "prompts": prompts,
        "start": at(T),
        "end": at(T + 8 * HOUR),
        "maxTokens": budget,
    });
    let r = org.call("POST", "/api/org/v1/compute", Some(cookie), body);
    assert_eq!(r.status, 201, "{}", r.body);
    r.json()["id"].as_str().unwrap().to_string()
}

fn act(org: &Org, cookie: &str, id: &str, what: &str, body: Value) -> Resp {
    let path = format!("/api/org/v1/compute/{id}/{what}");
    let r = org.call("POST", &path, Some(cookie), body);
    assert_eq!(r.status, 200, "{}", r.body);
    r
}

/// Files and approves a request with the asked window.
fn approved(run: &Run, prompts: &[&str], budget: u64) -> String {
    let id = file(&run.org, &run.user, prompts, budget);
    act(&run.org, &run.admin, &id, "approve", json!({}));
    id
}

fn summary(org: &Org, cookie: &str, id: &str) -> Value {
    let r = org.get(&format!("/api/org/v1/compute/{id}"), Some(cookie));
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()
}

fn results(org: &Org, cookie: &str, id: &str) -> Vec<Value> {
    let r = org.get(&format!("/api/org/v1/compute/{id}/results"), Some(cookie));
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()["results"].as_array().unwrap().clone()
}

/// Polls `ok` every 20 ms until it holds or `secs` pass.
fn within(secs: u64, mut ok: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        if ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    ok()
}

/// Waits for the request to reach `status`, with a deadline a loaded
/// runner meets.
fn ends(run: &Run, id: &str, status: &str) -> Value {
    let mut last = Value::Null;
    let got = within(30, || {
        last = summary(&run.org, &run.admin, id);
        last["status"] == status
    });
    assert!(got, "never {status}: {last}");
    last
}

/// What the stub was sent, as each chat's user prompt.
fn sent(stub: &Stub) -> Vec<String> {
    let s = stub.state();
    s.seen
        .iter()
        .map(|x| {
            x.body["messages"][0]["content"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

/// Lets the scheduler run a few ticks while nothing should happen.
fn idle() {
    std::thread::sleep(Duration::from_millis(1500));
}

#[test]
fn case_2_nothing_is_sent_before_the_window_opens() {
    let run = ready();
    let id = approved(&run, &["one", "two", "three"], 5000);
    run.org.move_clock(Some(&at(T - 60)));
    idle();
    assert!(sent(&run.stub).is_empty());
    assert_eq!(summary(&run.org, &run.user, &id)["status"], "approved");
}

#[test]
fn case_3_the_window_opening_sends_each_prompt_alone_in_order() {
    let run = ready();
    let prompts = ["one", "two", "three"];
    let id = approved(&run, &prompts, 5000);
    run.org.move_clock(Some(&at(T)));
    assert!(within(2, || !sent(&run.stub).is_empty()), "not started");
    let done = ends(&run, &id, "done");
    assert_eq!(sent(&run.stub), prompts);
    assert_eq!(run.stub.state().max_open, 1);
    for (seen, p) in run.stub.state().seen.iter().zip(prompts) {
        assert_eq!(
            seen.body,
            json!({
                "model": "a",
                "messages": [{"role": "user", "content": p}],
                "stream": true,
                "stream_options": {"include_usage": true},
            })
        );
    }
    let got = results(&run.org, &run.user, &id);
    let rows: Vec<_> = got
        .iter()
        .map(|r| {
            (
                r["index"].clone(),
                r["prompt"].clone(),
                r["text"].clone(),
                r["status"].clone(),
            )
        })
        .collect();
    let want: Vec<_> = prompts
        .iter()
        .enumerate()
        .map(|(i, p)| (json!(i), json!(p), json!("one two"), json!(200)))
        .collect();
    assert_eq!(rows, want);
    assert_eq!(
        (&done["done"], &done["startedAt"]),
        (&json!(3), &json!(at(T)))
    );
    assert_eq!(done["endedAt"], at(T));
}

fn chat(org: &Org, cookie: &str) -> Resp {
    let body = json!({"model": "stub/a", "messages": [{"role": "user", "content": "hi"}]});
    org.call("POST", "/api/org/v1/chat/completions", Some(cookie), body)
}

fn limit(run: &Run, daily: u64) {
    let body = json!({
        "dailyTokens": daily,
        "hours": [{"days": [1, 2, 3, 4, 5], "start": "09:00", "end": "17:00"}],
    });
    let path = format!("/api/org/v1/members/{}/limits", run.user_id);
    let mut r = org_support::Req::new("PUT", &path, &run.org);
    r.headers.push(("X-Fonsviva-Request", "1".into()));
    r.headers.push(("Cookie", run.admin.clone()));
    r.body = Some(body.to_string());
    let r = run.org.send(r);
    assert_eq!(r.status, 200, "{}", r.body);
}

#[test]
fn case_4_hours_and_the_daily_limit_neither_hold_back_nor_count_a_run() {
    let run = ready();
    limit(&run, 30);
    run.org.move_clock(Some(&at(T - 12 * HOUR)));
    assert_eq!(chat(&run.org, &run.user).status, 200);
    assert_eq!(chat(&run.org, &run.user).status, 200);
    assert_eq!(chat(&run.org, &run.user).status, 429);
    let id = approved(&run, &["one", "two"], 5000);
    run.org.move_clock(Some(&at(T)));
    ends(&run, &id, "done");
    assert_eq!(sent(&run.stub).len(), 4);
    limit(&run, 40);
    run.org.move_clock(Some(&at(T + 12 * HOUR)));
    let me = run.org.get("/api/org/v1/me/limits", Some(&run.user));
    assert_eq!(me.json()["now"]["allowed"], true, "{}", me.body);
    let r = chat(&run.org, &run.user);
    assert_eq!(r.status, 200, "{}", r.body);
}

#[test]
fn case_5_it_runs_in_the_admins_window() {
    let run = ready();
    let id = file(&run.org, &run.user, &["one"], 5000);
    let (start, end) = (T + 2 * HOUR, T + 10 * HOUR);
    let body = json!({"start": at(start), "end": at(end), "maxTokens": 1200});
    act(&run.org, &run.admin, &id, "approve", body);
    run.org.move_clock(Some(&at(T + HOUR)));
    idle();
    assert!(sent(&run.stub).is_empty());
    run.org.move_clock(Some(&at(start)));
    assert!(within(2, || !sent(&run.stub).is_empty()), "not started");
    let done = ends(&run, &id, "done");
    let a = &done["approved"];
    assert_eq!(
        (&a["start"], &a["end"], &a["maxTokens"]),
        (&json!(at(start)), &json!(at(end)), &json!(1200))
    );
}

#[test]
fn case_7_the_budget_stops_the_run_after_the_prompt_that_reached_it() {
    let run = ready();
    {
        let mut s = run.stub.state();
        (s.prompt_tokens, s.completion_tokens) = (40, 40);
    }
    let id = approved(&run, &["1", "2", "3", "4", "5"], 100);
    run.org.move_clock(Some(&at(T)));
    let got = ends(&run, &id, "stopped");
    assert_eq!(
        (&got["reason"], &got["done"]),
        (&json!("budget"), &json!(2))
    );
    assert_eq!(sent(&run.stub), ["1", "2"]);
    assert_eq!(got["usage"]["promptTokens"], 80);
}

/// Starts a three prompt request whose second prompt stalls, and waits
/// for that prompt to be in flight.
fn stalled(run: &Run) -> String {
    run.stub.state().stall_on = Some(2);
    let id = approved(run, &["one", "two", "three"], 5000);
    run.org.move_clock(Some(&at(T)));
    assert!(
        within(10, || sent(&run.stub).len() == 2),
        "prompt 2 not sent"
    );
    id
}

#[test]
fn case_8_the_windows_end_cuts_the_prompt_in_flight() {
    let run = ready();
    let id = stalled(&run);
    run.org.move_clock(Some(&at(T + 8 * HOUR)));
    let moved = Instant::now();
    assert!(
        within(2, || run.stub.state().closed.is_some()),
        "upstream not dropped"
    );
    assert!(run.stub.state().closed.unwrap() - moved < Duration::from_secs(2));
    ends(&run, &id, "expired");
    let got = results(&run.org, &run.user, &id);
    let rows: Vec<_> = got
        .iter()
        .map(|r| (r["status"].clone(), r["text"].clone()))
        .collect();
    assert_eq!(
        rows,
        [(json!(200), json!("one two")), (json!(499), json!("one "))]
    );
    assert_eq!(sent(&run.stub).len(), 2);
}

#[test]
fn case_9_a_cancel_drops_the_prompt_in_flight_and_keeps_the_answers() {
    let run = ready();
    let id = stalled(&run);
    act(&run.org, &run.user, &id, "cancel", json!({}));
    assert!(
        within(2, || run.stub.state().closed.is_some()),
        "upstream not dropped"
    );
    assert_eq!(summary(&run.org, &run.user, &id)["status"], "cancelled");
    idle();
    let got = results(&run.org, &run.user, &id);
    assert_eq!(
        (&got[0]["index"], &got[0]["status"]),
        (&json!(0), &json!(200))
    );
    assert!(got.iter().all(|r| r["index"] != 2), "{got:?}");
    assert_eq!(sent(&run.stub).len(), 2);
}

#[test]
fn case_10_a_restart_inside_the_window_runs_the_stopped_prompt_again() {
    let mut run = ready();
    let id = stalled(&run);
    run.org
        .db()
        .execute(
            "INSERT INTO compute_results (compute_id, idx, text, status) VALUES (?1, 1, 'cut', 499)",
            [&id],
        )
        .unwrap();
    run.stub.state().stall_on = None;
    run.org.restart(org_support::RESOLVE);
    ends(&run, &id, "done");
    assert_eq!(sent(&run.stub), ["one", "two", "two", "three"]);
    let got = results(&run.org, &run.user, &id);
    let rows: Vec<_> = got
        .iter()
        .map(|r| (r["status"].clone(), r["text"].clone()))
        .collect();
    assert_eq!(rows, vec![(json!(200), json!("one two")); 3]);
}

#[test]
fn case_11_a_server_down_across_the_window_expires_it_on_start() {
    let mut run = ready();
    let id = approved(&run, &["one"], 5000);
    run.org.restart_at(&at(T + 9 * HOUR));
    ends(&run, &id, "expired");
    idle();
    assert!(sent(&run.stub).is_empty());
}

#[test]
fn case_12_a_source_error_on_one_prompt_is_its_status_and_the_run_goes_on() {
    let run = ready();
    run.stub.state().fail_on = Some((2, 500));
    let id = approved(&run, &["one", "two", "three"], 5000);
    run.org.move_clock(Some(&at(T)));
    ends(&run, &id, "done");
    let got = results(&run.org, &run.user, &id);
    let statuses: Vec<_> = got.iter().map(|r| r["status"].clone()).collect();
    assert_eq!(statuses, [json!(200), json!(500), json!(200)]);
    assert_eq!(sent(&run.stub).len(), 3);
}

#[test]
fn case_13_a_model_taken_away_or_a_disabled_member_fails_it_unsent() {
    let run = ready();
    let id = approved(&run, &["one"], 5000);
    let path = format!("/api/org/v1/members/{}/limits", run.user_id);
    let mut r = org_support::Req::new("PUT", &path, &run.org);
    r.headers.push(("X-Fonsviva-Request", "1".into()));
    r.headers.push(("Cookie", run.admin.clone()));
    r.body = Some(json!({"models": ["stub/b"]}).to_string());
    assert_eq!(run.org.send(r).status, 200);
    let (other, other_id) = member(&run.org, &run.admin, "user", "Cy");
    let gone = file(&run.org, &other, &["two"], 5000);
    act(&run.org, &run.admin, &gone, "approve", json!({}));
    let path = format!("/api/org/v1/members/{other_id}");
    let r = run
        .org
        .call("PATCH", &path, Some(&run.admin), json!({"disabled": true}));
    assert_eq!(r.status, 200, "{}", r.body);
    let got = summary(&run.org, &run.admin, &gone);
    assert_eq!(
        (&got["status"], &got["reason"]),
        (&json!("failed"), &json!("member_disabled"))
    );
    run.org.move_clock(Some(&at(T)));
    let got = ends(&run, &id, "failed");
    assert_eq!(got["reason"], "not_allowed");
    idle();
    assert!(sent(&run.stub).is_empty());
}

#[test]
fn case_14_one_runs_at_a_time_per_member_and_four_in_the_org() {
    let run = ready();
    run.stub.state().hold = true;
    let mut ids = vec![
        approved(&run, &["bo one"], 5000),
        approved(&run, &["bo two"], 5000),
    ];
    for name in ["Cy", "Di", "Ed", "Fa"] {
        let (c, _) = member(&run.org, &run.admin, "user", name);
        let id = file(&run.org, &c, &[name], 5000);
        act(&run.org, &run.admin, &id, "approve", json!({}));
        ids.push(id);
    }
    run.org.move_clock(Some(&at(T)));
    assert!(within(5, || run.stub.state().open == 4), "four not running");
    idle();
    let statuses: Vec<_> = ids
        .iter()
        .map(|id| summary(&run.org, &run.admin, id)["status"].clone())
        .collect();
    let running = statuses.iter().filter(|s| *s == "running").count();
    assert_eq!(running, 4, "{statuses:?}");
    assert_ne!(
        (&statuses[0], &statuses[1]),
        (&json!("running"), &json!("running"))
    );
    assert_eq!(run.stub.state().max_open, 4);
    run.stub.state().released = true;
    for id in &ids {
        ends(&run, id, "done");
    }
    assert_eq!(run.stub.state().max_open, 4);
    let bo: Vec<_> = sent(&run.stub)
        .into_iter()
        .filter(|p| p.starts_with("bo"))
        .collect();
    assert_eq!(bo, ["bo one", "bo two"]);
}

#[test]
fn case_18_each_prompt_writes_one_costed_usage_row_with_its_request() {
    let run = ready();
    let pricing = json!({"pricing": {"promptPerMillion": "2", "completionPerMillion": "8"}});
    let path = format!("/api/org/v1/sources/{}", run.source);
    assert_eq!(
        run.org
            .call("PATCH", &path, Some(&run.admin), pricing)
            .status,
        200
    );
    let id = approved(&run, &["one", "two", "three"], 5000);
    run.org.move_clock(Some(&at(T)));
    let done = ends(&run, &id, "done");
    let rows: Vec<(String, i64, i64, i64, bool)> = {
        let db = run.org.db();
        let mut q = db
            .prepare(
                "SELECT compute_id, prompt_tokens, completion_tokens, cost_nanos, cost_known
                 FROM usage ORDER BY seq",
            )
            .unwrap();
        q.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    };
    let each = 11 * 2_000 + 7 * 8_000;
    assert_eq!(rows, vec![(id.clone(), 11, 7, each, true); 3]);
    let got = results(&run.org, &run.user, &id);
    assert!(
        got.iter()
            .all(|r| r["costNanos"] == json!(each.to_string())),
        "{got:?}"
    );
    assert_eq!(
        done["usage"],
        json!({"promptTokens": 33, "completionTokens": 21, "costNanos": (3 * each).to_string(), "costKnown": true})
    );
    let r = run.org.get(
        "/api/org/v1/usage/totals?days=1&by=member",
        Some(&run.admin),
    );
    let total = &r.json()["total"];
    assert_eq!(
        (
            &total["requests"],
            &total["promptTokens"],
            &total["costNanos"]
        ),
        (&json!(3), &json!(33), &json!((3 * each).to_string())),
        "{}",
        r.body
    );
}

#[test]
fn case_20_a_prompt_marker_reaches_no_usage_audit_or_log() {
    let mut run = ready();
    let id = approved(&run, &["zqmark-one", "zqmark-two"], 5000);
    run.org.move_clock(Some(&at(T)));
    ends(&run, &id, "done");
    let audit = run.org.get("/api/org/v1/audit", Some(&run.admin)).body;
    let usage = run.org.get("/api/org/v1/usage", Some(&run.admin)).body;
    let mut stored = String::new();
    for table in ["usage", "audit"] {
        let db = run.org.db();
        let mut q = db.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let n = q.column_count();
        let mut rows = q.query([]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            for i in 0..n {
                let v: rusqlite::types::Value = r.get(i).unwrap();
                stored.push_str(&format!("{v:?}\n"));
            }
        }
    }
    let log = run.org.stop();
    for (what, text) in [
        ("audit", &audit),
        ("usage", &usage),
        ("rows", &stored),
        ("log", &log),
    ] {
        assert!(!text.contains("zqmark"), "a prompt in the {what}: {text}");
    }
}
