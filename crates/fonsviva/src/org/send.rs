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
use super::store::{MARK_CLOSE, MARK_OPEN, TEXT_LIMIT};

/// Earlier messages sent with a new one at most, newest kept.
const CONTEXT_MESSAGES: usize = 100;
const CONTEXT_BYTES: usize = 200 << 10;
/// The longest stream line read for its delta.
const LINE_LIMIT: usize = 1 << 20;

type Event = Result<Bytes, std::io::Error>;

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

/// The newest of `earlier` (oldest first) within the context limits,
/// oldest first. A message with the role of the newer one kept after it,
/// the new user message first, is left out, so roles alternate.
fn newest(earlier: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    let mut bytes = 0;
    for (role, text) in earlier.into_iter().rev() {
        if role == out.last().map_or("user", |m| m.0.as_str()) {
            continue;
        }
        if out.len() == CONTEXT_MESSAGES || bytes + text.len() > CONTEXT_BYTES {
            break;
        }
        bytes += text.len();
        out.push((role, text));
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
/// is still there; `None` when it holds no text or isn't kept.
fn append(
    st: &St,
    member: &str,
    role: &str,
    text: &str,
    model: &str,
    (status, after): (Option<u16>, Option<i64>),
) -> anyhow::Result<Option<i64>> {
    let store = st.store();
    if !store.settings()?.keep_threads {
        return Ok(None);
    }
    if let Some(seq) = after
        && !store.has_message(member, seq)?
    {
        return Ok(None);
    }
    Ok(store
        .append_message(member, role, text, model, status)?
        .ok())
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
        match st
            .store()
            .thread(&member, None, 2 * CONTEXT_MESSAGES as i64)
        {
            Ok((rows, _)) => rows.into_iter().rev().map(|m| (m.role, m.text)).collect(),
            Err(e) => return internal(e),
        }
    } else {
        match history(req.get("history")) {
            Some(h) => h,
            None => return bad(),
        }
    };
    let mut messages: Vec<Value> = newest(earlier)
        .into_iter()
        .map(|(role, text)| json!({"role": role, "content": text}))
        .collect();
    messages.push(json!({"role": "user", "content": clean}));
    let up = json!({"model": model, "messages": messages, "stream": true});
    let (mut stored, mut user) = (false, None);
    let mut store_user = || {
        if stored {
            return Ok(());
        }
        user = append(&st, &member, "user", text, model, (None, None)).map_err(|e| {
            eprintln!("fonsviva org: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal")
        })?;
        stored = true;
        Ok(())
    };
    let answer = match proxy(
        &st,
        &c,
        Ok(Bytes::from(up.to_string())),
        Some(&mut store_user),
    )
    .await
    {
        Ok(r) => Ok(r),
        Err((s, code)) if stored => Err((s.as_u16(), code)),
        Err((s, code)) => return retry_after(code, err(s, code)),
    };
    let model = model.to_string();
    let (tx, rx) = mpsc::channel::<Event>(32);
    tokio::spawn(async move {
        let _turn = turn;
        let _ = tx.send(event(json!({"type": "start", "seq": user}))).await;
        let (status, reply, code) = match answer {
            Err((s, code)) => (s, String::new(), Some(code)),
            Ok(r) if !r.status().is_success() => {
                (r.status().as_u16(), String::new(), Some("upstream"))
            }
            Ok(r) => {
                let (s, reply) = relay(r, &tx).await;
                (s, reply, (s == UPSTREAM_DOWN).then_some("upstream"))
            }
        };
        let seq = user.and_then(|u| {
            let ended = (Some(status), Some(u));
            append(&st, &member, "assistant", &reply, &model, ended).unwrap_or_else(|e| {
                eprintln!("fonsviva org: {e:#}");
                None
            })
        });
        let mut done = json!({"type": "done", "seq": seq, "status": status});
        if let Some(code) = code {
            done["error"] = code.into();
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
async fn relay(r: Response, tx: &mpsc::Sender<Event>) -> (u16, String) {
    let status = r.status().as_u16();
    let mut up = r.into_body().into_data_stream();
    let (mut lines, mut reply) = (Lines::default(), String::new());
    let end = 'read: loop {
        let next = tokio::select! {
            _ = tx.closed() => break CLIENT_GONE,
            c = up.next() => c,
        };
        let chunk = match next {
            None => {
                if let Some(l) = lines.rest()
                    && !pass(&l, &mut reply, tx).await
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
                && !pass(&l, &mut reply, tx).await
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
    (end, reply)
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

/// Relays a line's delta and adds it to the reply; false when the client
/// left.
async fn pass(line: &[u8], reply: &mut String, tx: &mpsc::Sender<Event>) -> bool {
    let Some(t) = delta(line) else {
        return true;
    };
    let mut room = TEXT_LIMIT.saturating_sub(reply.len()).min(t.len());
    while !t.is_char_boundary(room) {
        room -= 1;
    }
    reply.push_str(&t[..room]);
    tx.send(event(json!({"type": "delta", "text": t})))
        .await
        .is_ok()
}

/// The content of a stream line's first choice delta.
fn delta(line: &[u8]) -> Option<String> {
    let data = line.strip_prefix(b"data:")?;
    let v: Value = serde_json::from_slice(data.trim_ascii()).ok()?;
    let t = v["choices"][0]["delta"]["content"].as_str()?;
    (!t.is_empty()).then(|| t.to_string())
}
