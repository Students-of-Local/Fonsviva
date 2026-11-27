//! `POST /thread/messages`: a member's message goes to a model with the
//! thread before it, through the chat proxy, and the reply streams back as
//! events and joins the thread (docs/ARCHITECTURE.md, Threads).

use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::extract::{Extension, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::chat::{CLIENT_GONE, UPSTREAM_DOWN};
use super::http::{Caller, St, body, err, internal, proxy, retry_after};
use super::store::{MARK_CLOSE, MARK_OPEN, Step, TEXT_LIMIT};
use super::tools::{self, Outcome, Turn};

/// Earlier messages sent with a new one at most, newest kept.
const CONTEXT_MESSAGES: usize = 100;
const CONTEXT_BYTES: usize = 200 << 10;
/// The longest stream line read for its delta.
const LINE_LIMIT: usize = 1 << 20;
/// Model calls one send with tools makes at most; the last offers none.
const MAX_STEPS: usize = 8;
/// Tool calls run in a step; later ones are answered `too_many_calls`.
const CALLS_RUN: usize = 4;
/// Tool calls a step's stream is read for at most, and the longest id or
/// name and the most args bytes in all.
const CALL_SLOTS: usize = 64;
const NAME_LIMIT: usize = 200;
const ARGS_LIMIT: usize = 8 << 20;
/// What joins the texts of a reply's steps.
const GAP: &str = "\n\n";

pub(super) type Event = Result<Bytes, std::io::Error>;

/// A member's one send in progress, given back when dropped.
struct Busy {
    st: St,
    member: String,
}

impl Drop for Busy {
    fn drop(&mut self) {
        let mut s = self.st.sending.lock().unwrap_or_else(|e| e.into_inner());
        s.remove(&self.member);
    }
}

fn take_turn(st: &St, member: &str) -> Option<Busy> {
    let mut s = st.sending.lock().unwrap_or_else(|e| e.into_inner());
    s.insert(member.to_string()).then(|| Busy {
        st: st.clone(),
        member: member.to_string(),
    })
}

/// What the context keeps of earlier messages seen newest first. A message
/// with the role of the newer one kept after it, the new user message
/// first, is skipped, so roles alternate, and the context ends at
/// `CONTEXT_MESSAGES` messages or `CONTEXT_BYTES` of text.
struct Budget {
    last: String,
    messages: usize,
    bytes: usize,
}

impl Budget {
    fn new() -> Budget {
        Budget {
            last: "user".into(),
            messages: 0,
            bytes: 0,
        }
    }

    /// Whether to keep a message of this role and size, or `None` once the
    /// context is full.
    fn take(&mut self, role: &str, bytes: usize) -> Option<bool> {
        if role == self.last {
            return Some(false);
        }
        if self.messages == CONTEXT_MESSAGES || self.bytes + bytes > CONTEXT_BYTES {
            return None;
        }
        self.last = role.to_string();
        self.messages += 1;
        self.bytes += bytes;
        Some(true)
    }
}

/// The newest of `earlier` (oldest first) that the budget keeps, oldest
/// first.
fn newest(earlier: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut budget = Budget::new();
    let mut out = vec![];
    for (role, text) in earlier.into_iter().rev() {
        match budget.take(&role, text.len()) {
            None => break,
            Some(true) => out.push((role, text)),
            Some(false) => {}
        }
    }
    out.reverse();
    out
}

/// The `history` a client sends while threads aren't kept, or `None` when
/// it isn't a list of `{role, text}` with a user or assistant role.
fn history(v: Option<&Value>) -> Option<Vec<(String, String)>> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Some(vec![]);
    };
    v.as_array()?
        .iter()
        .map(|m| {
            let role = m["role"]
                .as_str()
                .filter(|r| ["user", "assistant"].contains(r))?;
            Some((role.to_string(), m["text"].as_str()?.to_string()))
        })
        .collect()
}

fn event(v: Value) -> Event {
    Ok(Bytes::from(format!("data: {v}\n\n")))
}

/// Appends a message to the member's thread while the org keeps threads,
/// read again at each write, and, for a reply, while the message it answers
/// is still there, committed with its tool calls and the artifacts its send
/// made; `None` when it holds nothing to keep or isn't kept.
fn append(
    st: &St,
    member: &str,
    role: &str,
    text: &str,
    model: &str,
    (status, after): (Option<u16>, Option<i64>),
    (steps, made): (&[Step], &[String]),
) -> anyhow::Result<Option<i64>> {
    let mut store = st.store();
    if !store.settings()?.keep_threads {
        return Ok(None);
    }
    match (status, after) {
        (Some(status), Some(after)) => {
            store.append_reply(member, after, (text, model, status), (steps, made))
        }
        _ => Ok(store
            .append_message(member, role, text, model, status)?
            .ok()),
    }
}

pub async fn send(
    State(st): State<St>,
    Extension(c): Extension<Caller>,
    b: Result<Bytes, BytesRejection>,
) -> Response {
    let b = match b {
        Ok(b) => b,
        Err(r) if r.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return err(StatusCode::PAYLOAD_TOO_LARGE, "too_large");
        }
        Err(_) => return err(StatusCode::BAD_REQUEST, "bad_request"),
    };
    let bad = || err(StatusCode::BAD_REQUEST, "bad_request");
    let Some(req) = body::<Value>(&b) else {
        return bad();
    };
    let (Some(text), Some(model)) = (req["text"].as_str(), req["model"].as_str()) else {
        return bad();
    };
    let clean = text.replace([MARK_OPEN, MARK_CLOSE], "");
    if clean.trim().is_empty() {
        return bad();
    }
    if clean.len() > TEXT_LIMIT {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "too_large");
    }
    let member = c.member.id.clone();
    let Some(turn) = take_turn(&st, &member) else {
        return err(StatusCode::CONFLICT, "busy");
    };
    let keep = match st.store().settings() {
        Ok(s) => s.keep_threads,
        Err(e) => return internal(e),
    };
    let earlier = if keep {
        let mut budget = Budget::new();
        match st.store().context(&member, |role, n| budget.take(role, n)) {
            Ok(rows) => rows,
            Err(e) => return internal(e),
        }
    } else {
        match history(req.get("history")) {
            Some(h) => newest(h),
            None => return bad(),
        }
    };
    let mut messages: Vec<Value> = earlier
        .into_iter()
        .map(|(role, text)| json!({"role": role, "content": text}))
        .collect();
    messages.push(json!({"role": "user", "content": clean}));
    let tools = match req.get("tools") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(on)) => on.then(|| st.store().settings()),
        Some(_) => return bad(),
    };
    let mut tools = match tools.transpose() {
        Ok(t) => t.map(|s| Turn::new(st.clone(), member.clone(), tools::offered(&s))),
        Err(e) => return internal(e),
    };
    let up = step(model, &messages, tools.as_ref().map(Turn::specs));
    let (mut stored, mut user) = (false, None);
    let mut store_user = || {
        if stored {
            return Ok(());
        }
        user =
            append(&st, &member, "user", text, model, (None, None), (&[], &[])).map_err(|e| {
                eprintln!("fonsviva org: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            })?;
        stored = true;
        Ok(())
    };
    let answer = match proxy(&st, &c, Ok(up), Some(&mut store_user), None).await {
        Ok(r) => Ok(r),
        Err((s, code)) if stored => Err((s.as_u16(), code)),
        Err((s, code)) => return retry_after(&st, &c.member, code, err(s, code)),
    };
    let model = model.to_string();
    let (tx, rx) = mpsc::channel::<Event>(32);
    tokio::spawn(async move {
        let _turn = turn;
        let _ = tx.send(event(json!({"type": "start", "seq": user}))).await;
        let (mut answer, mut n, mut reply, mut steps) = (answer, 1, String::new(), vec![]);
        let mut made_calls = 0;
        let (status, code) = loop {
            let r = match answer {
                Err((s, code)) => break (s, Some(code)),
                Ok(r) => r,
            };
            made_calls += 1;
            if !r.status().is_success() {
                break (r.status().as_u16(), Some("upstream"));
            }
            let owed = !reply.is_empty();
            let (mut gap, from) = (owed, reply.len());
            let mut calls = tools.as_ref().map(|_| Calls::default());
            let s = read(r, &tx, (&mut reply, &mut gap), calls.as_mut()).await;
            let from = if owed && !gap { from + GAP.len() } else { from };
            let calls = calls.map(Calls::done).unwrap_or_default();
            if s == CLIENT_GONE || s == UPSTREAM_DOWN || calls.is_empty() || n == MAX_STEPS {
                break (s, (s == UPSTREAM_DOWN).then_some("upstream"));
            }
            let Some(turn) = tools.as_mut() else {
                break (s, None);
            };
            let text = &reply[from..];
            messages.push(json!({
                "role": "assistant",
                "content": if text.is_empty() { Value::Null } else { text.into() },
                "tool_calls": calls.iter().map(|c| json!({
                    "id": c.id, "type": "function",
                    "function": {"name": c.name, "arguments": if c.over { "{}" } else { &c.args }},
                })).collect::<Vec<_>>(),
            }));
            for (i, c) in calls.iter().enumerate() {
                let args = tools::shown(&c.name, &c.args);
                let tool = json!({"type": "tool", "step": n, "name": c.name, "args": args});
                let _ = tx.send(event(tool)).await;
                let out = match (i < CALLS_RUN, c.over) {
                    (false, _) => Outcome::error("too_many_calls"),
                    (true, true) => Outcome::error("too_large"),
                    (true, false) => turn.call(&c.name, &c.args).await,
                };
                let mut ended =
                    json!({"type": "tool_result", "step": n, "name": c.name, "ok": out.ok()});
                if let Some(e) = out.result.get("error") {
                    ended["error"] = e.clone();
                }
                let _ = tx.send(event(ended)).await;
                if let Some(e) = out.event.clone() {
                    let _ = tx.send(event(e)).await;
                }
                let result = out.result.to_string();
                messages.push(json!({"role": "tool", "tool_call_id": c.id, "content": result}));
                let result = if result.len() > tools::RESULT_LIMIT {
                    json!({"error": "too_large"}).to_string()
                } else {
                    result
                };
                let (name, args) = (c.name.clone(), args.to_string());
                steps.push(Step {
                    name,
                    args,
                    ok: out.ok(),
                    result,
                });
            }
            if tx.is_closed() {
                break (CLIENT_GONE, None);
            }
            n += 1;
            let offer = (n < MAX_STEPS).then(|| turn.specs());
            let up = step(&model, &messages, offer);
            answer = proxy(&st, &c, Ok(up), None, None)
                .await
                .map_err(|(s, code)| (s.as_u16(), code));
        };
        let on = tools.is_some();
        let made = tools.map(|t| t.made).unwrap_or_default();
        let seq = user.and_then(|u| {
            let ended = (Some(status), Some(u));
            let kept = (&steps[..], &made[..]);
            append(&st, &member, "assistant", &reply, &model, ended, kept).unwrap_or_else(|e| {
                eprintln!("fonsviva org: {e:#}");
                None
            })
        });
        let mut done = json!({"type": "done", "seq": seq, "status": status});
        if let Some(code) = code {
            done["error"] = code.into();
        }
        if on {
            done["steps"] = made_calls.into();
        }
        let _ = tx.send(event(done)).await;
    });
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
}

/// Passes the reply's deltas on as they arrive and returns its status and
/// text: 499 when the client left, which drops the upstream answer, and
/// 502 when the source cut it.
pub(super) async fn relay(r: Response, tx: &mpsc::Sender<Event>) -> (u16, String) {
    let mut reply = String::new();
    let status = read(r, tx, (&mut reply, &mut false), None).await;
    (status, reply)
}

/// One step's upstream body: the thread so far and, when offered, tools.
fn step(model: &str, messages: &[Value], tools: Option<Value>) -> Bytes {
    let mut up = json!({"model": model, "messages": messages, "stream": true});
    if let Some(t) = tools {
        up["tools"] = t;
    }
    Bytes::from(up.to_string())
}

/// Passes a step's deltas on as they arrive, adds its text to `reply`,
/// after `GAP` when `gap` asks and the step has text, and its tool calls to
/// `calls`, and returns its status: 499 when the client left, which drops
/// the upstream answer, and 502 when the source cut it.
async fn read(
    r: Response,
    tx: &mpsc::Sender<Event>,
    (reply, gap): (&mut String, &mut bool),
    mut calls: Option<&mut Calls>,
) -> u16 {
    let status = r.status().as_u16();
    let mut up = r.into_body().into_data_stream();
    let mut lines = Lines::default();
    let end = 'read: loop {
        let next = tokio::select! {
            _ = tx.closed() => break CLIENT_GONE,
            c = up.next() => c,
        };
        let chunk = match next {
            None => {
                if let Some(l) = lines.rest()
                    && !pass(&l, (reply, gap), tx, calls.as_deref_mut()).await
                {
                    break CLIENT_GONE;
                }
                break status;
            }
            Some(Err(_)) => break UPSTREAM_DOWN,
            Some(Ok(c)) => c,
        };
        for &b in chunk.iter() {
            if let Some(l) = lines.push(b)
                && !pass(&l, (reply, gap), tx, calls.as_deref_mut()).await
            {
                break 'read CLIENT_GONE;
            }
        }
    };
    if lines.dropped > 0 {
        eprintln!(
            "fonsviva org: a thread reply line over 1 MiB was not relayed ({} bytes)",
            lines.dropped
        );
    }
    end
}

/// A stream's lines; one over `LINE_LIMIT` is dropped and its bytes counted.
#[derive(Default)]
struct Lines {
    buf: Vec<u8>,
    over: bool,
    dropped: u64,
}

impl Lines {
    /// The line `b` ends, unless it was dropped.
    fn push(&mut self, b: u8) -> Option<Vec<u8>> {
        if b == b'\n' {
            let over = std::mem::replace(&mut self.over, false);
            let line = std::mem::take(&mut self.buf);
            return (!over).then_some(line);
        }
        if self.over {
            self.dropped += 1;
        } else if self.buf.len() < LINE_LIMIT {
            self.buf.push(b);
        } else {
            self.over = true;
            self.dropped += self.buf.len() as u64 + 1;
            self.buf = Vec::new();
        }
        None
    }

    /// The last line, when the stream ended without a newline.
    fn rest(&mut self) -> Option<Vec<u8>> {
        (!self.over && !self.buf.is_empty()).then(|| std::mem::take(&mut self.buf))
    }
}

/// Relays a line's delta, adds it to the reply, sending and adding `GAP`
/// first when `gap` is still owed, and its tool call parts to `calls`;
/// false when the client left.
async fn pass(
    line: &[u8],
    (reply, gap): (&mut String, &mut bool),
    tx: &mpsc::Sender<Event>,
    calls: Option<&mut Calls>,
) -> bool {
    let Some(d) = delta(line) else {
        return true;
    };
    if let Some(c) = calls {
        c.add(&d["tool_calls"]);
    }
    let Some(t) = d["content"].as_str().filter(|t| !t.is_empty()) else {
        return true;
    };
    if *gap && reply.len() + GAP.len() < TEXT_LIMIT {
        *gap = false;
        reply.push_str(GAP);
        let sent = tx.send(event(json!({"type": "delta", "text": GAP})));
        if sent.await.is_err() {
            return false;
        }
    }
    let mut room = TEXT_LIMIT.saturating_sub(reply.len()).min(t.len());
    while !t.is_char_boundary(room) {
        room -= 1;
    }
    reply.push_str(&t[..room]);
    tx.send(event(json!({"type": "delta", "text": t})))
        .await
        .is_ok()
}

/// A stream line's first choice delta.
fn delta(line: &[u8]) -> Option<Value> {
    let data = line.strip_prefix(b"data:")?;
    let mut v: Value = serde_json::from_slice(data.trim_ascii()).ok()?;
    v.pointer_mut("/choices/0/delta").map(Value::take)
}

/// A tool call joined from its stream parts.
#[derive(Default)]
struct Call {
    index: u64,
    id: String,
    name: String,
    args: String,
    /// Its args passed `ARGS_LIMIT`.
    over: bool,
}

/// A step's tool calls, joined by index, or by place in their delta when
/// they have none, as their parts arrive: a call's id and name come from
/// its first part that has them, and its args from every part. A part with
/// no index whose id differs from its call's starts a new call, which later
/// parts at that place join.
#[derive(Default)]
struct Calls {
    list: Vec<Call>,
    bytes: usize,
    places: std::collections::HashMap<usize, u64>,
}

impl Calls {
    fn add(&mut self, parts: &Value) {
        for (at, p) in parts.as_array().into_iter().flatten().enumerate() {
            let index = match p["index"].as_u64() {
                Some(i) => i,
                None => self.place(at, p["id"].as_str()),
            };
            let at = match self.list.iter().position(|c| c.index == index) {
                Some(at) => at,
                None if self.list.len() < CALL_SLOTS => {
                    self.list.push(Call {
                        index,
                        ..Call::default()
                    });
                    self.list.len() - 1
                }
                None => continue,
            };
            let c = &mut self.list[at];
            let f = &p["function"];
            for (to, part) in [(&mut c.id, &p["id"]), (&mut c.name, &f["name"])] {
                if let Some(s) = part.as_str()
                    && to.is_empty()
                    && s.len() <= NAME_LIMIT
                {
                    *to = s.to_string();
                }
            }
            if let Some(a) = f["arguments"].as_str() {
                if self.bytes + a.len() > ARGS_LIMIT {
                    c.over = true;
                } else if !c.over {
                    self.bytes += a.len();
                    c.args.push_str(a);
                }
            }
        }
    }

    /// The index of a part with none at place `at` in its delta.
    fn place(&mut self, at: usize, id: Option<&str>) -> u64 {
        let index = *self.places.entry(at).or_insert(at as u64);
        let new = self
            .list
            .iter()
            .find(|c| c.index == index)
            .is_some_and(|c| id.is_some_and(|id| !c.id.is_empty() && c.id != id));
        if !new {
            return index;
        }
        let next = self.list.iter().map(|c| c.index + 1).max().unwrap_or(0);
        self.places.insert(at, next);
        next
    }

    /// The calls in index order, each with an id.
    fn done(mut self) -> Vec<Call> {
        self.list.sort_by_key(|c| c.index);
        for c in &mut self.list {
            if c.id.is_empty() {
                c.id = format!("call_{}", c.index);
            }
        }
        self.list
    }
}
