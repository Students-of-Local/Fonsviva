//! Model sources: which hosts count as the org's own, and the models each
//! source offers.

use futures_util::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::store::{Source, SourceKey};

const TIMEOUT: Duration = Duration::from_secs(10);
const MODELS_TTL: Duration = Duration::from_secs(30);
const MODELS_LIMIT: usize = 4 << 20;
/// The error a source shows when its sealed key can't be opened.
pub const UNREADABLE: &str = "cannot read its key";

pub fn name_ok(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

/// A source URL: http or https with a host and no user, query or fragment.
pub fn parse_base(s: &str) -> Option<url::Url> {
    let u = url::Url::parse(s).ok()?;
    let ok = matches!(u.scheme(), "http" | "https")
        && u.host_str().is_some()
        && u.username().is_empty()
        && u.password().is_none()
        && u.query().is_none()
        && u.fragment().is_none();
    ok.then_some(u)
}

fn v4_local(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || (a == 100 && (64..128).contains(&b))
}

fn v6_local(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    ip.is_loopback()
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        || ip.to_ipv4_mapped().is_some_and(v4_local)
}

/// True when the host is on the org's own machines or network.
pub fn is_local(u: &url::Url) -> bool {
    match u.host() {
        Some(url::Host::Ipv4(ip)) => v4_local(ip),
        Some(url::Host::Ipv6(ip)) => v6_local(ip),
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d == "localhost" || d.ends_with(".local") || d.ends_with(".ts.net")
        }
        None => false,
    }
}

pub fn notice(u: &url::Url, local: bool) -> String {
    let host = u.host_str().unwrap_or_default();
    if local {
        format!("Requests stay on {host}, a machine your org runs.")
    } else {
        format!(
            "Members' messages to these models are sent to {host}, outside your org's machines. That service's own terms apply to what it keeps."
        )
    }
}

/// The request with the source's key, or `None` when its key can't be
/// opened, so no request goes out without it.
pub(super) fn authed(r: reqwest::RequestBuilder, s: &Source) -> Option<reqwest::RequestBuilder> {
    match &s.api_key {
        SourceKey::None => Some(r),
        SourceKey::Key(k) => Some(r.bearer_auth(k)),
        SourceKey::Unreadable => None,
    }
}

struct Cached {
    at: Instant,
    models: Result<Vec<String>, String>,
}

/// The HTTP client every source is asked through, and each source's models.
pub struct Sources {
    pub(super) client: reqwest::Client,
    cache: Mutex<HashMap<String, Cached>>,
}

impl Sources {
    pub fn new() -> Sources {
        let client = reqwest::Client::builder()
            .connect_timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .expect("HTTP client");
        Sources {
            client,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, Cached>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn last_error(&self, id: &str) -> Option<String> {
        self.cache().get(id)?.models.as_ref().err().cloned()
    }

    pub fn forget(&self, id: &str) {
        self.cache().remove(id);
    }

    /// The source's upstream model ids, asked at most every 30 s.
    pub async fn models(&self, s: &Source) -> Result<Vec<String>, String> {
        if let Some(c) = self.cache().get(&s.id)
            && c.at.elapsed() < MODELS_TTL
            && !matches!(s.api_key, SourceKey::Unreadable)
        {
            return c.models.clone();
        }
        let models = self.fetch(s).await;
        self.cache().insert(
            s.id.clone(),
            Cached {
                at: Instant::now(),
                models: models.clone(),
            },
        );
        models
    }

    /// Errors are short and name no key and no body.
    async fn fetch(&self, s: &Source) -> Result<Vec<String>, String> {
        let url = format!("{}/models", s.base_url);
        let sent = authed(self.client.get(url), s)
            .ok_or(UNREADABLE)?
            .timeout(TIMEOUT)
            .send();
        let resp = sent.await.map_err(|e| {
            if e.is_timeout() {
                "timed out".to_string()
            } else {
                "can't connect".to_string()
            }
        })?;
        if !resp.status().is_success() {
            return Err(format!("answered HTTP {}", resp.status().as_u16()));
        }
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| "the answer broke off".to_string())?;
            if body.len() + chunk.len() > MODELS_LIMIT {
                return Err("the model list is over 4 MiB".into());
            }
            body.extend_from_slice(&chunk);
        }
        let v: Value =
            serde_json::from_slice(&body).map_err(|_| "the model list is not JSON".to_string())?;
        let data = v["data"].as_array().ok_or("the model list has no data")?;
        Ok(data
            .iter()
            .filter_map(|m| m["id"].as_str().map(str::to_string))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_hosts() {
        for u in [
            "http://127.0.0.1:8080/v1",
            "http://10.1.2.3",
            "http://172.16.0.1",
            "http://192.168.1.9",
            "http://100.64.0.1",
            "http://100.127.255.255",
            "http://169.254.1.1",
            "http://[::1]",
            "http://[fd00::1]",
            "http://[fe80::1]",
            "http://[::ffff:10.0.0.1]",
            "http://localhost:11434",
            "http://gpu.local",
            "http://box.tail1234.ts.net/v1",
        ] {
            assert!(is_local(&parse_base(u).unwrap()), "{u}");
        }
        for u in [
            "https://api.example.com/v1",
            "http://100.63.0.1",
            "http://100.128.0.1",
            "http://172.32.0.1",
            "http://8.8.8.8",
            "http://[2001:db8::1]",
            "http://local.example.com",
            "http://ts.net.example.com",
        ] {
            assert!(!is_local(&parse_base(u).unwrap()), "{u}");
        }
    }
}
