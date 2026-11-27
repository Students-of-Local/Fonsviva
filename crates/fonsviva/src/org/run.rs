//! The compute runner: a scheduler task starts approved requests in their
//! window, and each runs its prompts in order through the chat proxy
//! (docs/ARCHITECTURE.md, Compute requests).

use anyhow::{Context, Result};
use axum::body::Bytes;
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use super::chat::CLIENT_GONE;
use super::http::{Caller, Run, St, may_chat, offers, proxy};
use super::limits;
use super::send::relay;
use super::store::{Answer, ComputeRow};

/// How often the scheduler looks for requests to start and a runner checks
/// for a cancel or its window's end.
const TICK: Duration = Duration::from_secs(1);
/// Requests running at once in the org.
const ORG_RUNNING: usize = 4;

fn tick() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("FONSVIVA_TEST_TICK_MS")
        .ok()
        .and_then(|t| t.parse().ok())
    {
        return Duration::from_millis(ms);
    }
    TICK
}

fn clock() -> i64 {
    limits::now().as_second()
}

pub async fn schedule(st: St) {
    let mut every = tokio::time::interval(tick());
    loop {
        every.tick().await;
        if let Err(e) = start_due(&st) {
            eprintln!("fonsviva org: compute: {e:#}");
        }
    }
}

/// Ends what is past its window, resumes what a stop left running, then
/// starts approved requests in start order while their member has none
/// running and the org has room.
fn start_due(st: &St) -> Result<()> {
    let now = clock();
    let due = st.store().compute_due()?;
    let mut runs = st.runs.lock().unwrap_or_else(|e| e.into_inner());
    let mut busy: HashSet<&str> = HashSet::new();
    for d in due.iter().filter(|d| d.running) {
        busy.insert(&d.member);
    }
    let alive: Vec<String> = runs.values().cloned().collect();
    busy.extend(alive.iter().map(String::as_str));
    for d in &due {
        if runs.contains_key(&d.id) {
            continue;
        }
        if d.end <= now {
            st.store().end_compute(&d.id, "expired", None, now)?;
            continue;
        }
        if !d.running {
            let room = busy.len() < ORG_RUNNING && !busy.contains(d.member.as_str());
            if d.start > now || !room || !st.store().start_compute(&d.id, now)? {
                continue;
            }
            busy.insert(&d.member);
        }
        runs.insert(d.id.clone(), d.member.clone());
        tokio::spawn(run(st.clone(), d.id.clone()));
    }
    Ok(())
}

async fn run(st: St, id: String) {
    if let Err(e) = steps(&st, &id).await {
        eprintln!("fonsviva org: compute {id}: {e:#}");
    }
    let mut runs = st.runs.lock().unwrap_or_else(|e| e.into_inner());
    runs.remove(&id);
}

/// Why a prompt in flight was dropped.
#[derive(PartialEq)]
enum Cut {
    End,
    Cancel,
}

async fn steps(st: &St, id: &str) -> Result<()> {
    loop {
        let Some(c) = st.store().compute(id)? else {
            return Ok(());
        };
        if c.status != "running" {
            return Ok(());
        }
        let a = c
            .approved
            .as_ref()
            .context("a running request is approved")?;
        let now = clock();
        let end =
            |status: &str, reason: Option<&str>| st.store().end_compute(id, status, reason, now);
        if now >= a.end {
            end("expired", None)?;
            return Ok(());
        }
        let Some((idx, prompt)) = st.store().compute_next(id)? else {
            end("done", None)?;
            return Ok(());
        };
        if c.usage.prompt_tokens + c.usage.completion_tokens >= a.budget {
            end("stopped", Some("budget"))?;
            return Ok(());
        }
        if let Some(reason) = refused(st, &c).await? {
            end("failed", Some(reason))?;
            return Ok(());
        }
        let (status, text, spent, cut) = match ask(st, &c, a.end, &prompt).await? {
            Ok(got) => got,
            Err(reason) => {
                end("failed", Some(reason))?;
                return Ok(());
            }
        };
        let (prompt_tokens, completion_tokens, cost_nanos) = spent.unwrap_or((None, None, 0));
        let answer = Answer {
            text: &text,
            status,
            prompt_tokens,
            completion_tokens,
            cost_nanos,
        };
        st.store().put_answer(id, idx, &answer)?;
        if cut == Some(Cut::End) {
            st.store().end_compute(id, "expired", None, clock())?;
        }
        if cut.is_some() {
            return Ok(());
        }
    }
}

/// Why the member may no longer run the request, checked before each
/// prompt.
async fn refused(st: &St, c: &ComputeRow) -> Result<Option<&'static str>> {
    let Some((m, false)) = st.store().member_state(&c.member)? else {
        return Ok(Some("member_disabled"));
    };
    let (lim, _) = limits::of(st, &m)?;
    if !may_chat(st, &m)? || lim.models.as_ref().is_some_and(|ok| !ok.contains(&c.model)) {
        return Ok(Some("not_allowed"));
    }
    if offers(st, &c.model).await? == Some(false) {
        return Ok(Some("model_gone"));
    }
    Ok(None)
}

type Asked = (u16, String, Option<super::http::Spent>, Option<Cut>);

/// Sends one prompt and reads its reply, dropping it at the window's end
/// or a cancel. `Err` is the reason the request fails.
async fn ask(
    st: &St,
    c: &ComputeRow,
    until: i64,
    prompt: &str,
) -> Result<std::result::Result<Asked, &'static str>> {
    let Some((m, _)) = st.store().member_state(&c.member)? else {
        return Ok(Err("member_disabled"));
    };
    let (to, spent) = oneshot::channel();
    let run = Run {
        id: c.id.clone(),
        spent: Arc::new(Mutex::new(Some(to))),
    };
    let body = json!({
        "model": c.model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    let caller = Caller::of(m);
    let (tx, mut rx) = mpsc::channel(32);
    let answer = async {
        let sent = proxy(
            st,
            &caller,
            Ok(Bytes::from(body.to_string())),
            None,
            Some(run),
        )
        .await;
        match sent {
            Ok(r) => Ok(relay(r, &tx).await),
            Err((status, code)) => Err((status.as_u16(), code)),
        }
    };
    let mut every = tokio::time::interval(tick());
    let mut cut = None;
    let got = {
        tokio::pin!(answer);
        loop {
            tokio::select! {
                got = &mut answer => break Some(got),
                _ = rx.recv() => {}
                _ = every.tick() => {
                    cut = stop(st, &c.id, until)?;
                    if cut.is_some() {
                        rx.close();
                        break tokio::time::timeout(tick(), &mut answer).await.ok();
                    }
                }
            }
        }
    };
    let (status, text) = match got {
        None => (CLIENT_GONE, String::new()),
        Some(Ok(read)) => read,
        Some(Err((_, "unknown_model"))) => return Ok(Err("model_gone")),
        Some(Err((_, "students_off" | "model_not_allowed"))) => return Ok(Err("not_allowed")),
        Some(Err((status, _))) => (status, String::new()),
    };
    Ok(Ok((status, text, spent.await.ok(), cut)))
}

/// Whether the prompt in flight is to be dropped: the window ended, or the
/// request is no longer running.
fn stop(st: &St, id: &str, until: i64) -> Result<Option<Cut>> {
    if clock() >= until {
        return Ok(Some(Cut::End));
    }
    let running = st
        .store()
        .compute(id)?
        .is_some_and(|c| c.status == "running");
    Ok((!running).then_some(Cut::Cancel))
}
