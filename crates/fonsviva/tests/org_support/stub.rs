//! A stand-in OpenAI-compatible source on 127.0.0.1:0: it lists models,
//! answers chat with and without streaming, records what it got, and can be
//! told to fail, to stall before its headers, to cut a stream, to hold a
//! stream's last chunk, or to drip a long stream.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Seen {
    pub headers: HeaderMap,
    pub body: Value,
}

pub struct StubState {
    pub models: Vec<String>,
    /// Chat answers with this status and body.
    pub fail: Option<(u16, String)>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    /// Stream a chunk every 100 ms for 30 s instead of the short stream.
    pub drip: bool,
    /// Wait 30 s before answering chat, sending no headers.
    pub stall: bool,
    /// Break the short stream's connection after its first chunk.
    pub cut: bool,
    /// Hold the short stream's last chunks until `released`.
    pub hold: bool,
    pub released: bool,
    pub seen: Vec<Seen>,
    pub model_asks: Vec<HeaderMap>,
    /// How long the model list takes to answer.
    pub models_delay: Duration,
    /// Answer this many model list asks with `models_code` before listing.
    pub models_fail: u32,
    pub models_code: u16,
    /// Stream usage on the last chunk with choices, and the others as
    /// `inline_part` writes them, instead of a usage-only chunk.
    pub inline_usage: bool,
    /// How long a chat answer without streaming takes.
    pub chat_delay: Duration,
    /// Answer with no usage at all, as a source that ignores
    /// `include_usage` does.
    pub no_usage: bool,
    /// The reply text, and the first streamed part, in place of the usual.
    pub reply: Option<String>,
    /// The first streamed delta, in place of the reply's.
    pub delta: Option<Value>,
    /// Usage with its prompt tokens alone.
    pub partial_usage: bool,
    /// Keep a stream open after its `[DONE]` until `released`.
    pub linger: bool,
    /// End a stream with these bytes in place of its last chunk and `[DONE]`.
    pub tail: Option<String>,
    /// Put the usage on every streamed chunk with choices.
    pub usage_each: bool,
    /// When a streamed answer's reader went away.
    pub closed: Option<Instant>,
    /// When the short stream sent its last chunk.
    pub finished: Option<Instant>,
}

pub struct Stub {
    pub port: u16,
    pub state: Arc<Mutex<StubState>>,
    _rt: tokio::runtime::Runtime,
}

type S = Arc<Mutex<StubState>>;

impl Stub {
    pub fn start() -> Stub {
        let state = Arc::new(Mutex::new(StubState {
            models: vec!["tiny-1".into(), "org/big-2".into()],
            fail: None,
            prompt_tokens: 11,
            completion_tokens: 7,
            drip: false,
            stall: false,
            cut: false,
            hold: false,
            released: false,
            seen: vec![],
            model_asks: vec![],
            models_delay: Duration::ZERO,
            models_fail: 0,
            models_code: 500,
            inline_usage: false,
            chat_delay: Duration::ZERO,
            no_usage: false,
            reply: None,
            delta: None,
            partial_usage: false,
            linger: false,
            tail: None,
            usage_each: false,
            closed: None,
            finished: None,
        }));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/v1/models", get(models))
            .route("/v1/chat/completions", post(chat))
            .with_state(state.clone());
        rt.spawn(async move { axum::serve(listener, app).await });
        Stub {
            port,
            state,
            _rt: rt,
        }
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, StubState> {
        self.state.lock().unwrap()
    }
}

async fn models(State(s): State<S>, h: HeaderMap) -> Response {
    let (delay, data, fail): (Duration, Vec<Value>, Option<u16>) = {
        let mut s = s.lock().unwrap();
        s.model_asks.push(h);
        let fail = (s.models_fail > 0).then_some(s.models_code);
        s.models_fail = s.models_fail.saturating_sub(1);
        let data = s
            .models
            .iter()
            .map(|m| json!({"id": m, "object": "model", "owned_by": "stub"}))
            .collect();
        (s.models_delay, data, fail)
    };
    tokio::time::sleep(delay).await;
    if let Some(code) = fail {
        return StatusCode::from_u16(code).unwrap().into_response();
    }
    axum::Json(json!({"object": "list", "data": data})).into_response()
}

/// A content event as some sources write it: `usage: null`, keys out of
/// order and a float with more digits than an f64 holds.
pub fn inline_part(t: &str) -> String {
    format!(
        r#"data: {{"z":1,"choices":[{{"index":0,"delta":{{"content":"{t}"}}}}],"usage":null,"p":0.1234567890123456789}}"#
    )
}

fn chunk(v: Value) -> Result<Bytes, std::io::Error> {
    Ok(Bytes::from(format!("data: {v}\n\n")))
}

async fn chat(State(s): State<S>, headers: HeaderMap, body: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let stream = body["stream"] == true;
    let (fail, usage, drip, stall, cut, hold, inline, delay, reply) = {
        let mut st = s.lock().unwrap();
        st.seen.push(Seen { headers, body });
        let usage = if st.partial_usage {
            json!({"prompt_tokens": st.prompt_tokens})
        } else {
            json!({
                "prompt_tokens": st.prompt_tokens,
                "completion_tokens": st.completion_tokens,
                "total_tokens": st.prompt_tokens + st.completion_tokens,
            })
        };
        (
            st.fail.clone(),
            usage,
            st.drip,
            st.stall,
            st.cut,
            st.hold,
            st.inline_usage,
            st.chat_delay,
            st.reply.clone(),
        )
    };
    let (no_usage, delta, linger, each, tail) = {
        let st = s.lock().unwrap();
        let tail = st.tail.clone();
        (
            st.no_usage,
            st.delta.clone(),
            st.linger,
            st.usage_each,
            tail,
        )
    };
    let usage = if no_usage { Value::Null } else { usage };
    if stall {
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    if let Some((code, text)) = fail {
        let status = StatusCode::from_u16(code).unwrap();
        return (status, [(header::CONTENT_TYPE, "application/json")], text).into_response();
    }
    if !stream {
        tokio::time::sleep(delay).await;
        return axum::Json(json!({
            "id": "chatcmpl-stub",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": reply.as_deref().unwrap_or("stub reply")}, "finish_reason": "stop"}],
            "usage": usage,
        }))
        .into_response();
    }
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        let part = |t: &str| {
            if inline {
                return Ok(Bytes::from(format!("{}\n\n", inline_part(t))));
            }
            let mut v = json!({"choices": [{"index": 0, "delta": {"content": t}}]});
            if each {
                v["usage"] = usage.clone();
            }
            chunk(v)
        };
        if drip {
            for _ in 0..300 {
                if tx.send(part("drip")).await.is_err() {
                    s.lock().unwrap().closed = Some(Instant::now());
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            return;
        }
        let first = match delta {
            Some(d) => chunk(json!({"choices": [{"index": 0, "delta": d}]})),
            None => part(reply.as_deref().unwrap_or("one ")),
        };
        let _ = tx.send(first).await;
        if cut {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = tx.send(Err(std::io::Error::other("cut"))).await;
            return;
        }
        let _ = tx.send(part("two")).await;
        if hold {
            let until = Instant::now() + Duration::from_secs(10);
            while !s.lock().unwrap().released && Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        } else {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        if let Some(t) = tail {
            let _ = tx.send(Ok(Bytes::from(t))).await;
            return;
        }
        let last = if no_usage {
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
        } else if inline {
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": usage})
        } else {
            json!({"choices": [], "usage": usage})
        };
        let _ = tx.send(chunk(last)).await;
        let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
        s.lock().unwrap().finished = Some(Instant::now());
        let until = Instant::now() + Duration::from_secs(10);
        while linger && !s.lock().unwrap().released && Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header("x-stub-secret", "upstream-only")
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap()
}
