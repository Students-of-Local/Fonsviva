//! The org server's HTTP routes, the permission table every route is
//! checked against, and the Host, Origin and CSRF guard in front of them.

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{
    ConnectInfo, DefaultBodyLimit, Extension, MatchedPath, Path as UrlPath, Query, Request, State,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{MethodRouter, delete, get, patch, post, put};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fonsviva_core::util::rfc3339;

use super::chat;
use super::costs::{self, Pricing};
use super::limits;
use super::sources::{self, Sources};
use super::store::{
    AddRefused, By, DAY, JoinRefused, MERGES, Member, MemberRefused, Role, SESSION_IDLE,
    SettingsPatch, SourceKey, Store, Usage,
};

pub const COOKIE: &str = "fv_session";
/// Failed join attempts one address may make in `JOIN_WINDOW`.
const JOIN_FAILURES: usize = 10;
const JOIN_WINDOW: Duration = Duration::from_secs(10 * 60);
const CHAT_LIMIT: usize = 16 << 20;
/// The largest thread send: its text and a history, each at its limit with
/// every character escaped as `\u00XX` (six bytes), and room for JSON.
const SEND_LIMIT: usize = 4 << 20;
/// The longest `model` a chat may name.
const MODEL_LIMIT: usize = 200;
/// Messages one `GET /thread` returns at most, and by default.
const PAGE_LIMIT: i64 = 100;
const PAGE_DEFAULT: i64 = 50;

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
    ("PATCH", "/api/org/v1/sources/{id}", ADMINS),
    ("DELETE", "/api/org/v1/sources/{id}", ADMINS),
    ("GET", "/api/org/v1/models", MEMBERS),
    ("POST", "/api/org/v1/chat/completions", MEMBERS),
    ("GET", "/api/org/v1/settings", ADMINS),
    ("PATCH", "/api/org/v1/settings", ADMINS),
    ("POST", "/api/org/v1/keys", MEMBERS),
    ("GET", "/api/org/v1/keys", MEMBERS),
    ("DELETE", "/api/org/v1/keys/{id}", MEMBERS),
    ("GET", "/api/org/v1/usage", ADMINS),
    ("GET", "/api/org/v1/usage/totals", ADMINS),
    ("GET", "/api/org/v1/me/usage", MEMBERS),
    ("GET", "/api/org/v1/thread", MEMBERS),
    ("DELETE", "/api/org/v1/thread", MEMBERS),
    ("POST", "/api/org/v1/thread/messages", MEMBERS),
    ("DELETE", "/api/org/v1/thread/messages/{seq}", MEMBERS),
    ("GET", "/api/org/v1/thread/search", MEMBERS),
    ("GET", "/api/org/v1/members/{id}/thread", ADMINS),
    ("GET", "/api/org/v1/members/{id}/limits", ADMINS),
    ("PUT", "/api/org/v1/members/{id}/limits", ADMINS),
    ("GET", "/api/org/v1/me/limits", MEMBERS),
    ("GET", "/api/org/v1/privacy", MEMBERS),
    ("GET", "/v1/models", MEMBERS),
    ("POST", "/v1/chat/completions", MEMBERS),
];

pub(super) type St = Arc<OrgState>;

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
            Method::PATCH,
            "/api/org/v1/sources/{id}",
            patch(update_source),
        ),
        (
            Method::DELETE,
            "/api/org/v1/sources/{id}",
            delete(remove_source),
        ),
        (Method::GET, "/api/org/v1/models", get(models)),
        (
            Method::POST,
            "/api/org/v1/chat/completions",
            post(chat_route).layer(DefaultBodyLimit::max(CHAT_LIMIT)),
        ),
        (Method::GET, "/api/org/v1/settings", get(settings)),
        (
            Method::PATCH,
            "/api/org/v1/settings",
            patch(update_settings),
        ),
        (Method::POST, "/api/org/v1/keys", post(create_key)),
        (Method::GET, "/api/org/v1/keys", get(keys)),
        (Method::DELETE, "/api/org/v1/keys/{id}", delete(revoke_key)),
        (Method::GET, "/api/org/v1/usage", get(all_usage)),
        (Method::GET, "/api/org/v1/usage/totals", get(totals)),
        (Method::GET, "/api/org/v1/me/usage", get(my_usage)),
        (Method::GET, "/api/org/v1/thread", get(thread)),
        (Method::DELETE, "/api/org/v1/thread", delete(delete_thread)),
        (
            Method::POST,
            "/api/org/v1/thread/messages",
            post(super::send::send).layer(DefaultBodyLimit::max(SEND_LIMIT)),
        ),
        (
            Method::DELETE,
            "/api/org/v1/thread/messages/{seq}",
            delete(delete_message),
        ),
        (Method::GET, "/api/org/v1/thread/search", get(search)),
        (
            Method::GET,
            "/api/org/v1/members/{id}/thread",
            get(member_thread),
        ),
        (
            Method::GET,
            "/api/org/v1/members/{id}/limits",
            get(limits::member_limits),
        ),
        (
            Method::PUT,
            "/api/org/v1/members/{id}/limits",
            put(limits::put_limits),
        ),
        (Method::GET, "/api/org/v1/me/limits", get(limits::my_limits)),
        (Method::GET, "/api/org/v1/privacy", get(privacy)),
        (Method::GET, "/v1/models", get(v1_models)),
        (
            Method::POST,
            "/v1/chat/completions",
            post(v1_chat).layer(DefaultBodyLimit::max(CHAT_LIMIT)),
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
    /// Chat requests of limited members that passed the daily limit check
    /// and haven't written their usage row yet, and the tokens they hold.
    in_flight: Mutex<HashMap<String, (usize, i64)>>,
    /// Members with a thread send running.
    pub(super) sending: Mutex<HashSet<String>>,
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
            in_flight: Mutex::new(HashMap::new()),
            sending: Mutex::new(HashSet::new()),
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

    pub(super) fn store(&self) -> std::sync::MutexGuard<'_, Store> {
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

pub(super) fn internal(e: anyhow::Error) -> Response {
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

/// Who a request acts as: the member, and the personal key when it came
/// through the `/v1` door.
#[derive(Clone)]
pub struct Caller {
    pub member: Member,
    key: Option<String>,
}

/// An error in OpenAI's shape, for the `/v1` door: `type` is OpenAI's
/// category and `code` is ours.
pub fn oai_err(status: StatusCode, code: &str) -> Response {
    let kind = match code {
        "daily_limit" => "insufficient_quota",
        "too_many_in_flight" => "rate_limit_error",
        "upstream" | "upstream_auth" | "internal" => "server_error",
        _ => "invalid_request_error",
    };
    let message = match code {
        "invalid_api_key" => "Missing or unknown API key.",
        "daily_limit" => "You have used today's tokens.",
        "too_many_in_flight" => "Too many requests at once; wait for one to finish.",
        "students_off" => "Students can't chat in this org yet.",
        "unknown_model" => "No such model.",
        "model_not_allowed" => "This model isn't open to you.",
        "outside_hours" => "Chat is closed for you at this hour.",
        "upstream" => "The model source can't be reached.",
        "upstream_auth" => "The model source refused the org's key.",
        "too_large" => "The request is too large.",
        "bad_request" => "The request is not valid.",
        "misdirected" => "This server doesn't answer for that host.",
        "forbidden" => "Not allowed.",
        "not_found" => "No such route.",
        _ => "Something went wrong.",
    };
    (
        status,
        Json(json!({ "error": { "message": message, "type": kind, "param": null, "code": code } })),
    )
        .into_response()
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    let (scheme, k) = header_str(h, header::AUTHORIZATION)?
        .trim()
        .split_once(|c: char| c.is_ascii_whitespace())?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let k = k.trim();
    is_token(k.strip_prefix("fvk_")?).then_some(k)
}

/// The `/v1` door takes a personal key and no cookie, Origin or CSRF header,
/// and answers every error in OpenAI's shape.
async fn guard(State(st): State<St>, mut req: Request, next: Next) -> Response {
    let door = req.uri().path().starts_with("/v1/");
    let fail = |status, code| {
        if door {
            oai_err(status, code)
        } else {
            err(status, code)
        }
    };
    let h = req.headers();
    if !st.host_ok(header_str(h, header::HOST)) {
        return fail(StatusCode::MISDIRECTED_REQUEST, "misdirected");
    }
    if !door
        && let Some(o) = h.get(header::ORIGIN)
        && !o
            .to_str()
            .is_ok_and(|o| o.trim().eq_ignore_ascii_case(&st.origin))
    {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let safe = matches!(*req.method(), Method::GET | Method::HEAD);
    if !door && !safe && header_str(h, "x-fonsviva-request".parse().expect("name")) != Some("1") {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }
    let Some(path) = req.extensions().get::<MatchedPath>() else {
        if door {
            return oai_err(StatusCode::NOT_FOUND, "not_found");
        }
        return next.run(req).await;
    };
    let roles = match permission(req.method(), path.as_str()) {
        None => return fail(StatusCode::FORBIDDEN, "forbidden"),
        Some(Allow::Anyone) => return next.run(req).await,
        Some(Allow::Roles(roles)) => roles,
    };
    if door {
        let found = match bearer(req.headers()) {
            Some(k) => st.store().key_member(k),
            None => Ok(None),
        };
        let (member, key) = match found {
            Ok(Some(f)) => f,
            Ok(None) => return oai_err(StatusCode::UNAUTHORIZED, "invalid_api_key"),
            Err(e) => {
                eprintln!("fonsviva org: {e:#}");
                return oai_err(StatusCode::INTERNAL_SERVER_ERROR, "internal");
            }
        };
        if !roles.contains(&member.role) {
            return oai_err(StatusCode::FORBIDDEN, "forbidden");
        }
        req.extensions_mut().insert(Caller {
            member,
            key: Some(key),
        });
        return next.run(req).await;
    }
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
    req.extensions_mut().insert(Caller {
        member: member.clone(),
        key: None,
    });
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
                "pricing": s.pricing.as_ref().map(Pricing::json),
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

/// Sets or clears a source's pricing: the body is `{pricing}` alone.
async fn update_source(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    UrlPath(id): UrlPath<String>,
    b: Bytes,
) -> Response {
    let p = body::<serde_json::Map<String, serde_json::Value>>(&b)
        .filter(|m| m.len() == 1)
        .and_then(|m| Pricing::parse(m.get("pricing")?));
    let Some(p) = p else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    match st.store().set_pricing(&s.member.id, &id, p.as_ref()) {
        Ok(true) => Json(json!({ "pricing": p.as_ref().map(Pricing::json) })).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "not_found"),
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

async fn models(State(st): State<St>, Extension(c): Extension<Caller>) -> Response {
    match listed(&st, &c.member).await {
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
    let (lim, _) = limits::of(st, m)?;
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
        .filter(|(s, m)| {
            lim.models
                .as_ref()
                .is_none_or(|ok| ok.contains(&format!("{s}/{m}")))
        })
        .collect())
}

async fn chat_route(
    State(st): State<St>,
    Extension(c): Extension<Caller>,
    b: Result<Bytes, BytesRejection>,
) -> Response {
    proxy(&st, &c, b, None)
        .await
        .unwrap_or_else(|(status, code)| retry_after(&st, &c.member, code, err(status, code)))
}

async fn v1_models(State(st): State<St>, Extension(c): Extension<Caller>) -> Response {
    match listed(&st, &c.member).await {
        Ok(list) => {
            let data: Vec<_> = list
                .iter()
                .map(|(src, m)| json!({"id": format!("{src}/{m}"), "object": "model", "owned_by": src}))
                .collect();
            Json(json!({"object": "list", "data": data})).into_response()
        }
        Err(e) => {
            eprintln!("fonsviva org: {e:#}");
            oai_err(StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    }
}

async fn v1_chat(
    State(st): State<St>,
    Extension(c): Extension<Caller>,
    b: Result<Bytes, BytesRejection>,
) -> Response {
    proxy(&st, &c, b, None)
        .await
        .unwrap_or_else(|(status, code)| retry_after(&st, &c.member, code, oai_err(status, code)))
}

/// A daily limit answer says when the next UTC day starts, and an
/// outside hours answer when the member's next window opens.
pub(super) fn retry_after(st: &OrgState, m: &Member, code: &str, mut r: Response) -> Response {
    let wait = match code {
        "daily_limit" => {
            let now = fonsviva_core::util::unix_now();
            DAY - now % DAY
        }
        "outside_hours" => {
            let shut = limits::of(st, m)
                .ok()
                .and_then(|(l, s)| limits::shut(&l, &s, limits::now()));
            match shut {
                Some(Some(w)) => w.max(1) as u64,
                _ => return r,
            }
        }
        "too_many_in_flight" => 1,
        _ => return r,
    };
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(wait));
    r
}

/// What a thread send asks of the proxy: run once every check has passed,
/// just before the chat goes upstream.
pub(super) type Sending<'a> =
    &'a mut (dyn FnMut() -> std::result::Result<(), (StatusCode, &'static str)> + Send);

/// The chat proxy behind both doors and the thread. `Err` is a status and
/// an error code.
pub(super) async fn proxy(
    st: &St,
    c: &Caller,
    b: Result<Bytes, BytesRejection>,
    thread: Option<Sending<'_>>,
) -> std::result::Result<Response, (StatusCode, &'static str)> {
    let fail = |e: anyhow::Error| {
        eprintln!("fonsviva org: {e:#}");
        (StatusCode::INTERNAL_SERVER_ERROR, "internal")
    };
    let b = b.map_err(|r| match r.status() {
        StatusCode::PAYLOAD_TOO_LARGE => (StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
        _ => (StatusCode::BAD_REQUEST, "bad_request"),
    })?;
    let m = &c.member;
    if !may_chat(st, m).map_err(fail)? {
        return Err((StatusCode::FORBIDDEN, "students_off"));
    }
    let (lim, settings) = limits::of(st, m).map_err(fail)?;
    if limits::shut(&lim, &settings, limits::now()).is_some() {
        return Err((StatusCode::FORBIDDEN, "outside_hours"));
    }
    let mut req: serde_json::Map<String, serde_json::Value> =
        body(&b).ok_or((StatusCode::BAD_REQUEST, "bad_request"))?;
    let id = req
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "bad_request"))?
        .to_string();
    if id.len() > MODEL_LIMIT {
        return Err((StatusCode::BAD_REQUEST, "bad_request"));
    }
    if lim.models.as_ref().is_some_and(|ok| !ok.contains(&id)) {
        return Err((StatusCode::FORBIDDEN, "model_not_allowed"));
    }
    let sent = prompt(&req);
    let reserved = reserve(st, m, lim.daily_tokens, sent, completion(&req)).map_err(fail)??;
    let unknown = (StatusCode::NOT_FOUND, "unknown_model");
    let (name, upstream) = chat::split_model(&id).ok_or(unknown)?;
    let source = st.store().sources().map_err(fail)?;
    let source = source.into_iter().find(|s| s.name == name).ok_or(unknown)?;
    let upstream_down = (StatusCode::BAD_GATEWAY, "upstream");
    let upstream_auth = (StatusCode::BAD_GATEWAY, "upstream_auth");
    let started = Instant::now();
    let reserved = Arc::new(reserved);
    let arm = |model: String, on_drop: bool| {
        let st = st.clone();
        let reserved = reserved.clone();
        let member = m.id.clone();
        let key = c.key.clone();
        let write = move |status: u16, p: Option<i64>, c: Option<i64>, got: Option<u64>| {
            let _reserved = reserved;
            let fill = got.is_some() || status == chat::CLIENT_GONE;
            let guess = fill && (p.is_none() || c.is_none());
            let (p, c) = if fill {
                (p.or(Some(sent)), c.or(Some(estimate(got.unwrap_or(0)))))
            } else {
                (p, c)
            };
            let u = Usage {
                member: &member,
                key: key.as_deref(),
                model: &model,
                status,
                prompt_tokens: p,
                completion_tokens: c,
                estimated: guess,
                ms: started.elapsed().as_millis() as i64,
            };
            if let Err(e) = st.store().record_usage(&u) {
                eprintln!("fonsviva org: usage: {e:#}");
            }
        };
        if on_drop {
            chat::Record::new(write)
        } else {
            chat::Record::quiet(write)
        }
    };
    let early = arm(source.name.clone(), false);
    let offered = match st.sources.offered(&source).await {
        Ok(o) => o,
        Err(e) => {
            early.done(chat::UPSTREAM_DOWN, None, None, None);
            return Err(if e.auth { upstream_auth } else { upstream_down });
        }
    };
    early.skip();
    if !offered.iter().any(|o| o == upstream) {
        return Err(unknown);
    }
    if let Some(send) = thread {
        send()?;
    }
    let rec = arm(id.clone(), true);
    req.insert("model".into(), upstream.into());
    let mut strip = false;
    if req.get("stream").and_then(|v| v.as_bool()) == Some(true) {
        let opts = req.entry("stream_options").or_insert_with(|| json!({}));
        if !opts.is_object() {
            *opts = json!({});
        }
        if let Some(o) = opts.as_object_mut() {
            strip = o.get("include_usage").and_then(|v| v.as_bool()) != Some(true);
            o.insert("include_usage".into(), json!(true));
        }
    }
    let body = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => {
            rec.skip();
            return Err(fail(e.into()));
        }
    };
    let r = st
        .sources
        .forward(&source, body, strip, rec)
        .await
        .map_err(|()| upstream_down)?;
    if matches!(r.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(upstream_auth);
    }
    Ok(r)
}

/// Chat requests a member held to the daily limit may have in flight.
const IN_FLIGHT: usize = 4;

/// The completion a request may ask for when it names no limit.
const DEFAULT_COMPLETION: i64 = 1024;

/// The prompt tokens of each image part, and the least of an audio or
/// file part.
const MEDIA_PART: i64 = 85;

/// Tokens for `bytes` of text, at four bytes a token.
fn estimate(bytes: u64) -> i64 {
    bytes.div_ceil(4).min(i64::MAX as u64) as i64
}

/// The prompt a request may cost, whatever the size of the body: four bytes
/// a token of the `tools`, `functions`, `tool_choice` and `response_format`
/// it sends, the text and tool arguments in its `messages` and each content
/// part as written; 85 tokens for each `image_url` part; and for each
/// `input_audio` or `file` part, 85 or a token for each 16 bytes of it,
/// whichever is more.
fn prompt(req: &serde_json::Map<String, serde_json::Value>) -> i64 {
    let size = |v: &serde_json::Value| v.to_string().len() as u64;
    let fields = ["tools", "functions", "tool_choice", "response_format"];
    let mut bytes: u64 = fields.iter().filter_map(|k| req.get(*k)).map(size).sum();
    let mut media = 0i64;
    let msgs = req.get("messages").and_then(|v| v.as_array());
    for msg in msgs.into_iter().flatten() {
        match &msg["content"] {
            serde_json::Value::String(t) => bytes += t.len() as u64,
            serde_json::Value::Array(parts) => {
                for part in parts {
                    media = media.saturating_add(match part["type"].as_str() {
                        Some("image_url") => MEDIA_PART,
                        Some("input_audio" | "file") => {
                            MEDIA_PART.max((size(part) / 16).min(i64::MAX as u64) as i64)
                        }
                        _ => {
                            bytes += size(part);
                            0
                        }
                    });
                }
            }
            _ => {}
        }
        for call in msg["tool_calls"].as_array().into_iter().flatten() {
            let args = call["function"]["arguments"].as_str();
            bytes += args.map_or(0, |t| t.len() as u64);
        }
    }
    estimate(bytes).saturating_add(media)
}

/// The completion a request asks for: the larger of `max_completion_tokens`
/// and `max_tokens`, else 1024, times its `n` choices (an integer of 1 or
/// more, else 1).
fn completion(req: &serde_json::Map<String, serde_json::Value>) -> i64 {
    let asked = ["max_completion_tokens", "max_tokens"]
        .iter()
        .filter_map(|k| req.get(*k).and_then(|v| v.as_u64()))
        .max()
        .map_or(DEFAULT_COMPLETION, |n| n.min(i64::MAX as u64) as i64);
    let n = req
        .get("n")
        .and_then(|v| v.as_u64())
        .filter(|n| *n >= 1)
        .map_or(1, |n| n.min(i64::MAX as u64) as i64);
    asked.saturating_mul(n)
}

/// A limited member's place among their requests in flight and the tokens
/// it holds, given back when dropped: after the request's usage row is
/// written, or on any path that writes none.
struct Reserved {
    st: St,
    member: String,
    held: i64,
}

impl Drop for Reserved {
    fn drop(&mut self) {
        let mut f = self.st.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((n, held)) = f.get_mut(&self.member) {
            *n -= 1;
            *held -= self.held;
            if *n == 0 {
                f.remove(&self.member);
            }
        }
    }
}

/// Checks the daily limit (UTC) and takes a place in flight under one lock:
/// a request is let in while today's tokens and those its member's other
/// requests in flight hold are under the limit, and holds its charge or
/// what is left of the limit, whichever is less. The day is spent only
/// when today's tokens reach the limit; while held charges alone fill it,
/// the answer is too_many_in_flight. A refusal takes no entry. `limit` is
/// the member's daily limit as it holds for them, if any.
fn reserve(
    st: &St,
    m: &Member,
    limit: Option<u64>,
    prompt: i64,
    completion: i64,
) -> Result<std::result::Result<Option<Reserved>, (StatusCode, &'static str)>> {
    let Some(limit) = limit else {
        return Ok(Ok(None));
    };
    let mut f = st.in_flight.lock().unwrap_or_else(|e| e.into_inner());
    let now = fonsviva_core::util::unix_now();
    let today = (now - now % DAY) as i64;
    let used = st.store().tokens_since(&m.id, today)?;
    if used >= limit as i64 {
        return Ok(Err((StatusCode::TOO_MANY_REQUESTS, "daily_limit")));
    }
    let (n, held) = f.get(&m.id).copied().unwrap_or_default();
    let left = limit as i64 - used - held;
    if left <= 0 || n >= IN_FLIGHT {
        return Ok(Err((StatusCode::TOO_MANY_REQUESTS, "too_many_in_flight")));
    }
    let take = prompt.saturating_add(completion).min(left);
    let (n, held) = f.entry(m.id.clone()).or_default();
    *n += 1;
    *held += take;
    Ok(Ok(Some(Reserved {
        st: st.clone(),
        member: m.id.clone(),
        held: take,
    })))
}

async fn settings(State(st): State<St>) -> Response {
    json_or_500(st.store().settings())
}

async fn update_settings(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    b: Bytes,
) -> Response {
    let bad = || err(StatusCode::BAD_REQUEST, "bad_request");
    let Some(p): Option<serde_json::Map<String, serde_json::Value>> = body(&b) else {
        return bad();
    };
    let flag = |name: &str| match p.get(name) {
        None => Ok(None),
        Some(v) => v.as_bool().map(Some).ok_or(()),
    };
    let daily = match p.get("dailyTokens") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(v) => match v.as_u64().filter(|n| (1..=1_000_000_000).contains(n)) {
            Some(n) => Some(Some(n)),
            None => return bad(),
        },
    };
    let kwh = match p.get("electricityPerKwh") {
        None => None,
        Some(v) => match v.as_str().and_then(|s| costs::micros(s, costs::KWH_MAX)) {
            Some(m) => Some(m),
            None => return bad(),
        },
    };
    let (Ok(students_allowed), Ok(keep_threads), Ok(admins_read_threads)) = (
        flag("studentsAllowed"),
        flag("keepThreads"),
        flag("adminsReadThreads"),
    ) else {
        return bad();
    };
    let timezone = match p.get("timezone") {
        None => None,
        Some(v) => match v.as_str().and_then(limits::zone_name) {
            Some(z) => Some(z),
            None => return bad(),
        },
    };
    let patch = SettingsPatch {
        students_allowed,
        daily_tokens: daily,
        keep_threads,
        admins_read_threads,
        electricity_per_kwh: kwh,
        timezone,
    };
    if patch == SettingsPatch::default() {
        return bad();
    }
    json_or_500(st.store().update_settings(&s.member.id, &patch))
}

async fn privacy(State(st): State<St>) -> Response {
    json_or_500(st.store().settings().map(
        |s| json!({"keepThreads": s.keep_threads, "adminsReadThreads": s.admins_read_threads}),
    ))
}

/// A number from `q` in `lo..=hi`, `default` when it is absent, `None`
/// when it is anything else.
fn bounded(q: &HashMap<String, String>, name: &str, lo: i64, hi: i64, default: i64) -> Option<i64> {
    match q.get(name) {
        None => Some(default),
        Some(v) => v.parse().ok().filter(|n| (lo..=hi).contains(n)),
    }
}

/// `before` and `limit` of a thread page, `None` when either is bad.
fn paging(q: &HashMap<String, String>) -> Option<(Option<i64>, i64)> {
    let before = match q.get("before") {
        None => None,
        Some(v) => Some(v.parse::<i64>().ok().filter(|n| *n > 0)?),
    };
    Some((before, bounded(q, "limit", 1, PAGE_LIMIT, PAGE_DEFAULT)?))
}

/// One page of a member's thread. With `keepThreads` off it is empty.
fn page(st: &OrgState, member: &str, (before, limit): (Option<i64>, i64)) -> Response {
    let store = st.store();
    json_or_500(store.settings().and_then(|s| {
        if !s.keep_threads {
            return Ok(json!({"messages": [], "stored": false, "more": false}));
        }
        let (messages, more) = store.thread(member, before, limit)?;
        Ok(json!({"messages": messages, "stored": true, "more": more}))
    }))
}

async fn thread(
    State(st): State<St>,
    Extension(c): Extension<Caller>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    match paging(&q) {
        Some(p) => page(&st, &c.member.id, p),
        None => err(StatusCode::BAD_REQUEST, "bad_request"),
    }
}

/// An admin reads another member's thread only while the org lets admins
/// read threads. Each read that shows a kept thread is audited, under the
/// same store guard and only once the page was read.
async fn member_thread(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(p) = paging(&q) else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    let store = st.store();
    let read = (|| {
        let set = store.settings()?;
        if !set.admins_read_threads {
            return Ok(err(StatusCode::FORBIDDEN, "threads_private"));
        }
        if !store.has_member(&id)? {
            return Ok(err(StatusCode::NOT_FOUND, "not_found"));
        }
        if !set.keep_threads {
            return Ok(
                Json(json!({"messages": [], "stored": false, "more": false})).into_response(),
            );
        }
        let (messages, more) = store.thread(&id, p.0, p.1)?;
        store.audit_thread_read(&s.member.id, &id)?;
        Ok(Json(json!({"messages": messages, "stored": true, "more": more})).into_response())
    })();
    read.unwrap_or_else(internal)
}

async fn search(
    State(st): State<St>,
    Extension(c): Extension<Caller>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let text = q
        .get("q")
        .filter(|t| (1..=200).contains(&t.chars().count()) && !t.trim().is_empty());
    let (Some(text), Some(limit)) = (text, bounded(&q, "limit", 1, 50, 20)) else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    let store = st.store();
    json_or_500(store.settings().and_then(|s| {
        if !s.keep_threads {
            return Ok(json!({"results": [], "stored": false}));
        }
        let results = store.search(&c.member.id, text, limit)?;
        Ok(json!({"results": results, "stored": true}))
    }))
}

async fn delete_message(
    State(st): State<St>,
    Extension(c): Extension<Caller>,
    UrlPath(seq): UrlPath<String>,
) -> Response {
    let Ok(seq) = seq.parse::<i64>() else {
        return err(StatusCode::NOT_FOUND, "not_found");
    };
    let deleted = st.store().delete_message(&c.member.id, seq);
    after_delete(&st, &c.member.id, deleted).await
}

async fn delete_thread(State(st): State<St>, Extension(c): Extension<Caller>) -> Response {
    let deleted = st.store().delete_thread(&c.member.id).map(|()| true);
    after_delete(&st, &c.member.id, deleted).await
}

/// 204 once the text is cleared, 404 when there was nothing to delete.
/// While any delete's cleanup is pending, a 404 runs it first, so the retry
/// of a delete that answered busy clears its text; that retry is still
/// busy while the log stays held, and no other member's 404 ever is.
async fn after_delete(st: &OrgState, member: &str, deleted: Result<bool>) -> Response {
    let busy = || {
        let mut r = err(StatusCode::SERVICE_UNAVAILABLE, "busy");
        r.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(1));
        r
    };
    match deleted {
        Err(e) => internal(e),
        Ok(true) => match cleared(st, MERGES).await {
            Err(e) => {
                if let Err(m) = st.store().mark_cleanup(member) {
                    eprintln!("fonsviva org: {m:#}");
                }
                internal(e)
            }
            Ok(true) => StatusCode::NO_CONTENT.into_response(),
            Ok(false) => match st.store().mark_cleanup(member) {
                Err(e) => internal(e),
                Ok(()) => busy(),
            },
        },
        Ok(false) => {
            let (any, mine) = match st.store().cleanup_pending(member) {
                Err(e) => return internal(e),
                Ok(p) => p,
            };
            if !any {
                return err(StatusCode::NOT_FOUND, "not_found");
            }
            match cleared(st, MERGES).await {
                Err(e) if mine => internal(e),
                Ok(false) if mine => busy(),
                Err(e) => {
                    eprintln!("fonsviva org: {e:#}");
                    err(StatusCode::NOT_FOUND, "not_found")
                }
                Ok(_) => err(StatusCode::NOT_FOUND, "not_found"),
            }
        }
    }
}

/// Checkpoint tries after the first, and the wait between them.
const CHECKPOINT_RETRIES: usize = 5;
const CHECKPOINT_WAIT: Duration = Duration::from_millis(50);

/// After a delete: drops deleted words from the index and empties the
/// write-ahead log, retrying while a reader holds it. The store is held
/// for one merge or one try at a time, and never while waiting. A try
/// merges again first, so a delete made while this one waited is merged
/// too; one that finds no merge work and empties the log clears every
/// pending cleanup. False when `merges` steps still left work or the log
/// stays held; the delete is done, and a retry clears it again.
async fn cleared(st: &OrgState, merges: usize) -> Result<bool> {
    let (mut steps, mut tried) = (0, 0);
    loop {
        {
            let store = st.store();
            if store.merge_step()? {
                steps += 1;
                if steps == merges {
                    return Ok(false);
                }
                continue;
            }
            if store.checkpoint()? {
                store.clear_cleanups()?;
                return Ok(true);
            }
        }
        if tried == CHECKPOINT_RETRIES {
            return Ok(false);
        }
        tried += 1;
        tokio::time::sleep(CHECKPOINT_WAIT).await;
    }
}

#[derive(serde::Deserialize)]
struct KeyReq {
    name: String,
}

async fn create_key(State(st): State<St>, Extension(c): Extension<Caller>, b: Bytes) -> Response {
    let Some(r): Option<KeyReq> = body(&b) else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    let name = r.name.trim();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    }
    match st.store().create_key(&c.member.id, name) {
        Ok(Some((id, key))) => Json(json!({"id": id, "name": name, "key": key})).into_response(),
        Ok(None) => err(StatusCode::CONFLICT, "too_many_keys"),
        Err(e) => internal(e),
    }
}

async fn keys(State(st): State<St>, Extension(c): Extension<Caller>) -> Response {
    json_or_500(st.store().keys(&c.member.id))
}

async fn revoke_key(
    State(st): State<St>,
    Extension(c): Extension<Caller>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    match st.store().revoke_key(&c.member.id, &id) {
        Ok(true) => Json(json!({})).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e),
    }
}

/// The first second of the window `?days=N` asks for: today and the N - 1
/// UTC days before it, 7 by default, N at most `max`.
fn since(q: &HashMap<String, String>, max: u64) -> Option<i64> {
    let days = match q.get("days") {
        None => 7,
        Some(d) => d.parse::<u64>().ok().filter(|d| (1..=max).contains(d))?,
    };
    let now = fonsviva_core::util::unix_now();
    Some((now - now % DAY - (days - 1) * DAY) as i64)
}

async fn all_usage(State(st): State<St>, Query(q): Query<HashMap<String, String>>) -> Response {
    match since(&q, 90) {
        Some(t) => json_or_500(st.store().usage(t, None)),
        None => err(StatusCode::BAD_REQUEST, "bad_request"),
    }
}

async fn my_usage(
    State(st): State<St>,
    Extension(c): Extension<Caller>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    match since(&q, 90) {
        Some(t) => json_or_500(st.store().usage(t, Some(&c.member.id))),
        None => err(StatusCode::BAD_REQUEST, "bad_request"),
    }
}

async fn totals(State(st): State<St>, Query(q): Query<HashMap<String, String>>) -> Response {
    let by = match q.get("by").map(String::as_str) {
        Some("member") => By::Member,
        Some("model") => By::Model,
        _ => return err(StatusCode::BAD_REQUEST, "bad_request"),
    };
    let Some(t) = since(&q, 366) else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    json_or_500(
        st.store()
            .totals(t, by)
            .map(|(rows, total)| json!({ "rows": rows, "total": total })),
    )
}

/// Serves until Ctrl-C or SIGTERM.
pub async fn run(listener: tokio::net::TcpListener, st: St) -> Result<()> {
    if st.store().cleanup_pending("")?.0 {
        let st = st.clone();
        tokio::spawn(async move {
            if let Err(e) = cleared(&st, MERGES).await {
                eprintln!("fonsviva org: {e:#}");
            }
        });
    }
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
    fn a_request_is_charged_its_text_and_the_completion_it_asks_for() {
        let req = |v: serde_json::Value| v.as_object().unwrap().clone();
        assert_eq!(completion(&req(json!({}))), 1024);
        assert_eq!(completion(&req(json!({"max_tokens": 400}))), 400);
        for (a, b) in [(400, 300), (300, 400)] {
            let both = json!({"max_tokens": a, "max_completion_tokens": b});
            assert_eq!(completion(&req(both)), 400, "the larger of {a} and {b}");
        }
        assert_eq!(completion(&req(json!({"max_tokens": "400"}))), 1024);
        assert_eq!(completion(&req(json!({"max_tokens": u64::MAX}))), i64::MAX);
        assert_eq!(completion(&req(json!({"max_tokens": 400, "n": 3}))), 1200);
        assert_eq!(completion(&req(json!({"n": 2}))), 2048);
        assert_eq!(completion(&req(json!({"max_tokens": 400, "n": 0}))), 400);
        assert_eq!(completion(&req(json!({"max_tokens": 400, "n": "3"}))), 400);
        let huge = json!({"max_tokens": 2, "n": u64::MAX});
        assert_eq!(completion(&req(huge)), i64::MAX);
        let media = |t: &str| json!({"type": t, t: {"data": "A".repeat(1 << 20)}});
        let text = json!({"type": "text", "text": "x".repeat(40)});
        let tools = json!([{"type": "function", "function": {"name": "x".repeat(4000)}}]);
        let asks = req(json!({
            "tools": tools.clone(),
            "tool_choice": "auto",
            "messages": [
                {"role": "system", "content": "x".repeat(40)},
                {"role": "user", "content": [
                    text.clone(), media("image_url"), media("input_audio"), media("file"),
                ]},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"type": "function", "function": {"name": "f", "arguments": "x".repeat(40)}},
                ]},
            ],
        }));
        let bytes = 80 + text.to_string().len() + tools.to_string().len() + 6;
        let part = |t: &str| media(t).to_string().len() as i64 / 16;
        let parts = 85 + part("input_audio") + part("file");
        assert_eq!(prompt(&asks), bytes.div_ceil(4) as i64 + parts);
    }

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

    fn limited(tokens: u64) -> (tempfile::TempDir, St, Member) {
        let d = tempfile::tempdir().unwrap();
        let mut store = Store::create(&d.path().join("org.db"), "https://org.example").unwrap();
        store
            .update_settings(
                "adm",
                &SettingsPatch {
                    daily_tokens: Some(Some(tokens)),
                    ..Default::default()
                },
            )
            .unwrap();
        let url = "https://org.example".parse().unwrap();
        let st = OrgState::new(store, &url, "127.0.0.1:0".parse().unwrap(), false);
        let m = Member {
            id: "mem_a".into(),
            name: "A".into(),
            role: Role::User,
        };
        (d, Arc::new(st), m)
    }

    #[tokio::test]
    async fn a_cleanup_that_runs_out_of_merges_is_busy() {
        let d = tempfile::tempdir().unwrap();
        let mut store = Store::create(&d.path().join("org.db"), "https://org.example").unwrap();
        let link = store.new_admin_link("cli").unwrap();
        let (m, _) = store.join(&link, "Ada").unwrap().unwrap();
        for i in 0..20 {
            store
                .append_message(&m.id, "user", &format!("word{i}"), "stub/m", None)
                .unwrap()
                .unwrap();
        }
        store.delete_thread(&m.id).unwrap();
        let url = "https://org.example".parse().unwrap();
        let st = OrgState::new(store, &url, "127.0.0.1:0".parse().unwrap(), false);
        assert!(!cleared(&st, 1).await.unwrap());
        assert!(cleared(&st, MERGES).await.unwrap());
    }

    #[tokio::test]
    async fn a_cleanup_that_ends_while_another_waits_merges_its_work_first() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("org.db");
        let mut store = Store::create(&db, "https://org.example").unwrap();
        let link = store.new_admin_link("cli").unwrap();
        let (a, _) = store.join(&link, "Ada").unwrap().unwrap();
        let link = store.new_admin_link("cli").unwrap();
        let (b, _) = store.join(&link, "Bo").unwrap().unwrap();
        let bs = store
            .append_message(&b.id, "user", "plain words", "stub/m", None)
            .unwrap()
            .unwrap();
        for i in 0..100 {
            let words: Vec<String> = (0..200).map(|j| format!("w{i}x{j}")).collect();
            let kept = format!("b{i} {}", words.join(" "));
            store
                .append_message(&b.id, "user", &kept, "stub/m", None)
                .unwrap()
                .unwrap();
            let text = format!("zzmarker{i} {}", words.join(" "));
            store
                .append_message(&a.id, "user", &text, "stub/m", None)
                .unwrap()
                .unwrap();
        }
        store.delete_message(&b.id, bs).unwrap();
        let url = "https://org.example".parse().unwrap();
        let st = OrgState::new(store, &url, "127.0.0.1:0".parse().unwrap(), false);
        let reader = rusqlite::Connection::open(&db).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let _: i64 = reader
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        let marked = |d: &std::path::Path| {
            ["org.db", "org.db-wal"].iter().any(|f| {
                let bytes = std::fs::read(d.join(f)).unwrap_or_default();
                bytes.windows(8).any(|w| w == b"zzmarker")
            })
        };
        let busy_a = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            st.store().delete_thread(&a.id).unwrap();
            assert!(!cleared(&st, 1).await.unwrap());
            st.store().mark_cleanup(&a.id).unwrap();
            assert_eq!(st.store().cleanup_pending(&a.id).unwrap(), (true, true));
            drop(reader);
        };
        let (done_b, ()) = tokio::join!(cleared(&st, MERGES), busy_a);
        assert!(done_b.unwrap());
        assert!(
            !st.store().merge_step().unwrap(),
            "a cleanup ended with merge work left"
        );
        let r = after_delete(&st, &a.id, Ok(false)).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert!(
            !marked(d.path()),
            "a cleanup ended with words still in the index"
        );
    }

    #[test]
    fn a_refused_request_leaves_its_member_in_flight_as_it_was() {
        let (_d, st, m) = limited(1000);
        let held = |st: &St| st.in_flight.lock().unwrap().get("mem_a").copied();
        let four: Vec<_> = (0..4)
            .map(|_| reserve(&st, &m, Some(1000), 10, 0).unwrap().unwrap())
            .collect();
        assert_eq!(held(&st), Some((4, 40)));
        let r = reserve(&st, &m, Some(1000), 10, 0).unwrap();
        assert!(matches!(r, Err((_, "too_many_in_flight"))));
        assert_eq!(held(&st), Some((4, 40)));
        drop(four);
        let all = reserve(&st, &m, Some(1000), 1000, 0).unwrap().unwrap();
        let r = reserve(&st, &m, Some(1000), 10, 0).unwrap();
        assert!(matches!(r, Err((_, "too_many_in_flight"))));
        assert_eq!(held(&st), Some((1, 1000)));
        drop(all);
        assert_eq!(held(&st), None);
    }

    #[test]
    fn case_20_the_thread_routes_are_in_the_table() {
        let who = |m: &str, p: &str| match permission(&m.parse().unwrap(), p) {
            Some(Allow::Roles(r)) => r.to_vec(),
            other => panic!("{m} {p}: {other:?}"),
        };
        let members = [Role::Admin, Role::Full, Role::User, Role::Student];
        for (m, p) in [
            ("GET", "/api/org/v1/thread"),
            ("DELETE", "/api/org/v1/thread"),
            ("POST", "/api/org/v1/thread/messages"),
            ("DELETE", "/api/org/v1/thread/messages/{seq}"),
            ("GET", "/api/org/v1/thread/search"),
            ("GET", "/api/org/v1/privacy"),
        ] {
            assert_eq!(who(m, p), members, "{m} {p}");
        }
        assert_eq!(who("GET", "/api/org/v1/members/{id}/thread"), [Role::Admin]);
        assert_eq!(gaps(PERMISSIONS), Vec::<String>::new());
    }

    #[test]
    fn case_16_the_cost_routes_are_in_the_table() {
        for (m, p) in [
            ("PATCH", "/api/org/v1/sources/{id}"),
            ("GET", "/api/org/v1/usage/totals"),
        ] {
            match permission(&m.parse().unwrap(), p) {
                Some(Allow::Roles(r)) => assert_eq!(r, [Role::Admin], "{m} {p}"),
                other => panic!("{m} {p}: {other:?}"),
            }
        }
        assert_eq!(gaps(PERMISSIONS), Vec::<String>::new());
    }

    #[test]
    fn case_18_the_limit_routes_are_in_the_table() {
        let who = |m: &str, p: &str| match permission(&m.parse().unwrap(), p) {
            Some(Allow::Roles(r)) => r.to_vec(),
            other => panic!("{m} {p}: {other:?}"),
        };
        for m in ["GET", "PUT"] {
            assert_eq!(who(m, "/api/org/v1/members/{id}/limits"), [Role::Admin]);
        }
        assert_eq!(
            who("GET", "/api/org/v1/me/limits"),
            [Role::Admin, Role::Full, Role::User, Role::Student]
        );
        assert_eq!(gaps(PERMISSIONS), Vec::<String>::new());
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
            pricing: None,
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
