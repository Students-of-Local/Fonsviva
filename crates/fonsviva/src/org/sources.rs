//! Model sources: which hosts count as the org's own, and the models each
//! source offers.

use futures_util::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::store::{Source, SourceKey};

const TIMEOUT: Duration = Duration::from_secs(10);
const MODELS_TTL: Duration = Duration::from_secs(30);
const MODELS_LIMIT: usize = 4 << 20;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// The error a source shows when its sealed key can't be opened.
pub const UNREADABLE: &str = "cannot read its key";
/// The error a local source shows when its name now resolves off the org's
/// network.
pub const LEFT: &str = "left the org's network";

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

fn ip_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => v4_local(ip),
        IpAddr::V6(ip) => v6_local(ip),
    }
}

/// The addresses a name resolves to; none when it doesn't resolve in 5 s.
/// Debug builds read `FONSVIVA_TEST_RESOLVE` (`name=ip,ip;name=`) instead
/// of the system resolver when it is set, so tests make no DNS queries.
async fn resolve(name: &str, port: u16) -> Vec<IpAddr> {
    #[cfg(debug_assertions)]
    if let Ok(fake) = std::env::var("FONSVIVA_TEST_RESOLVE") {
        return fake
            .split(';')
            .filter_map(|e| e.split_once('='))
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .flat_map(|(_, ips)| ips.split(',').filter_map(|ip| ip.parse().ok()))
            .collect();
    }
    match tokio::time::timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host((name, port))).await {
        Ok(Ok(addrs)) => addrs.map(|a| a.ip()).collect(),
        _ => vec![],
    }
}

/// True when the host is on the org's own network: a literal private
/// address, `localhost`, or a name whose every address is private. A name
/// that doesn't resolve is not.
pub async fn is_local(u: &url::Url) -> bool {
    match u.host() {
        Some(url::Host::Ipv4(ip)) => v4_local(ip),
        Some(url::Host::Ipv6(ip)) => v6_local(ip),
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            if d == "localhost" {
                return true;
            }
            let ips = resolve(&d, u.port_or_known_default().unwrap_or(443)).await;
            !ips.is_empty() && ips.into_iter().all(ip_local)
        }
        None => false,
    }
}

#[derive(Debug)]
struct LeftNetwork;

impl std::fmt::Display for LeftNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(LEFT)
    }
}

impl std::error::Error for LeftNetwork {}

/// The resolver of the client local sources are asked through. Each
/// connection resolves the name again and is given only addresses that are
/// all private, so a source stored as local never reaches a host off the
/// org's network, however long ago it was added or its list was cached.
struct OwnNetwork;

impl reqwest::dns::Resolve for OwnNetwork {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let name = name.as_str().trim_end_matches('.').to_ascii_lowercase();
        Box::pin(async move {
            let ips = if name == "localhost" {
                vec![Ipv4Addr::LOCALHOST.into(), Ipv6Addr::LOCALHOST.into()]
            } else {
                resolve(&name, 0).await
            };
            if ips.is_empty() {
                return Err("the name does not resolve".into());
            }
            if !ips.iter().all(|ip| ip_local(*ip)) {
                return Err(Box::new(LeftNetwork) as _);
            }
            let addrs: reqwest::dns::Addrs =
                Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

fn left_network(e: &reqwest::Error) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(c) = cause {
        if c.is::<LeftNetwork>() {
            return true;
        }
        cause = c.source();
    }
    false
}

pub fn notice(u: &url::Url, local: bool) -> String {
    let host = u.host_str().unwrap_or_default();
    if local {
        format!("Requests go to {host}, an address on your org's own network.")
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

/// The HTTP clients sources are asked through, and each source's models.
pub struct Sources {
    pub(super) client: reqwest::Client,
    local: reqwest::Client,
    cache: Mutex<HashMap<String, Cached>>,
    /// One model list fetch at a time per source.
    flights: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
}

impl Sources {
    pub fn new() -> Sources {
        Sources {
            client: builder().build().expect("HTTP client"),
            local: builder()
                .dns_resolver(Arc::new(OwnNetwork))
                .build()
                .expect("HTTP client"),
            cache: Mutex::new(HashMap::new()),
            flights: Mutex::new(HashMap::new()),
        }
    }

    /// The client for this source: a local one only reaches private
    /// addresses.
    pub(super) fn client_for(&self, s: &Source) -> &reqwest::Client {
        if s.local { &self.local } else { &self.client }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, Cached>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn flights(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<tokio::sync::Mutex<()>>>> {
        self.flights.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fresh(&self, id: &str) -> Option<Result<Vec<String>, String>> {
        let cache = self.cache();
        let c = cache.get(id).filter(|c| c.at.elapsed() < MODELS_TTL)?;
        Some(c.models.clone())
    }

    /// Caches a fetched list. A failure never replaces a fresh success.
    fn settle(&self, id: &str, models: Result<Vec<String>, String>) {
        let mut cache = self.cache();
        let fresh_ok = cache
            .get(id)
            .is_some_and(|c| c.models.is_ok() && c.at.elapsed() < MODELS_TTL);
        if models.is_err() && fresh_ok {
            return;
        }
        cache.insert(
            id.to_string(),
            Cached {
                at: Instant::now(),
                models,
            },
        );
    }

    /// The error of the source's last model list, while it is fresh.
    pub fn last_error(&self, id: &str) -> Option<String> {
        let cache = self.cache();
        let c = cache.get(id).filter(|c| c.at.elapsed() < MODELS_TTL)?;
        c.models.as_ref().err().cloned()
    }

    pub fn forget(&self, id: &str) {
        self.cache().remove(id);
        self.flights().remove(id);
    }

    /// Drops the cached lists of sources no longer in the store.
    pub fn keep(&self, ids: &[&str]) {
        self.cache().retain(|id, _| ids.contains(&id.as_str()));
        self.flights().retain(|id, _| ids.contains(&id.as_str()));
    }

    /// The source's upstream model ids, asked at most every 30 s and by one
    /// request at a time: callers that arrive during a fetch wait for it.
    /// A source whose key can't be opened is never asked and never cached:
    /// the store reports that state as it is.
    pub async fn models(&self, s: &Source) -> Result<Vec<String>, String> {
        if matches!(s.api_key, SourceKey::Unreadable) {
            return Err(UNREADABLE.into());
        }
        if let Some(m) = self.fresh(&s.id) {
            return m;
        }
        let flight = self.flights().entry(s.id.clone()).or_default().clone();
        let _turn = flight.lock().await;
        if let Some(m) = self.fresh(&s.id) {
            return m;
        }
        let models = self.fetch(s).await;
        self.settle(&s.id, models.clone());
        models
    }

    /// Errors are short and name no key and no body.
    async fn fetch(&self, s: &Source) -> Result<Vec<String>, String> {
        let url = format!("{}/models", s.base_url);
        let sent = authed(self.client_for(s).get(url), s)
            .ok_or(UNREADABLE)?
            .timeout(TIMEOUT)
            .send();
        let resp = sent.await.map_err(|e| {
            if left_network(&e) {
                LEFT.to_string()
            } else if e.is_timeout() {
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
    fn private_addresses() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.9",
            "100.64.0.1",
            "100.127.255.255",
            "169.254.1.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(ip_local(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "100.63.0.1",
            "100.128.0.1",
            "172.32.0.1",
            "8.8.8.8",
            "2001:db8::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!ip_local(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn the_system_resolver_finds_localhost() {
        if std::env::var_os("FONSVIVA_TEST_RESOLVE").is_some() {
            return;
        }
        let ips = resolve("localhost", 80).await;
        assert!(
            !ips.is_empty() && ips.iter().all(|ip| ip.is_loopback()),
            "{ips:?}"
        );
    }

    #[test]
    fn a_late_failure_keeps_a_fresh_list() {
        let s = Sources::new();
        s.settle("src_a", Ok(vec!["tiny-1".into()]));
        s.settle("src_a", Err("answered HTTP 429".into()));
        assert_eq!(s.fresh("src_a"), Some(Ok(vec!["tiny-1".to_string()])));
        assert_eq!(s.last_error("src_a"), None);
    }

    #[test]
    fn a_stale_error_is_not_shown() {
        let s = Sources::new();
        let old = Instant::now().checked_sub(MODELS_TTL).unwrap();
        s.cache().insert(
            "src_old".into(),
            Cached {
                at: old,
                models: Err("can't connect".into()),
            },
        );
        s.cache().insert(
            "src_new".into(),
            Cached {
                at: Instant::now(),
                models: Err("can't connect".into()),
            },
        );
        assert_eq!(s.last_error("src_old"), None);
        assert_eq!(s.last_error("src_new").as_deref(), Some("can't connect"));
    }
}
