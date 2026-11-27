//! A stand-in OpenAI-compatible source on 127.0.0.1:0: it lists models,
//! answers chat with and without streaming, records what it got, and can be
//! told to fail or to drip a long stream.

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
    pub seen: Vec<Seen>,
    pub model_asks: Vec<HeaderMap>,
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
            seen: vec![],
            model_asks: vec![],
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
    let mut s = s.lock().unwrap();
    s.model_asks.push(h);
    let data: Vec<Value> = s
        .models
        .iter()
        .map(|m| json!({"id": m, "object": "model", "owned_by": "stub"}))
        .collect();
    axum::Json(json!({"object": "list", "data": data})).into_response()
}

fn chunk(v: Value) -> Result<Bytes, std::io::Error> {
    Ok(Bytes::from(format!("data: {v}\n\n")))
}

async fn chat(State(s): State<S>, headers: HeaderMap, body: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let stream = body["stream"] == true;
    let (fail, usage, drip) = {
        let mut st = s.lock().unwrap();
        st.seen.push(Seen { headers, body });
        let usage = json!({
            "prompt_tokens": st.prompt_tokens,
            "completion_tokens": st.completion_tokens,
            "total_tokens": st.prompt_tokens + st.completion_tokens,
        });
        (st.fail.clone(), usage, st.drip)
    };
    if let Some((code, text)) = fail {
        let status = StatusCode::from_u16(code).unwrap();
        return (status, [(header::CONTENT_TYPE, "application/json")], text).into_response();
    }
    if !stream {
        return axum::Json(json!({
            "id": "chatcmpl-stub",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "stub reply"}, "finish_reason": "stop"}],
            "usage": usage,
        }))
        .into_response();
    }
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        let part = |t: &str| chunk(json!({"choices": [{"index": 0, "delta": {"content": t}}]}));
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
        let _ = tx.send(part("one ")).await;
        let _ = tx.send(part("two")).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let _ = tx.send(chunk(json!({"choices": [], "usage": usage}))).await;
        let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
        s.lock().unwrap().finished = Some(Instant::now());
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header("x-stub-secret", "upstream-only")
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap()
}
