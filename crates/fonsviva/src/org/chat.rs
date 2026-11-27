//! The chat proxy: forwards a member's request to a source, streams the
//! answer back, and reads its token counts without keeping any text.

use axum::body::{Body, Bytes};
use axum::http::header;
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

/// The status a usage row gets when the client went away before the
/// source answered.
pub const CLIENT_GONE: u16 = 499;
/// The status a usage row gets when the source can't be reached or fails
/// after its headers.
pub const UPSTREAM_DOWN: u16 = 502;

type Done = Box<dyn FnOnce(u16, Option<i64>, Option<i64>) + Send>;

/// Writes one usage row: through `done` once, or with 499 and no tokens
/// when dropped before that, as when the client goes away.
pub struct Record(Option<Done>);

impl Record {
    pub fn new(f: impl FnOnce(u16, Option<i64>, Option<i64>) + Send + 'static) -> Record {
        Record(Some(Box::new(f)))
    }

    pub fn done(mut self, status: u16, p: Option<i64>, c: Option<i64>) {
        if let Some(f) = self.0.take() {
            f(status, p, c);
        }
    }

    /// Drops the record without a row.
    pub fn skip(mut self) {
        self.0 = None;
    }
}

impl Drop for Record {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f(CLIENT_GONE, None, None);
        }
    }
}

impl Sources {
    /// Sends a chat request to the source and streams its answer back with
    /// its status and content type. `rec` gets the status and the tokens
    /// the answer reported once it ends or the client goes away, and 502
    /// when the source fails before or after its headers. With `strip`, a
    /// streamed answer loses its usage-only event. `Err` means the source
    /// could not be reached, or its key can't be opened, in which case
    /// nothing is sent.
    pub async fn forward(
        &self,
        s: &Source,
        body: Vec<u8>,
        strip: bool,
        rec: Record,
    ) -> Result<Response, ()> {
        let url = format!("{}/chat/completions", s.base_url);
        let Some(req) = authed(self.client_for(s).post(url), s) else {
            rec.done(UPSTREAM_DOWN, None, None);
            return Err(());
        };
        let req = req
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
        let resp = match req.send().await {
            Ok(r) => r,
            Err(_) => {
                rec.done(UPSTREAM_DOWN, None, None);
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
            let mut tap = Tap::new(sse, strip);
            let mut up = resp.bytes_stream();
            let mut end = status;
            loop {
                let next = tokio::select! {
                    _ = tx.closed() => break,
                    c = up.next() => c,
                };
                match next {
                    Some(Ok(chunk)) => {
                        let out = tap.feed(chunk);
                        if !out.is_empty() && tx.send(Ok(out)).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(_)) => {
                        end = UPSTREAM_DOWN;
                        let _ = tx.send(Err(std::io::Error::other("upstream"))).await;
                        break;
                    }
                    None => {
                        let out = tap.flush();
                        if !out.is_empty() {
                            let _ = tx.send(Ok(out)).await;
                        }
                        break;
                    }
                }
            }
            drop(up);
            let (p, c) = tap.usage();
            rec.done(end, p, c);
        });
        let mut r = Response::builder().status(status);
        if let Some(c) = ctype {
            r = r.header(header::CONTENT_TYPE, c);
        }
        Ok(r.body(Body::from_stream(ReceiverStream::new(rx)))
            .expect("response"))
    }
}

enum Kind {
    Other,
    UsageOnly,
    WithUsage(Value),
}

/// Reads `usage` from an answer as it passes: the last SSE event that has
/// one, or the whole JSON body. With `strip`, each SSE line is held until
/// it ends and a usage-only event is left out of what passes on.
struct Tap {
    sse: bool,
    strip: bool,
    buf: Vec<u8>,
    over: bool,
    dropped: bool,
    usage: Option<(Option<i64>, Option<i64>)>,
}

impl Tap {
    fn new(sse: bool, strip: bool) -> Tap {
        Tap {
            sse,
            strip: strip && sse,
            buf: Vec::new(),
            over: false,
            dropped: false,
            usage: None,
        }
    }

    /// Reads a chunk and returns what passes on to the client.
    fn feed(&mut self, chunk: Bytes) -> Bytes {
        if !self.sse {
            if self.buf.len() + chunk.len() > ANSWER_LIMIT {
                self.over = true;
                self.buf = Vec::new();
            }
            if !self.over {
                self.buf.extend_from_slice(&chunk);
            }
            return chunk;
        }
        let mut out = Vec::new();
        for &b in chunk.iter() {
            if b == b'\n' {
                if self.over {
                    out.push(b);
                } else {
                    let line = std::mem::take(&mut self.buf);
                    if let Some(l) = self.pass(line)
                        && self.strip
                    {
                        out.extend_from_slice(&l);
                        out.push(b);
                    }
                }
                self.over = false;
            } else if self.over {
                out.push(b);
            } else if self.buf.len() < LINE_LIMIT {
                self.buf.push(b);
            } else {
                self.over = true;
                self.dropped = false;
                out.append(&mut self.buf);
                out.push(b);
            }
        }
        if self.strip { Bytes::from(out) } else { chunk }
    }

    /// Reads a whole SSE line and returns what passes on: nothing for a
    /// usage-only event being stripped or the blank line ending one, and
    /// an event without its `usage` key when it has choices too.
    fn pass(&mut self, line: Vec<u8>) -> Option<Vec<u8>> {
        let kind = self.line(&line);
        if !self.strip {
            return Some(line);
        }
        let was = std::mem::replace(&mut self.dropped, false);
        match kind {
            Kind::UsageOnly => {
                self.dropped = true;
                None
            }
            Kind::WithUsage(mut v) => {
                if let Some(o) = v.as_object_mut() {
                    o.remove("usage");
                }
                let mut l = format!("data: {v}").into_bytes();
                if line.ends_with(b"\r") {
                    l.push(b'\r');
                }
                Some(l)
            }
            Kind::Other if was && line.trim_ascii().is_empty() => None,
            Kind::Other => Some(line),
        }
    }

    /// The last SSE line when the answer ended without a newline.
    fn flush(&mut self) -> Bytes {
        if !self.sse || self.over || self.buf.is_empty() {
            return Bytes::new();
        }
        let line = std::mem::take(&mut self.buf);
        match self.pass(line) {
            Some(l) if self.strip => Bytes::from(l),
            _ => Bytes::new(),
        }
    }

    /// Records the usage on a `data:` line and says what kind of event it
    /// is: one holding only usage, as the final chunk of a stream asked
    /// for it does, or one with a `usage` key beside its choices.
    fn line(&mut self, line: &[u8]) -> Kind {
        let Some(data) = line.strip_prefix(b"data:") else {
            return Kind::Other;
        };
        let Ok(v) = serde_json::from_slice::<Value>(data.trim_ascii()) else {
            return Kind::Other;
        };
        self.take(&v);
        if v.get("usage").is_none() {
            Kind::Other
        } else if v["usage"].is_object() && v["choices"].as_array().is_none_or(|c| c.is_empty()) {
            Kind::UsageOnly
        } else {
            Kind::WithUsage(v)
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
