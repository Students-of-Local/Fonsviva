//! The org server's HTTP routes, the permission table every route is
//! checked against, and the Host, Origin and CSRF guard in front of them.

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Extension, MatchedPath, Path as UrlPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{MethodRouter, delete, get, patch, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fonsviva_core::util::rfc3339;

use super::sources::{self, Sources};
use super::store::{
    AddRefused, JoinRefused, Member, MemberRefused, Role, SESSION_IDLE, SourceKey, Store,
};

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
const ADMINS: Allow = Allow::Roles(&[Role::Admin]);

/// Every route and who may call it. A route missing here is refused, and
/// the `every_route_has_a_permission` test fails on it.
pub const PERMISSIONS: &[(&str, &str, Allow)] = &[
    ("GET", "/join/{token}", Allow::Anyone),
    ("POST", "/api/org/v1/join", Allow::Anyone),
    ("GET", "/api/org/v1/me", MEMBERS),
    ("POST", "/api/org/v1/logout", MEMBERS),
    ("GET", "/api/org/v1/members", ADMINS),
    ("PATCH", "/api/org/v1/members/{id}", ADMINS),
    ("GET", "/api/org/v1/invites", ADMINS),
    ("POST", "/api/org/v1/invites", ADMINS),
    ("DELETE", "/api/org/v1/invites/{id}", ADMINS),
    ("GET", "/api/org/v1/audit", ADMINS),
    ("GET", "/api/org/v1/sources", ADMINS),
    ("POST", "/api/org/v1/sources", ADMINS),
    ("DELETE", "/api/org/v1/sources/{id}", ADMINS),
    ("GET", "/api/org/v1/models", MEMBERS),
    ("GET", "/api/org/v1/settings", ADMINS),
    ("PATCH", "/api/org/v1/settings", ADMINS),
];

type St = Arc<OrgState>;

fn routes() -> Vec<(Method, &'static str, MethodRouter<St>)> {
    vec![
        (Method::GET, "/join/{token}", get(join_page)),
        (Method::POST, "/api/org/v1/join", post(join)),
        (Method::GET, "/api/org/v1/me", get(me)),
        (Method::POST, "/api/org/v1/logout", post(logout)),
        (Method::GET, "/api/org/v1/members", get(members)),
        (
            Method::PATCH,
            "/api/org/v1/members/{id}",
            patch(update_member),
        ),
        (Method::GET, "/api/org/v1/invites", get(invites)),
        (Method::POST, "/api/org/v1/invites", post(create_invite)),
        (
            Method::DELETE,
            "/api/org/v1/invites/{id}",
            delete(revoke_invite),
        ),
        (Method::GET, "/api/org/v1/audit", get(audit)),
        (Method::GET, "/api/org/v1/sources", get(list_sources)),
        (Method::POST, "/api/org/v1/sources", post(add_source)),
        (
            Method::DELETE,
            "/api/org/v1/sources/{id}",
            delete(remove_source),
        ),
        (Method::GET, "/api/org/v1/models", get(models)),
        (Method::GET, "/api/org/v1/settings", get(settings)),
        (
            Method::PATCH,
            "/api/org/v1/settings",
            patch(update_settings),
        ),
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
    /// The org URL, with no trailing slash, that join links start with.
    url: String,
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
    sources: Sources,
}

impl OrgState {
    pub fn new(store: Store, url: &url::Url, listen: SocketAddr, trust_cf: bool) -> OrgState {
        let host = url
            .host_str()
            .expect("org URL has a host")
            .to_ascii_lowercase();
        OrgState {
            store: Mutex::new(store),
            url: url.as_str().trim_end_matches('/').to_string(),
            origin: url.origin().ascii_serialization(),
            authority: match url.port() {
                Some(p) => format!("{host}:{p}"),
                None => host,
            },
            secure: url.scheme() == "https",
            loopback_port: listen.ip().is_loopback().then_some(listen.port()),
            failures: Mutex::new(HashMap::new()),
            trust_cf,
            sources: Sources::new(),
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

fn json_or_500<T: serde::Serialize>(r: Result<T>) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

async fn members(State(st): State<St>) -> Response {
    json_or_500(st.store().members())
}

#[derive(serde::Deserialize)]
struct MemberPatch {
    role: Option<String>,
    disabled: Option<bool>,
}

async fn update_member(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    UrlPath(id): UrlPath<String>,
    b: Bytes,
) -> Response {
    let Some(p): Option<MemberPatch> = body(&b) else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    let role = match p.role.as_deref().map(Role::parse) {
        Some(None) => return err(StatusCode::BAD_REQUEST, "bad_request"),
        Some(r) => r,
        None => None,
    };
    if role.is_none() && p.disabled.is_none() {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    }
    match st
        .store()
        .update_member(&s.member.id, &id, role, p.disabled)
    {
        Ok(Ok(())) if id == s.member.id && p.disabled == Some(true) => {
            let cookie = [(header::SET_COOKIE, st.cookie("", 0))];
            (cookie, Json(json!({}))).into_response()
        }
        Ok(Ok(())) => Json(json!({})).into_response(),
        Ok(Err(MemberRefused::NotFound)) => err(StatusCode::NOT_FOUND, "not_found"),
        Ok(Err(MemberRefused::LastAdmin)) => err(StatusCode::CONFLICT, "last_admin"),
        Err(e) => internal(e),
    }
}

async fn invites(State(st): State<St>) -> Response {
    json_or_500(st.store().invites())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct InviteReq {
    role: String,
    expires_hours: u64,
    uses: u32,
}

async fn create_invite(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    b: Bytes,
) -> Response {
    let Some(r): Option<InviteReq> = body(&b) else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    let Some(role) = Role::parse(&r.role) else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    if !(1..=720).contains(&r.expires_hours) || !(1..=500).contains(&r.uses) {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    }
    let made = st
        .store()
        .create_invite(role, r.expires_hours, r.uses, &s.member.id);
    match made {
        Ok((id, token)) => {
            Json(json!({ "id": id, "link": format!("{}/join/{token}", st.url) })).into_response()
        }
        Err(e) => internal(e),
    }
}

async fn revoke_invite(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    match st.store().revoke_invite(&s.member.id, &id) {
        Ok(true) => Json(json!({})).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e),
    }
}

async fn audit(State(st): State<St>) -> Response {
    json_or_500(st.store().audit_log())
}

async fn list_sources(State(st): State<St>) -> Response {
    let list = match st.store().sources() {
        Ok(l) => l,
        Err(e) => return internal(e),
    };
    let rows: Vec<_> = list
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "name": s.name,
                "baseUrl": s.base_url,
                "local": s.local,
                "created": rfc3339(s.created.max(0) as u64),
                "lastError": match s.api_key {
                    SourceKey::Unreadable => Some(sources::UNREADABLE.to_string()),
                    _ => st.sources.last_error(&s.id),
                },
            })
        })
        .collect();
    Json(rows).into_response()
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceReq {
    name: String,
    base_url: String,
    api_key: Option<String>,
}

async fn add_source(State(st): State<St>, Extension(s): Extension<Session>, b: Bytes) -> Response {
    let Some(r): Option<SourceReq> = body(&b) else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    let key_ok = r
        .api_key
        .as_deref()
        .is_none_or(|k| (1..=4096).contains(&k.len()) && k.bytes().all(|b| b.is_ascii_graphic()));
    let Some(url) =
        sources::parse_base(&r.base_url).filter(|_| sources::name_ok(&r.name) && key_ok)
    else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    let base_url = url.as_str().trim_end_matches('/').to_string();
    let local = sources::is_local(&url).await;
    if !local && url.scheme() != "https" {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    }
    let added = st.store().add_source(
        &s.member.id,
        &r.name,
        &base_url,
        r.api_key.as_deref(),
        local,
    );
    match added {
        Ok(Ok(id)) => Json(json!({
            "id": id,
            "name": r.name,
            "baseUrl": base_url,
            "local": local,
            "notice": sources::notice(&url, local),
        }))
        .into_response(),
        Ok(Err(AddRefused::NameTaken)) => err(StatusCode::CONFLICT, "name_taken"),
        Ok(Err(AddRefused::SecretsKeyMissing)) => err(StatusCode::CONFLICT, "secrets_key_missing"),
        Ok(Err(AddRefused::SecretsKeyBad)) => err(StatusCode::CONFLICT, "secrets_key_bad"),
        Err(e) => internal(e),
    }
}

async fn remove_source(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    match st.store().remove_source(&s.member.id, &id) {
        Ok(true) => {
            st.sources.forget(&id);
            Json(json!({})).into_response()
        }
        Ok(false) => err(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e),
    }
}

/// Students chat only when the org allows it.
fn may_chat(st: &OrgState, m: &Member) -> Result<bool> {
    Ok(m.role != Role::Student || st.store().settings()?.students_allowed)
}

async fn models(State(st): State<St>, Extension(s): Extension<Session>) -> Response {
    match listed(&st, &s.member).await {
        Ok(list) => {
            let rows: Vec<_> = list
                .iter()
                .map(|(src, m)| json!({"id": format!("{src}/{m}"), "source": src, "model": m}))
                .collect();
            Json(rows).into_response()
        }
        Err(e) => internal(e),
    }
}

/// Every model the member may use, as source name and upstream id. A source
/// that fails is left out.
async fn listed(st: &OrgState, m: &Member) -> Result<Vec<(String, String)>> {
    if !may_chat(st, m)? {
        return Ok(vec![]);
    }
    let list = st.store().sources()?;
    st.sources
        .keep(&list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>());
    let asked = futures_util::future::join_all(list.iter().map(|s| st.sources.models(s))).await;
    Ok(list
        .iter()
        .zip(asked)
        .flat_map(|(s, r)| {
            r.unwrap_or_default()
                .into_iter()
                .map(|m| (s.name.clone(), m))
        })
        .collect())
}

async fn settings(State(st): State<St>) -> Response {
    json_or_500(st.store().settings())
}

async fn update_settings(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    b: Bytes,
) -> Response {
    let Some(p): Option<serde_json::Map<String, serde_json::Value>> = body(&b) else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    let students = match p.get("studentsAllowed") {
        None => None,
        Some(v) => match v.as_bool() {
            Some(b) => Some(b),
            None => return err(StatusCode::BAD_REQUEST, "bad_request"),
        },
    };
    if students.is_none() {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    }
    json_or_500(st.store().update_settings(&s.member.id, students))
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

    /// Each route with no entry in `table`, and each entry no route serves.
    fn gaps(table: &[(&str, &str, Allow)]) -> Vec<String> {
        let routes = routes();
        let mut out = vec![];
        for (m, p, _) in &routes {
            if !table.iter().any(|(tm, tp, _)| *tm == m.as_str() && tp == p) {
                out.push(format!("{m} {p} has no entry in PERMISSIONS"));
            }
        }
        for (m, p, _) in table {
            if !routes
                .iter()
                .any(|(rm, rp, _)| rm.as_str() == *m && rp == p)
            {
                out.push(format!("PERMISSIONS lists {m} {p}, which no route serves"));
            }
        }
        out
    }

    #[test]
    fn every_route_has_a_permission() {
        assert_eq!(gaps(PERMISSIONS), Vec::<String>::new());
        for (m, p, _) in PERMISSIONS {
            let without: Vec<_> = PERMISSIONS
                .iter()
                .filter(|(tm, tp, _)| (tm, tp) != (m, p))
                .copied()
                .collect();
            assert_eq!(
                gaps(&without),
                [format!("{m} {p} has no entry in PERMISSIONS")]
            );
        }
    }

    #[tokio::test]
    async fn the_model_cache_keeps_only_current_sources() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::create(&d.path().join("org.db"), "https://org.example").unwrap();
        let url = "https://org.example".parse().unwrap();
        let st = OrgState::new(store, &url, "127.0.0.1:0".parse().unwrap(), false);
        let gone = super::super::store::Source {
            id: "src_gone".into(),
            name: "gone".into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: SourceKey::None,
            local: true,
            created: 0,
        };
        assert!(st.sources.models(&gone).await.is_err());
        assert!(st.sources.last_error("src_gone").is_some());
        let admin = Member {
            id: "mem_a".into(),
            name: "a".into(),
            role: Role::Admin,
        };
        listed(&st, &admin).await.unwrap();
        assert_eq!(st.sources.last_error("src_gone"), None);
    }
}
