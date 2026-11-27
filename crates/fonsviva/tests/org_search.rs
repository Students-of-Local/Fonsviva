//! Slice 7, second half: the org brain searches through a stub SearXNG and
//! reads stub pages on 127.0.0.1, which count as public only by name
//! through the debug-only `FONSVIVA_TEST_PUBLIC`, over real HTTP.

mod org_support;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use org_support::stub::Stub;
use org_support::{Org, add_source, admin, member, start};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MODEL: &str = "stub/tiny-1";

/// How the stub SearXNG answers.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Results,
    Slow,
    NotJson,
}

struct WebState {
    mode: Mode,
    /// Every path asked, with its query.
    hits: Vec<String>,
    port: u16,
}

/// A stub SearXNG and web pages on 127.0.0.1:0.
struct Web {
    port: u16,
    state: Arc<Mutex<WebState>>,
    _rt: tokio::runtime::Runtime,
}

type W = Arc<Mutex<WebState>>;

impl Web {
    fn start() -> Web {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(WebState {
            mode: Mode::Results,
            hits: vec![],
            port,
        }));
        let app = Router::new()
            .route("/search", get(search))
            .route("/page/{name}", get(page))
            .route("/r/{n}", get(hop))
            .route("/to/{name}", get(to))
            .layer(axum::middleware::from_fn_with_state(state.clone(), log))
            .with_state(state.clone());
        rt.spawn(async move { axum::serve(listener, app).await });
        Web {
            port,
            state,
            _rt: rt,
        }
    }

    fn url(&self, host: &str, path: &str) -> String {
        format!("http://{host}:{}{path}", self.port)
    }

    fn hits(&self) -> Vec<String> {
        self.state.lock().unwrap().hits.clone()
    }
}

async fn log(
    State(s): State<W>,
    uri: Uri,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    s.lock().unwrap().hits.push(uri.to_string());
    next.run(req).await
}

async fn search(State(s): State<W>, Query(q): Query<HashMap<String, String>>) -> Response {
    let (mode, port) = {
        let s = s.lock().unwrap();
        (s.mode, s.port)
    };
    match mode {
        Mode::Slow => tokio::time::sleep(Duration::from_secs(6)).await,
        Mode::NotJson => return "<html>no</html>".into_response(),
        Mode::Results => {}
    }
    let mut results = vec![
        json!({"title": "ftp", "url": "ftp://pages.test/x", "content": "x"}),
        json!({"title": "script", "url": "javascript:alert(1)", "content": "x"}),
    ];
    for i in 0..10 {
        results.push(json!({
            "title": format!("{i}{}", "t".repeat(300)),
            "url": format!("http://pages.test:{port}/page/html?n={i}"),
            "content": "s".repeat(500),
        }));
    }
    axum::Json(json!({"query": q.get("q"), "results": results})).into_response()
}

const HTML: &str = "<!doctype html><html><head><title>Caf&eacute; &amp; Co</title>\
<style>p{color:red}</style><script>var MARKSCRIPT = '<p>no</p>';</script></head>\
<body><h1>Menu</h1><p>Tea &lt;hot&gt; &#8364;3 &nbsp;and <b>cake</b></p><!-- MARKCOMMENT --></body></html>";

async fn page(Path(name): Path<String>) -> Response {
    let typed =
        |t: &'static str, body: Vec<u8>| ([(header::CONTENT_TYPE, t)], body).into_response();
    match name.as_str() {
        "html" => typed("text/html; charset=utf-8", HTML.into()),
        "plain" => typed("text/plain", b"line one\n  line two".to_vec()),
        "big" => typed("text/plain", vec![b'a'; 3 << 20]),
        "image" => typed("image/png", vec![0x89, b'P', b'N', b'G']),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

fn redirect(to: String) -> Response {
    (StatusCode::FOUND, [(header::LOCATION, to)]).into_response()
}

/// `/r/n` redirects `n` times before the plain page.
async fn hop(Path(n): Path<u32>) -> Response {
    match n {
        0 => redirect("/page/plain".into()),
        n => redirect(format!("/r/{}", n - 1)),
    }
}

/// Redirects to a page on a host that is not public, or of another scheme.
async fn to(State(s): State<W>, Path(name): Path<String>) -> Response {
    let port = s.lock().unwrap().port;
    redirect(match name.as_str() {
        "loopback" => format!("http://127.0.0.1:{port}/page/secret"),
        "name" => format!("http://loop.test:{port}/page/secret"),
        _ => "ftp://pages.test/page/secret".into(),
    })
}

/// An org with the stub source, an admin and a user: the org, the source,
/// the admin's and the user's cookies.
fn ready() -> (Org, Stub, String, String) {
    let org = start();
    let stub = Stub::start();
    let (a, _) = admin(&org);
    let r = add_source(&org, &a, "stub", &stub.base(), None);
    assert_eq!(r.status, 200, "{}", r.body);
    let (u, _) = member(&org, &a, "user", "Bo");
    (org, stub, a, u)
}

fn patch(org: &Org, admin: &str, v: Value) -> org_support::Resp {
    org.call("PATCH", "/api/org/v1/settings", Some(admin), v)
}

fn search_on(org: &Org, admin: &str, base: &str) {
    let r = patch(org, admin, json!({"searchUrl": base}));
    assert_eq!(r.status, 200, "{}", r.body);
}

fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"id": id, "name": name, "args": args})
}

fn read(id: &str, url: &str) -> Value {
    call(id, "read_page", json!({"url": url}))
}

fn script(stub: &Stub, steps: Vec<Value>) {
    stub.state().script = steps;
}

fn send(org: &Org, cookie: &str, text: &str) -> Vec<Value> {
    let r = org.call(
        "POST",
        "/api/org/v1/thread/messages",
        Some(cookie),
        json!({"text": text, "model": MODEL, "tools": true}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    r.body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

/// The tool results the stub's `n`th chat ask carried, parsed.
fn results(stub: &Stub, n: usize) -> Vec<Value> {
    stub.state().seen[n].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| serde_json::from_str(m["content"].as_str().unwrap()).unwrap())
        .collect()
}

/// Sends `text` with the model reading each of `urls`, four a step, and
/// returns each call's result.
fn read_all(org: &Org, stub: &Stub, cookie: &str, text: &str, urls: &[&str]) -> Vec<Value> {
    let mut steps: Vec<Value> = urls
        .chunks(4)
        .map(|c| {
            let calls: Vec<Value> = c.iter().map(|u| read("r", u)).collect();
            json!({"calls": calls})
        })
        .collect();
    steps.push(json!({"text": "done"}));
    let n = steps.len() - 1;
    script(stub, steps);
    let ev = send(org, cookie, text);
    assert_eq!(ev.last().unwrap()["status"], 200, "{:?}", ev.last());
    results(stub, n)
}

#[test]
fn case_20_the_search_url_follows_the_source_rules() {
    let (org, _stub, a, _u) = ready();
    let r = patch(
        &org,
        &a,
        json!({"searchUrl": "http://api.example.com/searx"}),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    let r = patch(
        &org,
        &a,
        json!({"searchUrl": "https://api.example.com/searx/"}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let s = r.json();
    assert_eq!(s["searchUrl"], "https://api.example.com/searx");
    assert_eq!(
        s["searchNotice"],
        "Searches go to api.example.com and from there to the search engines it asks. Your messages stay here; the words of each search leave the org."
    );
    let got = org.get("/api/org/v1/settings", Some(&a)).json();
    assert_eq!(got["searchNotice"], s["searchNotice"]);
    let web = Web::start();
    let r = patch(&org, &a, json!({"searchUrl": web.url("127.0.0.1", "")}));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["searchNotice"], Value::Null);
    for bad in [
        json!("ftp://127.0.0.1/"),
        json!("https://a@api.example.com/"),
        json!(7),
    ] {
        assert_eq!(patch(&org, &a, json!({"searchUrl": bad})).status, 400);
    }
    let r = patch(&org, &a, json!({"searchUrl": null}));
    assert_eq!(r.json()["searchUrl"], Value::Null);
}

#[test]
fn case_21_web_search_asks_searxng_and_caps_its_results() {
    let (org, stub, a, u) = ready();
    let web = Web::start();
    search_on(&org, &a, &web.url("127.0.0.1", ""));
    script(
        &stub,
        vec![
            json!({"calls": [call("s", "web_search", json!({"query": "tea & cake"}))]}),
            json!({"text": "done"}),
        ],
    );
    let ev = send(&org, &u, "find tea");
    let names: Vec<_> = stub.state().seen[0].body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].clone())
        .collect();
    assert_eq!(
        names,
        [
            "make_artifact",
            "update_artifact",
            "web_search",
            "read_page"
        ]
    );
    let hit = web
        .hits()
        .into_iter()
        .find(|h| h.starts_with("/search"))
        .unwrap();
    let q: HashMap<String, String> = url::Url::parse(&format!("http://x{hit}"))
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(
        (q["q"].as_str(), q["format"].as_str()),
        ("tea & cake", "json")
    );
    let got = &results(&stub, 1)[0]["results"];
    let list = got.as_array().unwrap();
    assert_eq!(list.len(), 8, "{got}");
    for (i, r) in list.iter().enumerate() {
        assert_eq!(
            r["url"],
            web.url("pages.test", &format!("/page/html?n={i}"))
        );
        assert_eq!(r["title"].as_str().unwrap().chars().count(), 200);
        assert_eq!(r["snippet"].as_str().unwrap().chars().count(), 300);
    }
    let tool = ev.iter().find(|e| e["type"] == "tool").unwrap();
    assert_eq!(tool["args"], json!({"query": "tea & cake"}));
}

#[test]
fn case_22_a_search_that_fails_is_search_failed() {
    let (org, stub, a, u) = ready();
    let web = Web::start();
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let down = format!("http://127.0.0.1:{}", closed.local_addr().unwrap().port());
    drop(closed);
    for (base, mode) in [
        (down, Mode::Results),
        (web.url("127.0.0.1", ""), Mode::Slow),
        (web.url("127.0.0.1", ""), Mode::NotJson),
    ] {
        web.state.lock().unwrap().mode = mode;
        search_on(&org, &a, &base);
        let at = stub.state().seen.len();
        let mut steps = vec![json!({}); at];
        steps.push(json!({"calls": [call("s", "web_search", json!({"query": "q"}))]}));
        steps.push(json!({"text": "went on"}));
        script(&stub, steps);
        let ev = send(&org, &u, "find");
        assert_eq!(results(&stub, at + 1), [json!({"error": "search_failed"})]);
        let done = ev.last().unwrap();
        assert_eq!((&done["status"], &done["steps"]), (&json!(200), &json!(2)));
    }
}

#[test]
fn case_23_read_page_opens_only_pages_this_send_met() {
    let (org, stub, a, u) = ready();
    let web = Web::start();
    search_on(&org, &a, &web.url("127.0.0.1", ""));
    let found = web.url("pages.test", "/page/html?n=0");
    let written = web.url("pages.test", "/page/plain");
    let neither = web.url("pages.test", "/page/html?n=99");
    script(
        &stub,
        vec![
            json!({"calls": [call("s", "web_search", json!({"query": "q"}))]}),
            json!({"calls": [read("a", &found), read("b", &written), read("c", &neither)]}),
            json!({"text": "done"}),
        ],
    );
    send(&org, &u, &format!("read ({written}), please."));
    let got = results(&stub, 2);
    assert_eq!(got[1]["url"], found);
    assert_eq!(got[1]["title"], "Caf\u{e9} & Co");
    assert_eq!(
        got[2],
        json!({"title": "", "url": written, "text": "line one\n  line two"})
    );
    assert_eq!(got[3], json!({"error": "not_offered"}));
    assert!(
        !web.hits().iter().any(|h| h.ends_with("n=99")),
        "{:?}",
        web.hits()
    );
}

#[test]
fn case_24_read_page_refuses_addresses_that_are_not_public() {
    let (org, stub, a, u) = ready();
    let web = Web::start();
    search_on(&org, &a, &web.url("127.0.0.1", ""));
    let source = add_source(&org, &a, "own", &web.url("src.test", "/v1"), None);
    assert_eq!(source.status, 200, "{}", source.body);
    let p = web.port;
    let blocked = [
        format!("http://127.0.0.1:{p}/page/plain"),
        format!("http://10.0.0.1:{p}/page/plain"),
        format!("http://[::1]:{p}/page/plain"),
        format!("http://169.254.169.254:{p}/page/plain"),
        format!("http://100.64.0.1:{p}/page/plain"),
        format!("http://[fd00::1]:{p}/page/plain"),
        format!("http://[::ffff:127.0.0.1]:{p}/page/plain"),
        format!("http://loop.test:{p}/page/plain"),
        format!("http://tenner.test:{p}/page/plain"),
        format!("http://mixed.test:{p}/page/plain"),
        format!("http://src.test:{p}/page/plain"),
        format!("http://localhost:{p}/page/plain"),
    ];
    let file = "file:///etc/passwd";
    let text = format!("read these: {} {file}", blocked.join(" "));
    let mut urls: Vec<&str> = blocked.iter().map(String::as_str).collect();
    urls.push(file);
    let got = read_all(&org, &stub, &u, &text, &urls);
    let mut want = vec![json!({"error": "blocked_address"}); blocked.len()];
    want.push(json!({"error": "not_offered"}));
    assert_eq!(got, want);
    let pages: Vec<_> = web
        .hits()
        .into_iter()
        .filter(|h| h.starts_with("/page"))
        .collect();
    assert!(pages.is_empty(), "{pages:?}");
}

#[test]
fn case_25_each_redirect_is_checked_and_three_at_most_are_followed() {
    let (org, stub, a, u) = ready();
    let web = Web::start();
    search_on(&org, &a, &web.url("127.0.0.1", ""));
    let urls: Vec<String> = ["/to/loopback", "/to/name", "/to/ftp", "/r/2", "/r/3"]
        .iter()
        .map(|p| web.url("pages.test", p))
        .collect();
    let refs: Vec<&str> = urls.iter().map(String::as_str).collect();
    let got = read_all(&org, &stub, &u, &urls.join(" "), &refs);
    let plain = web.url("pages.test", "/page/plain");
    assert_eq!(
        got,
        [
            json!({"error": "blocked_address"}),
            json!({"error": "blocked_address"}),
            json!({"error": "bad_scheme"}),
            json!({"title": "", "url": plain, "text": "line one\n  line two"}),
            json!({"error": "too_many_redirects"}),
        ]
    );
    let hits = web.hits();
    assert!(!hits.iter().any(|h| h.contains("secret")), "{hits:?}");
    let plain = hits.iter().filter(|h| *h == "/page/plain").count();
    assert_eq!(plain, 1, "{hits:?}");
}

#[test]
fn case_26_pages_are_reduced_to_text_and_cut() {
    let (org, stub, a, u) = ready();
    let web = Web::start();
    search_on(&org, &a, &web.url("127.0.0.1", ""));
    let urls: Vec<String> = ["/page/html", "/page/big", "/page/image"]
        .iter()
        .map(|p| web.url("pages.test", p))
        .collect();
    let refs: Vec<&str> = urls.iter().map(String::as_str).collect();
    let got = read_all(&org, &stub, &u, &urls.join(" "), &refs);
    assert_eq!(got[0]["title"], "Caf\u{e9} & Co");
    assert_eq!(
        got[0]["text"], "Menu Tea <hot> \u{20ac}3 and cake",
        "{}",
        got[0]
    );
    assert_eq!(got[1]["text"], "a".repeat(64 << 10));
    assert_eq!(got[2], json!({"error": "unsupported_type"}));
}

#[test]
fn case_27_privacy_shows_whether_search_is_on() {
    let (org, _stub, a, u) = ready();
    let search = |org: &Org| org.get("/api/org/v1/privacy", Some(&u)).json()["search"].clone();
    assert_eq!(search(&org), false);
    let web = Web::start();
    search_on(&org, &a, &web.url("127.0.0.1", ""));
    assert_eq!(search(&org), true);
    assert_eq!(patch(&org, &a, json!({"searchUrl": null})).status, 200);
    assert_eq!(search(&org), false);
}
