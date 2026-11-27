//! The org server's HTTP routes, the permission table every route is
//! checked against, and the Host, Origin and CSRF guard in front of them.

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Extension, MatchedPath, Path as UrlPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{MethodRouter, get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::store::{JoinRefused, Member, Role, SESSION_IDLE, Store};

pub const COOKIE: &str = "fv_session";
/// Failed join attempts one address may make in `JOIN_WINDOW`.
const JOIN_FAILURES: usize = 10;
const JOIN_WINDOW: Duration = Duration::from_secs(10 * 60);

/// Who may call a route.
#[derive(Debug, Clone, Copy)]
pub enum Allow {
    Anyone,
    Roles(&'static [Role]),
}

const MEMBERS: Allow = Allow::Roles(&[Role::Admin, Role::Full, Role::User, Role::Student]);

/// Every route and who may call it. A route missing here is refused, and
/// the `every_route_has_a_permission` test fails on it.
pub const PERMISSIONS: &[(&str, &str, Allow)] = &[
    ("GET", "/join/{token}", Allow::Anyone),
    ("POST", "/api/org/v1/join", Allow::Anyone),
    ("GET", "/api/org/v1/me", MEMBERS),
    ("POST", "/api/org/v1/logout", MEMBERS),
];

type St = Arc<OrgState>;

fn routes() -> Vec<(Method, &'static str, MethodRouter<St>)> {
    vec![
        (Method::GET, "/join/{token}", get(join_page)),
        (Method::POST, "/api/org/v1/join", post(join)),
        (Method::GET, "/api/org/v1/me", get(me)),
        (Method::POST, "/api/org/v1/logout", post(logout)),
    ]
}

fn permission(method: &Method, path: &str) -> Option<Allow> {
    let method = if method == Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };
    PERMISSIONS
        .iter()
        .find(|(m, p, _)| *m == method && *p == path)
        .map(|(_, _, a)| *a)
}

pub struct OrgState {
    store: Mutex<Store>,
    /// The org URL's origin, as browsers send it in `Origin`.
    origin: String,
    /// The org URL's host, with its port when it isn't the default.
    authority: String,
    secure: bool,
    /// The port this server listens on, when it listens on loopback.
    loopback_port: Option<u16>,
    failures: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
    /// Cloudflare fronts the server, so `CF-Connecting-IP` names the client.
    trust_cf: bool,
}

impl OrgState {
    pub fn new(store: Store, url: &url::Url, listen: SocketAddr, trust_cf: bool) -> OrgState {
        let host = url
            .host_str()
            .expect("org URL has a host")
            .to_ascii_lowercase();
        OrgState {
            store: Mutex::new(store),
            origin: url.origin().ascii_serialization(),
            authority: match url.port() {
                Some(p) => format!("{host}:{p}"),
                None => host,
            },
            secure: url.scheme() == "https",
            loopback_port: listen.ip().is_loopback().then_some(listen.port()),
            failures: Mutex::new(HashMap::new()),
            trust_cf,
        }
    }

    fn host_ok(&self, host: Option<&str>) -> bool {
        let Some(h) = host.map(|h| h.trim().to_ascii_lowercase()) else {
            return false;
        };
        if h == self.authority {
            return true;
        }
        self.loopback_port.is_some_and(|port| {
            ["127.0.0.1", "localhost", "[::1]"]
                .iter()
                .any(|l| h == format!("{l}:{port}"))
        })
    }

    fn cookie(&self, token: &str, max_age: u64) -> HeaderValue {
        let secure = if self.secure { "; Secure" } else { "" };
        HeaderValue::from_str(&format!(
            "{COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}{secure}"
        ))
        .expect("cookie is ASCII")
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True when `ip` has used up its failed join attempts.
    fn join_blocked(&self, ip: IpAddr) -> bool {
        let mut f = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        f.retain(|_, q| {
            while q
                .front()
                .is_some_and(|t| now.duration_since(*t) >= JOIN_WINDOW)
            {
                q.pop_front();
            }
            !q.is_empty()
        });
        f.get(&ip).is_some_and(|q| q.len() >= JOIN_FAILURES)
    }

    /// The address the join limit counts. On loopback a proxy fronts the
    /// server and appends the client it saw as the last `X-Forwarded-For`
    /// entry; entries before it are the client's own words. Cloudflare's
    /// `CF-Connecting-IP` counts only with `--trust-cf`. On a public listener
    /// both headers come from anyone and are ignored.
    fn client_ip(&self, h: &HeaderMap, peer: IpAddr) -> IpAddr {
        if self.loopback_port.is_none() {
            return peer;
        }
        let cf = self
            .trust_cf
            .then(|| header_str(h, "cf-connecting-ip".parse().expect("name")))
            .flatten();
        let xff = h
            .get_all("x-forwarded-for")
            .iter()
            .next_back()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next());
        [cf, xff]
            .into_iter()
            .flatten()
            .find_map(|v| v.trim().parse().ok())
            .unwrap_or(peer)
    }

    fn join_failed(&self, ip: IpAddr) {
        let mut f = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        f.entry(ip).or_default().push_back(Instant::now());
    }
}

pub fn router(st: St) -> Router {
    let mut r = Router::new();
    for (_, path, mr) in routes() {
        r = r.route(path, mr);
    }
    r.layer(middleware::from_fn_with_state(st.clone(), guard))
        .with_state(st)
}

/// An error answer: `{"error": code}`.
pub fn err(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({ "error": code }))).into_response()
}

fn internal(e: anyhow::Error) -> Response {
    eprintln!("fonsviva org: {e:#}");
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal")
}

fn header_str(h: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

fn session_cookie(h: &HeaderMap) -> Option<String> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(COOKIE)?.strip_prefix('='))
        .filter(|t| is_token(t))
        .map(str::to_string)
}

fn is_token(t: &str) -> bool {
    t.len() == 64 && t.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The signed-in member and the session token, for handlers behind `guard`.
#[derive(Clone)]
pub struct Session {
    pub member: Member,
    token: String,
}

async fn guard(State(st): State<St>, mut req: Request, next: Next) -> Response {
    let h = req.headers();
    if !st.host_ok(header_str(h, header::HOST)) {
        return err(StatusCode::MISDIRECTED_REQUEST, "misdirected");
    }
    if let Some(o) = h.get(header::ORIGIN)
        && !o
            .to_str()
            .is_ok_and(|o| o.trim().eq_ignore_ascii_case(&st.origin))
    {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let safe = matches!(*req.method(), Method::GET | Method::HEAD);
    if !safe && header_str(h, "x-fonsviva-request".parse().expect("name")) != Some("1") {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let Some(path) = req.extensions().get::<MatchedPath>() else {
        return next.run(req).await;
    };
    let roles = match permission(req.method(), path.as_str()) {
        None => return err(StatusCode::FORBIDDEN, "forbidden"),
        Some(Allow::Anyone) => return next.run(req).await,
        Some(Allow::Roles(roles)) => roles,
    };
    let Some(token) = session_cookie(req.headers()) else {
        return err(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    let member = match st.store().session(&token) {
        Ok(Some(m)) => m,
        Ok(None) => return err(StatusCode::UNAUTHORIZED, "unauthorized"),
        Err(e) => return internal(e),
    };
    if !roles.contains(&member.role) {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    req.extensions_mut().insert(Session {
        member,
        token: token.clone(),
    });
    let mut r = next.run(req).await;
    if !r.headers().contains_key(header::SET_COOKIE) {
        r.headers_mut()
            .insert(header::SET_COOKIE, st.cookie(&token, SESSION_IDLE));
    }
    r
}

/// Reads a JSON body; anything that doesn't parse is a 400.
pub fn body<T: DeserializeOwned>(b: &Bytes) -> Option<T> {
    serde_json::from_slice(b).ok()
}

fn member_json(m: &Member) -> serde_json::Value {
    json!({ "id": m.id, "name": m.name, "role": m.role.as_str() })
}

const JOIN_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="referrer" content="same-origin"><title>Join</title></head>
<body>
<form id="f"><label>Your name <input id="n" required maxlength="100" autocomplete="name"></label> <button>Join</button></form>
<p id="m"></p>
<script>
document.getElementById("f").onsubmit = async (e) => {
  e.preventDefault();
  const r = await fetch("/api/org/v1/join", {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-Fonsviva-Request": "1" },
    referrerPolicy: "same-origin",
    body: JSON.stringify({ token: "TOKEN", name: document.getElementById("n").value }),
  });
  document.getElementById("m").textContent =
    r.ok ? "You're in." : r.status === 429 ? "Too many tries. Wait ten minutes." : "This link doesn't work. Ask for a new one.";
};
</script>
</body></html>
"#;

async fn join_page(UrlPath(token): UrlPath<String>) -> Response {
    let no_store = [(header::CACHE_CONTROL, "no-store")];
    if !is_token(&token) {
        let page = "<!doctype html><p>This link doesn't work. Ask for a new one.</p>";
        return (StatusCode::GONE, no_store, Html(page)).into_response();
    }
    (no_store, Html(JOIN_PAGE.replace("TOKEN", &token))).into_response()
}

#[derive(serde::Deserialize)]
struct JoinReq {
    token: String,
    name: String,
}

async fn join(
    State(st): State<St>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    b: Bytes,
) -> Response {
    let ip = st.client_ip(&headers, peer.ip());
    if st.join_blocked(ip) {
        return err(StatusCode::TOO_MANY_REQUESTS, "too_many_requests");
    }
    let Some(r): Option<JoinReq> = body(&b) else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    let name = r.name.trim();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    }
    let joined = if is_token(&r.token) {
        st.store().join(&r.token, name)
    } else {
        Ok(Err(JoinRefused::Gone))
    };
    match joined {
        Ok(Ok((member, session))) => {
            let cookie = [(header::SET_COOKIE, st.cookie(&session, SESSION_IDLE))];
            (cookie, Json(member_json(&member))).into_response()
        }
        Ok(Err(JoinRefused::Gone)) => {
            st.join_failed(ip);
            err(StatusCode::GONE, "gone")
        }
        Err(e) => internal(e),
    }
}

async fn me(Extension(s): Extension<Session>) -> Response {
    Json(member_json(&s.member)).into_response()
}

async fn logout(State(st): State<St>, Extension(s): Extension<Session>) -> Response {
    if let Err(e) = st.store().end_session(&s.token) {
        return internal(e);
    }
    let cookie = [(header::SET_COOKIE, st.cookie("", 0))];
    (cookie, Json(json!({}))).into_response()
}

/// Serves until Ctrl-C or SIGTERM.
pub async fn run(listener: tokio::net::TcpListener, st: St) -> Result<()> {
    let app = router(st).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("SIGTERM handler");
                tokio::select! {
                    r = tokio::signal::ctrl_c() => r.expect("Ctrl-C handler"),
                    _ = term.recv() => {}
                }
            }
            #[cfg(not(unix))]
            tokio::signal::ctrl_c().await.expect("Ctrl-C handler");
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_has_a_permission() {
        let routes = routes();
        for (m, p, _) in &routes {
            assert!(
                permission(m, p).is_some(),
                "{m} {p} has no entry in PERMISSIONS"
            );
        }
        for (m, p, _) in PERMISSIONS {
            assert!(
                routes
                    .iter()
                    .any(|(rm, rp, _)| rm.as_str() == *m && rp == p),
                "PERMISSIONS lists {m} {p}, which no route serves"
            );
        }
    }
}
