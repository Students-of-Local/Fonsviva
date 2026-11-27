//! The org brain's web tools: `web_search` through the org's SearXNG, and
//! `read_page`, which reaches only public addresses and connects to the
//! address it checked (docs/ARCHITECTURE.md, Org brain).

use futures_util::StreamExt;
use reqwest::header::{CONTENT_TYPE, LOCATION};
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use super::{sources, tools};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// The most of an answer read, for a search or a page.
const BODY_LIMIT: usize = 2 << 20;
const RESULTS: usize = 8;
const TITLE_LIMIT: usize = 200;
const SNIPPET_LIMIT: usize = 300;
const REDIRECTS: usize = 3;
/// Tags whose text is left out, with all they hold.
const DROPPED: &[&str] = &["script", "style", "title", "noscript", "template"];
/// Tags that join their text to the words around them.
const INLINE: &[&str] = &[
    "a", "abbr", "b", "code", "em", "i", "mark", "small", "span", "strong", "sub", "sup", "u",
];

/// What leaves the org while search is on, for a SearXNG on the org's own
/// network or off it.
pub fn notice(u: &url::Url, local: bool) -> String {
    let host = u.host_str().unwrap_or_default();
    if local {
        return format!(
            "Searches go to the org's SearXNG at {host}, and from there to the search engines it asks. Pages are fetched from their own hosts. Your messages stay here; the words of each search and the address of each page read leave the org."
        );
    }
    format!(
        "Searches go to {host} and from there to the search engines it asks. Your messages stay here; the words of each search leave the org."
    )
}

fn v4_public(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || (a == 198 && (b & 0xfe) == 18)
        || ip.is_multicast()
        || a >= 240)
}

fn v6_public(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return v4_public(v4);
    }
    let s = ip.segments();
    let low = Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
    match s[0] {
        0 => false,
        0x64 if s[1] == 0xff9b => s[2..6] == [0; 4] && v4_public(low),
        0x100 if s[1..4] == [0; 3] => false,
        0x2002 => v4_public(Ipv4Addr::new(
            (s[1] >> 8) as u8,
            s[1] as u8,
            (s[2] >> 8) as u8,
            s[2] as u8,
        )),
        0x2001 if s[1] == 0xdb8 || (s[1] == 2 && s[2] == 0) => false,
        f => {
            (f & 0xfe00) != 0xfc00
                && (f & 0xffc0) != 0xfe80
                && (f & 0xffc0) != 0xfec0
                && f >> 8 != 0xff
        }
    }
}

fn ip_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => v4_public(ip),
        IpAddr::V6(ip) => v6_public(ip),
    }
}

/// Whether debug builds count this name as public whatever it resolves
/// to: `FONSVIVA_TEST_PUBLIC` lists such names (`a,b`), so tests reach
/// stub pages on 127.0.0.1.
fn counted_public(_name: &str) -> bool {
    #[cfg(debug_assertions)]
    if let Ok(names) = std::env::var("FONSVIVA_TEST_PUBLIC") {
        return names.split(',').any(|n| n.eq_ignore_ascii_case(_name));
    }
    false
}

/// Whether a name that resolves to `ips` may be read.
fn name_public(name: &str, ips: &[IpAddr]) -> bool {
    counted_public(name) || ips.iter().all(|ip| ip_public(*ip))
}

#[derive(Debug)]
struct Blocked;

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not a public address")
    }
}

impl std::error::Error for Blocked {}

/// The resolver pages are read through: a name is given only when every
/// address it resolves to is public, and the connection goes to those same
/// addresses, so no second lookup can move it.
struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let name = name.as_str().trim_end_matches('.').to_ascii_lowercase();
        Box::pin(async move {
            let ips = sources::resolve(&name, 0).await;
            if ips.is_empty() {
                return Err("the name does not resolve".into());
            }
            if !name_public(&name, &ips) {
                return Err(Box::new(Blocked) as _);
            }
            let addrs: reqwest::dns::Addrs =
                Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

fn blocked(e: &reqwest::Error) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(c) = cause {
        if c.is::<Blocked>() {
            return true;
        }
        cause = c.source();
    }
    false
}

/// A URL as pages are matched: http or https, without its fragment.
pub fn key(s: &str) -> Option<String> {
    let mut u = url::Url::parse(s).ok()?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        return None;
    }
    u.set_fragment(None);
    Some(u.into())
}

/// The http and https URLs written in a message, as `key` gives them: as
/// written, and also without trailing punctuation when it ends in some. A
/// `)` stays when the URL holds the `(` it closes.
pub fn urls_in(text: &str) -> Vec<String> {
    let mut out = vec![];
    for (at, _) in text.match_indices("http") {
        let rest = &text[at..];
        if !(rest.starts_with("http://") || rest.starts_with("https://")) {
            continue;
        }
        let end = rest
            .find(|c: char| c.is_whitespace() || "<>\"'`".contains(c))
            .unwrap_or(rest.len());
        let raw = &rest[..end];
        let mut u = raw.trim_end_matches(|c| ".,;:!?)]}".contains(c));
        while raw[u.len()..].starts_with(')') && u.matches('(').count() > u.matches(')').count() {
            u = &raw[..u.len() + 1];
        }
        out.extend(key(u));
        if u != raw {
            out.extend(key(raw));
        }
    }
    out
}

/// Whether a host of the org's own sources, or an address that isn't
/// public, is the page's host. Names are checked again when they resolve.
fn host_ok(u: &url::Url, own: &[String]) -> Result<(), &'static str> {
    if !matches!(u.scheme(), "http" | "https") {
        return Err("bad_scheme");
    }
    let host = u
        .host_str()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if own.contains(&host) {
        return Err("blocked_address");
    }
    match u.host() {
        Some(url::Host::Ipv4(ip)) if v4_public(ip) => Ok(()),
        Some(url::Host::Ipv6(ip)) if v6_public(ip) => Ok(()),
        Some(url::Host::Domain(_)) if host != "localhost" && !host.ends_with(".localhost") => {
            Ok(())
        }
        _ => Err("blocked_address"),
    }
}

/// A host as `host_ok` compares it.
pub fn host_of(base: &str) -> Option<String> {
    let u = url::Url::parse(base).ok()?;
    Some(u.host_str()?.trim_end_matches('.').to_ascii_lowercase())
}

/// An answer's body, at most `BODY_LIMIT` bytes: cut there, or `None` when
/// `whole` asks for all of it.
async fn body(r: reqwest::Response, whole: bool) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut stream = r.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if out.len() + chunk.len() > BODY_LIMIT {
            if whole {
                return None;
            }
            out.extend_from_slice(&chunk[..BODY_LIMIT - out.len()]);
            break;
        }
        out.extend_from_slice(&chunk);
    }
    Some(out)
}

fn chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// The first `RESULTS` results with an http or https URL that the search
/// service gives for `query`.
pub async fn search(
    client: &reqwest::Client,
    base: &str,
    query: &str,
) -> Result<Vec<Value>, &'static str> {
    let failed = "search_failed";
    let r = client
        .get(format!("{}/search", base.trim_end_matches('/')))
        .query(&[("q", query), ("format", "json")])
        .timeout(SEARCH_TIMEOUT)
        .send()
        .await
        .map_err(|_| failed)?;
    if !r.status().is_success() {
        return Err(failed);
    }
    let body = body(r, true).await.ok_or(failed)?;
    let v: Value = serde_json::from_slice(&body).map_err(|_| failed)?;
    let results = v["results"].as_array().ok_or(failed)?;
    Ok(results
        .iter()
        .filter_map(|r| {
            let url = key(r["url"].as_str()?)?;
            Some(json!({
                "title": chars(r["title"].as_str().unwrap_or_default(), TITLE_LIMIT),
                "url": url,
                "snippet": chars(r["content"].as_str().unwrap_or_default(), SNIPPET_LIMIT),
            }))
        })
        .take(RESULTS)
        .collect())
}

/// A page's title, its URL after redirects and its text, read only from
/// public addresses that are no host of `own`.
pub async fn read(u: url::Url, own: &[String]) -> Result<Value, &'static str> {
    let client = reqwest::Client::builder()
        .connect_timeout(READ_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .dns_resolver(Arc::new(PublicOnly))
        .build()
        .map_err(|_| "read_failed")?;
    tokio::time::timeout(READ_TIMEOUT, fetch(&client, u, own))
        .await
        .unwrap_or(Err("read_failed"))
}

async fn fetch(
    client: &reqwest::Client,
    mut u: url::Url,
    own: &[String],
) -> Result<Value, &'static str> {
    let mut hops = 0;
    let r = loop {
        host_ok(&u, own)?;
        let r = client.get(u.clone()).send().await.map_err(|e| {
            if blocked(&e) {
                "blocked_address"
            } else {
                "read_failed"
            }
        })?;
        if !r.status().is_redirection() {
            break r;
        }
        if hops == REDIRECTS {
            return Err("too_many_redirects");
        }
        hops += 1;
        u = r
            .headers()
            .get(LOCATION)
            .and_then(|l| l.to_str().ok())
            .and_then(|l| u.join(l).ok())
            .ok_or("read_failed")?;
    };
    if !r.status().is_success() {
        return Err("read_failed");
    }
    let kind = r
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|t| t.to_str().ok())
        .and_then(|t| t.split(';').next())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let html = match kind.as_str() {
        "text/html" | "application/xhtml+xml" => true,
        "text/plain" => false,
        _ => return Err("unsupported_type"),
    };
    let page = body(r, false).await.ok_or("read_failed")?;
    let page = String::from_utf8_lossy(&page);
    let (title, text) = if html {
        reduce(&page)
    } else {
        (String::new(), page.into_owned())
    };
    u.set_fragment(None);
    Ok(fitted(chars(&title, TITLE_LIMIT), u.as_str(), text))
}

/// A page's answer with its text cut so the answer as JSON is at most
/// `tools::RESULT_LIMIT` bytes.
fn fitted(title: String, url: &str, mut text: String) -> Value {
    let rest = json!({"title": title, "url": url, "text": ""})
        .to_string()
        .len();
    let room = tools::RESULT_LIMIT.saturating_sub(rest);
    let mut used = 0;
    let end = text
        .char_indices()
        .find(|(_, c)| {
            used += escaped(*c);
            used > room
        })
        .map_or(text.len(), |(at, _)| at);
    text.truncate(end);
    json!({"title": title, "url": url, "text": text})
}

/// The bytes a char takes in a JSON string.
fn escaped(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if c < ' ' => 6,
        c => c.len_utf8(),
    }
}

fn fold(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decoded(s: &str) -> String {
    html_escape::decode_html_entities(s).into_owned()
}

/// An HTML page's `<title>` and its text: no comment, script, style or
/// tag, entities decoded and whitespace folded.
fn reduce(html: &str) -> (String, String) {
    let lower = html.to_ascii_lowercase();
    let (mut out, mut title, mut i) = (String::new(), None, 0);
    while let Some(lt) = html[i..].find('<') {
        let at = i + lt;
        out.push_str(&html[i..at]);
        let rest = &lower[at + 1..];
        if !rest.starts_with(|c: char| c.is_ascii_alphabetic() || "/!?".contains(c)) {
            out.push('<');
            i = at + 1;
            continue;
        }
        if rest.starts_with("!--") {
            i = rest.find("-->").map_or(html.len(), |e| at + 1 + e + 3);
            out.push(' ');
            continue;
        }
        let name: String = rest
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        let end = lower[at..].find('>').map_or(html.len(), |e| at + e + 1);
        i = end;
        if !rest.starts_with('/') && DROPPED.contains(&name.as_str()) {
            let close = format!("</{name}");
            let stop = lower[end..].find(&close).map_or(html.len(), |e| end + e);
            if name == "title" && title.is_none() {
                title = Some(fold(&decoded(&html[end..stop])));
            }
            i = lower[stop..].find('>').map_or(html.len(), |e| stop + e + 1);
        }
        if !INLINE.contains(&name.as_str()) {
            out.push(' ');
        }
    }
    out.push_str(&html[i..]);
    (title.unwrap_or_default(), fold(&decoded(&out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_addresses_that_are_not_public_are_refused() {
        for u in [
            "http://[64:ff9b::7f00:1]/",
            "http://[64:ff9b:1::a00:1]/",
            "http://[2002:7f00:1::]/",
            "http://[2002:a00:1::]/",
            "http://[2001:2::1]/",
            "http://[100::1]/",
            "http://192.0.2.10/",
            "http://198.51.100.1/",
            "http://203.0.113.5/",
        ] {
            let u = url::Url::parse(u).unwrap();
            assert_eq!(host_ok(&u, &[]), Err("blocked_address"), "{u}");
        }
    }

    #[test]
    fn a_name_resolving_to_nat64_of_loopback_is_refused() {
        let ips = ["64:ff9b::7f00:1".parse().unwrap()];
        assert!(!name_public("nat64.test", &ips));
        let ips = ["93.184.215.14".parse().unwrap()];
        assert!(name_public("nat64.test", &ips));
    }

    #[test]
    fn a_page_answer_fits_whatever_its_text_escapes_to() {
        for unit in ["a", "\"", "\u{1}", "\u{e9}", "\u{1f600}"] {
            let v = fitted("t".into(), "http://pages.test/", unit.repeat(1 << 20));
            let n = v.to_string().len();
            assert!(
                (tools::RESULT_LIMIT - 6..=tools::RESULT_LIMIT).contains(&n),
                "{unit:?} {n}"
            );
        }
    }

    #[test]
    fn six_to_four_of_a_public_address_is_public() {
        let u = url::Url::parse("http://[2002:808:808::]/").unwrap();
        assert_eq!(host_ok(&u, &[]), Ok(()));
        let u = url::Url::parse("http://[64:ff9b::808:808]/").unwrap();
        assert_eq!(host_ok(&u, &[]), Ok(()));
    }
}
