//! The chat proxy: forwards a member's request to a source, streams the
//! answer back, and reads its token counts without keeping any text.

use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header};
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::sources::{Sources, authed, name_ok};
use super::store::Source;

/// How much of a non-streamed answer is read for its usage.
const ANSWER_LIMIT: usize = 16 << 20;
/// The longest SSE line read for usage; longer lines pass through unread.
const LINE_LIMIT: usize = 1 << 20;

impl Sources {
    /// Sends a chat request to the source and streams its answer back with
    /// its status and content type. `done` gets the status and the tokens
    /// the answer reported once it ends or the client goes away. `Err` means
    /// the source could not be reached, or its key can't be opened, in which
    /// case nothing is sent and `done` is not called.
    pub async fn forward(
        &self,
        s: &Source,
        body: Vec<u8>,
        done: impl FnOnce(u16, Option<i64>, Option<i64>) + Send + 'static,
    ) -> Result<Response, ()> {
        let url = format!("{}/chat/completions", s.base_url);
        let req = authed(self.client.post(url), s)
            .ok_or(())?
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
        let resp = match req.send().await {
            Ok(r) => r,
            Err(_) => {
                done(StatusCode::BAD_GATEWAY.as_u16(), None, None);
                return Err(());
            }
        };
        let status = resp.status().as_u16();
        let ctype = resp.headers().get(header::CONTENT_TYPE).cloned();
        let sse = ctype
            .as_ref()
            .and_then(|c| c.to_str().ok())
            .is_some_and(|c| c.starts_with("text/event-stream"));
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);
        tokio::spawn(async move {
            let mut tap = Tap::new(sse);
            let mut up = resp.bytes_stream();
            loop {
                let next = tokio::select! {
                    _ = tx.closed() => break,
                    c = up.next() => c,
                };
                match next {
                    Some(Ok(chunk)) => {
                        tap.feed(&chunk);
                        if tx.send(Ok(chunk)).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(_)) => {
                        let _ = tx.send(Err(std::io::Error::other("upstream"))).await;
                        break;
                    }
                    None => break,
                }
            }
            drop(up);
            let (p, c) = tap.usage();
            done(status, p, c);
        });
        let mut r = Response::builder().status(status);
        if let Some(c) = ctype {
            r = r.header(header::CONTENT_TYPE, c);
        }
        Ok(r.body(Body::from_stream(ReceiverStream::new(rx)))
            .expect("response"))
    }
}

/// Reads `usage` from an answer as it passes: the last SSE event that has
/// one, or the whole JSON body.
struct Tap {
    sse: bool,
    buf: Vec<u8>,
    over: bool,
    usage: Option<(Option<i64>, Option<i64>)>,
}

impl Tap {
    fn new(sse: bool) -> Tap {
        Tap {
            sse,
            buf: Vec::new(),
            over: false,
            usage: None,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        if !self.sse {
            if self.buf.len() + chunk.len() > ANSWER_LIMIT {
                self.over = true;
                self.buf = Vec::new();
            }
            if !self.over {
                self.buf.extend_from_slice(chunk);
            }
            return;
        }
        for &b in chunk {
            if b == b'\n' {
                if !self.over {
                    let line = std::mem::take(&mut self.buf);
                    self.line(&line);
                }
                self.buf.clear();
                self.over = false;
            } else if self.buf.len() < LINE_LIMIT {
                self.buf.push(b);
            } else {
                self.over = true;
            }
        }
    }

    fn line(&mut self, line: &[u8]) {
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        if let Ok(v) = serde_json::from_slice::<Value>(data.trim_ascii()) {
            self.take(&v);
        }
    }

    fn take(&mut self, v: &Value) {
        let u = &v["usage"];
        if u.is_object() {
            self.usage = Some((u["prompt_tokens"].as_i64(), u["completion_tokens"].as_i64()));
        }
    }

    fn usage(mut self) -> (Option<i64>, Option<i64>) {
        if !self.sse && !self.over {
            if let Ok(v) = serde_json::from_slice::<Value>(&self.buf) {
                self.take(&v);
            }
        } else if self.sse && !self.over && !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.line(&line);
        }
        self.usage.unwrap_or((None, None))
    }
}

/// The source name and the upstream id in `<source name>/<upstream id>`.
pub fn split_model(id: &str) -> Option<(&str, &str)> {
    id.split_once('/')
        .filter(|(s, m)| name_ok(s) && !m.is_empty())
}
