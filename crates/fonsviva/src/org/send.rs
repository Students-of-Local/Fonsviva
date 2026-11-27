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
/// oldest first.
fn newest(earlier: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut bytes = 0;
    for (role, text) in earlier.into_iter().rev() {
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

/// Appends a message to the member's thread; `None` when it holds no text.
fn append(
    st: &St,
    member: &str,
    role: &str,
    text: &str,
    model: &str,
    status: Option<u16>,
) -> anyhow::Result<Option<i64>> {
    Ok(st
        .store()
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
        match st.store().thread(&member, None, CONTEXT_MESSAGES as i64) {
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
    messages.push(json!({"role": "user", "content": text}));
    let up = json!({"model": model, "messages": messages, "stream": true});
    let (mut stored, mut user) = (false, None);
    let mut store_user = || {
        if stored {
            return Ok(());
        }
        stored = true;
        if keep {
            user = append(&st, &member, "user", text, model, None).map_err(|e| {
                eprintln!("fonsviva org: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            })?;
        }
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
        Err((s, _)) if s.is_server_error() => {
            if let Err(e) = store_user() {
                return err(e.0, e.1);
            }
            Err(s.as_u16())
        }
        Err((s, code)) => return retry_after(code, err(s, code)),
    };
    let model = model.to_string();
    let (tx, rx) = mpsc::channel::<Event>(32);
    tokio::spawn(async move {
        let _turn = turn;
        let _ = tx.send(event(json!({"type": "start", "seq": user}))).await;
        let (status, reply) = match answer {
            Err(s) => (s, String::new()),
            Ok(r) if !r.status().is_success() => (r.status().as_u16(), String::new()),
            Ok(r) => relay(r, &tx).await,
        };
        let seq = if keep {
            append(&st, &member, "assistant", &reply, &model, Some(status)).unwrap_or_else(|e| {
                eprintln!("fonsviva org: {e:#}");
                None
            })
        } else {
            None
        };
        let _ = tx
            .send(event(json!({"type": "done", "seq": seq, "status": status})))
            .await;
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
    let (mut line, mut reply) = (Vec::new(), String::new());
    loop {
        let next = tokio::select! {
            _ = tx.closed() => return (CLIENT_GONE, reply),
            c = up.next() => c,
        };
        let chunk = match next {
            None => return (status, reply),
            Some(Err(_)) => return (UPSTREAM_DOWN, reply),
            Some(Ok(c)) => c,
        };
        for &b in chunk.iter() {
            if b != b'\n' {
                if line.len() < LINE_LIMIT {
                    line.push(b);
                }
                continue;
            }
            let Some(t) = delta(&line) else {
                line.clear();
                continue;
            };
            line.clear();
            let mut room = TEXT_LIMIT.saturating_sub(reply.len()).min(t.len());
            while !t.is_char_boundary(room) {
                room -= 1;
            }
            reply.push_str(&t[..room]);
            if tx
                .send(event(json!({"type": "delta", "text": t})))
                .await
                .is_err()
            {
                return (CLIENT_GONE, reply);
            }
        }
    }
}

/// The content of a stream line's first choice delta.
fn delta(line: &[u8]) -> Option<String> {
    let data = line.strip_prefix(b"data:")?;
    let v: Value = serde_json::from_slice(data.trim_ascii()).ok()?;
    let t = v["choices"][0]["delta"]["content"].as_str()?;
    (!t.is_empty()).then(|| t.to_string())
}
