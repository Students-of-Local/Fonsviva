//! `fonsviva serve`: the daemon and its HTTP API (docs/API.md).

use anyhow::{Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path as UrlPath, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, broadcast, mpsc, watch};
use tokio_stream::wrappers::{BroadcastStream, ReceiverStream};

use crate::auth::{self, Action, KeyRecord, Keys, Scope, Tokens};
use crate::discovery::{self, API, DaemonInfo, Layout, SCHEMA};
use fonsviva_core::catalog::{self, Verdict};
use fonsviva_core::download::Cancel;
use fonsviva_core::engine::{self, Engine, Handle, LlamaCpp, LoadSpec, Loaded, Ollama};
use fonsviva_core::hardware::{self, Hardware};
use fonsviva_core::stats::{Rolling, Sample, Timings};
use fonsviva_core::store::{self, Installed, Locations};
use fonsviva_core::util::{rfc3339, unix_now};
use fonsviva_core::{paths, stats};

pub struct Running {
    pub model: Installed,
    pub loaded: Loaded,
    pub handle: Handle,
    pub started_at: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Job {
    pub id: String,
    /// pull, engine or bench
    pub kind: String,
    pub target: String,
    /// running, done, failed, cancelled
    pub state: String,
    pub done: u64,
    pub total: u64,
    pub fetched: u64,
    pub error: Option<String>,
    #[serde(skip)]
    pub cancel: Cancel,
    #[serde(skip)]
    pub discard: Option<std::path::PathBuf>,
}

pub struct AppState {
    pub port: u16,
    pub layout: Layout,
    pub info: DaemonInfo,
    owner_token: String,
    control_token: String,
    keys: Mutex<Keys>,
    tokens: Mutex<Tokens>,
    hw: Mutex<Option<(Instant, Hardware)>>,
    pub running: Mutex<HashMap<String, Running>>,
    stats: Mutex<HashMap<String, Rolling>>,
    jobs: Mutex<HashMap<String, Job>>,
    events: broadcast::Sender<(String, Value)>,
    load_lock: Mutex<()>,
    /// Turns true when shutdown starts; open event streams end on it.
    stopping: watch::Sender<bool>,
    pub locations: Locations,
}

type S = State<Arc<AppState>>;

/// An API error: `{"error":{"code","message","fix"?}}`.
pub struct ApiError(StatusCode, String, String, Option<String>);

impl ApiError {
    pub fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        ApiError(status, code.into(), message.into(), None)
    }
    pub fn fix(mut self, f: impl Into<String>) -> Self {
        self.3 = Some(f.into());
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut e = json!({"code": self.1, "message": self.2});
        if let Some(f) = self.3 {
            e["fix"] = json!(f);
        }
        (self.0, Json(json!({"error": e}))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("{e:#}"),
        )
    }
}

type ApiResult<T = Json<Value>> = std::result::Result<T, ApiError>;

fn bad(code: &str, msg: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, code, msg)
}

impl AppState {
    fn emit(&self, kind: &str, data: Value) {
        let _ = self.events.send((kind.to_string(), data));
    }

    async fn hardware(&self) -> Hardware {
        let mut g = self.hw.lock().await;
        if let Some((at, hw)) = g.as_ref()
            && at.elapsed() < Duration::from_secs(10)
        {
            return hw.clone();
        }
        let hw = tokio::task::spawn_blocking(hardware::detect)
            .await
            .expect("detect");
        *g = Some((Instant::now(), hw.clone()));
        hw
    }

    async fn models(&self) -> Vec<Installed> {
        let loc = self.locations.clone();
        tokio::task::spawn_blocking(move || store::scan(&loc))
            .await
            .unwrap_or_default()
    }

    fn scope(&self, token: &str, tokens: &Tokens) -> Option<Scope> {
        if auth::ct_eq(token, &self.owner_token) {
            Some(Scope::Owner)
        } else if auth::ct_eq(token, &self.control_token) {
            Some(Scope::Control)
        } else {
            tokens.check(token)
        }
    }
}

/// Starts the daemon and serves until stopped.
pub async fn serve() -> Result<()> {
    let layout = Layout::current();
    std::fs::create_dir_all(&layout.root)
        .with_context(|| format!("create data dir {}", layout.root.display()))?;
    let _lock = discovery::lock(&layout)?;
    discovery::check_schema(&layout)?;
    let data_dir_id = discovery::data_dir_id(&layout)?;
    let owner_token = auth::read_or_create(&layout.owner_token(), || auth::random_hex(32))?;
    let control_token = auth::read_or_create(&layout.control_token(), || auth::random_hex(32))?;
    let mut keys = Keys::load(layout.keys())?;
    // First run: a /v1 key for the desktop app and one for the CLI.
    for (app, path) in [("desktop", layout.desktop_key()), ("cli", layout.cli_key())] {
        let have = std::fs::read_to_string(&path)
            .ok()
            .filter(|s| keys.check(s.trim()).is_some());
        if have.is_none() {
            let (_, secret) = keys.create(app)?;
            auth::write_private(&path, secret.as_bytes())?;
        }
    }
    let tokens = Tokens::load(layout.tokens())?;

    let want = paths::port();
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", want)).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            let who = discovery::port_holder(want).unwrap_or_else(|| "another program".into());
            anyhow::bail!(
                "Port {want} is taken by {who}, so Fonsviva can't start. Stop that program, or set FONSVIVA_PORT to use another port (apps you wired will need it too)."
            );
        }
        Err(e) => return Err(e).context("bind 127.0.0.1"),
    };
    let port = listener.local_addr()?.port();
    let secret = auth::random_hex(32);
    let info = DaemonInfo {
        pid: std::process::id(),
        port,
        version: env!("CARGO_PKG_VERSION").into(),
        channel: paths::CHANNEL.into(),
        schema: SCHEMA,
        data_dir_id,
        secret,
        started_at: rfc3339(unix_now()),
    };
    discovery::write_info(&layout, &info)?;

    let (events, _) = broadcast::channel(256);
    let state = Arc::new(AppState {
        port,
        info,
        owner_token,
        control_token,
        keys: Mutex::new(keys),
        tokens: Mutex::new(tokens),
        hw: Mutex::new(None),
        running: Mutex::new(HashMap::new()),
        stats: Mutex::new(HashMap::new()),
        jobs: Mutex::new(HashMap::new()),
        events,
        load_lock: Mutex::new(()),
        stopping: watch::Sender::new(false),
        locations: Locations::detect(),
        layout,
    });
    eprintln!(
        "Fonsviva {} ({}) serving on http://127.0.0.1:{port} for {}",
        env!("CARGO_PKG_VERSION"),
        paths::CHANNEL,
        state.layout.root.display()
    );
    let app = router(state.clone());
    let st = state.clone();
    let mut stop = state.stopping.subscribe();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate() => {}
            _ = stop.wait_for(|s| *s) => {}
        }
        st.stopping.send_replace(true);
    });
    // A client that keeps a request open must not hold the daemon up, so the
    // drain gets a few seconds and the cleanup below runs either way.
    let mut stop = state.stopping.subscribe();
    let served = tokio::select! {
        r = std::future::IntoFuture::into_future(server) => r.map_err(anyhow::Error::from),
        _ = async {
            let _ = stop.wait_for(|s| *s).await;
            tokio::time::sleep(DRAIN).await;
        } => Ok(()),
    };
    // Stop every engine this daemon started.
    let all: Vec<Running> = state.running.lock().await.drain().map(|(_, r)| r).collect();
    for r in all {
        let _ = r.handle.unload().await;
    }
    let _ = std::fs::remove_file(state.layout.daemon_json());
    served
}

/// How long open requests get to finish once shutdown starts.
const DRAIN: Duration = Duration::from_secs(5);

#[cfg(unix)]
async fn terminate() {
    if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        s.recv().await;
    }
}
#[cfg(not(unix))]
async fn terminate() {
    std::future::pending::<()>().await
}

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/hardware", get(hardware_route))
        .route("/catalog", get(catalog_route))
        .route("/models", get(models_route))
        .route("/models/pull", post(pull_route))
        .route("/models/{*id}", delete(remove_route))
        .route("/jobs", get(jobs_route))
        .route("/jobs/{id}/cancel", post(cancel_route))
        .route("/keys", get(keys_list).post(keys_create))
        .route("/keys/{id}", delete(keys_revoke))
        .route("/plugin-keys", post(plugin_keys_create))
        .route("/plugin-keys/{id}", delete(plugin_keys_revoke))
        .route("/tokens", get(tokens_list).post(tokens_create))
        .route("/tokens/{id}", delete(tokens_revoke))
        .route("/engines", get(engines_route))
        .route("/engines/llama.cpp/install", post(engine_install_route))
        .route("/run", post(run_route))
        .route("/running", get(running_route))
        .route("/stop", post(stop_route))
        .route("/stats", get(stats_route))
        .route("/bench", post(bench_route))
        .route("/events", get(events_route))
        .route("/shutdown", post(shutdown_route))
        .route("/wire", any(not_built))
        .route("/wire/{*rest}", any(not_built))
        .route("/cloud", any(not_built))
        .route("/cloud/{*rest}", any(not_built))
        .layer(middleware::from_fn_with_state(state.clone(), api_auth));
    let openai = Router::new()
        .route("/models", get(v1_models))
        .route("/chat/completions", post(v1_proxy))
        .route("/completions", post(v1_proxy))
        .route("/embeddings", post(v1_proxy))
        .layer(middleware::from_fn_with_state(state.clone(), v1_auth))
        .layer(middleware::from_fn(v1_cors));
    Router::new()
        .route("/health", get(health))
        .nest("/api/v1", api)
        .nest("/v1", openai)
        .layer(middleware::from_fn_with_state(state.clone(), host_guard))
        .with_state(state)
}

// ------------------------------------------------------------ middleware

fn header_str(h: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

async fn host_guard(State(st): S, req: Request, next: Next) -> Response {
    if !auth::host_ok(header_str(req.headers(), header::HOST), st.port) {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "host",
            "Requests must address the daemon as 127.0.0.1 or localhost.",
        )
        .into_response();
    }
    next.run(req).await
}

/// The scope a route needs.
fn action_for(method: &Method, path: &str) -> Action {
    let p = path.trim_start_matches("/api/v1");
    match (method.as_str(), p) {
        (_, p) if p.starts_with("/keys") => Action::Keys,
        (_, p) if p.starts_with("/plugin-keys") => Action::PluginKeys,
        ("POST", "/tokens") | ("DELETE", _) if p.starts_with("/tokens") => Action::MintWatch,
        ("POST", "/models/pull") | ("POST", "/engines/llama.cpp/install") => Action::Pull,
        ("DELETE", p) if p.starts_with("/models/") => Action::Pull,
        ("POST", "/run") => Action::Run,
        ("POST", "/stop") => Action::Stop,
        ("POST", "/bench") => Action::Bench,
        ("POST", "/shutdown") => Action::Shutdown,
        ("POST", p) if p.starts_with("/jobs/") => Action::CancelJob,
        (m, p) if p.starts_with("/wire") && m != "GET" => Action::Wire,
        ("POST", p) if p.starts_with("/cloud/runpod/pods/") && p.ends_with("/stop") => {
            Action::PodStop
        }
        (m, p) if p.starts_with("/cloud") && m != "GET" => Action::CloudSpend,
        _ => Action::Read,
    }
}

async fn api_auth(State(st): S, req: Request, next: Next) -> Response {
    if !auth::origin_ok_for_api(header_str(req.headers(), header::ORIGIN), paths::is_dev()) {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "origin",
            "Only the Fonsviva app may call this API from a browser.",
        )
        .into_response();
    }
    let Some(token) = auth::bearer(header_str(req.headers(), header::AUTHORIZATION)) else {
        return ApiError::new(StatusCode::UNAUTHORIZED, "auth", "This API needs a token.")
            .fix("Send Authorization: Bearer <token from owner.token or control.token>.")
            .into_response();
    };
    let scope = {
        let tokens = st.tokens.lock().await;
        st.scope(token, &tokens)
    };
    let Some(scope) = scope else {
        return ApiError::new(
            StatusCode::UNAUTHORIZED,
            "auth",
            "That token is not valid for this daemon.",
        )
        .into_response();
    };
    let action = action_for(req.method(), req.uri().path());
    if !auth::allowed(scope, action) {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "scope",
            format!("A {scope:?} token can't do this."),
        )
        .into_response();
    }
    let mut req = req;
    req.extensions_mut().insert(scope);
    next.run(req).await
}

/// Browser OpenAI clients call /v1 from any page (docs/API.md), so /v1
/// answers CORS preflight without a key and marks its answers readable from
/// any Origin. The app key stays the guard. /api/v1 never gets this.
async fn v1_cors(req: Request, next: Next) -> Response {
    let allow_origin = (
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        header::HeaderValue::from_static("*"),
    );
    if req.method() == Method::OPTIONS {
        let mut headers = String::from("authorization, content-type");
        if let Some(asked) = header_str(req.headers(), header::ACCESS_CONTROL_REQUEST_HEADERS) {
            headers = format!("{headers}, {asked}");
        }
        let headers = header::HeaderValue::from_str(&headers)
            .unwrap_or(header::HeaderValue::from_static("authorization, content-type"));
        return (
            StatusCode::NO_CONTENT,
            [
                allow_origin,
                (
                    header::ACCESS_CONTROL_ALLOW_METHODS,
                    header::HeaderValue::from_static("GET, POST, OPTIONS"),
                ),
                (header::ACCESS_CONTROL_ALLOW_HEADERS, headers),
                (
                    header::ACCESS_CONTROL_MAX_AGE,
                    header::HeaderValue::from_static("600"),
                ),
            ],
        )
            .into_response();
    }
    let mut r = next.run(req).await;
    r.headers_mut().insert(allow_origin.0, allow_origin.1);
    r
}

async fn v1_auth(State(st): S, req: Request, next: Next) -> Response {
    let Some(key) = auth::bearer(header_str(req.headers(), header::AUTHORIZATION)) else {
        return ApiError::new(
            StatusCode::UNAUTHORIZED,
            "auth",
            "This endpoint needs an app key.",
        )
        .fix("Use the key Fonsviva made for your app (desktop.key, or Connect in the app).")
        .into_response();
    };
    let rec = st.keys.lock().await.check(key).cloned();
    let Some(rec) = rec else {
        return ApiError::new(
            StatusCode::UNAUTHORIZED,
            "auth",
            "That app key is not valid or was revoked.",
        )
        .into_response();
    };
    let mut req = req;
    req.extensions_mut().insert(rec);
    next.run(req).await
}

// ---------------------------------------------------------------- routes

#[derive(Deserialize)]
struct HealthQ {
    challenge: Option<String>,
}

async fn health(State(st): S, Query(q): Query<HealthQ>) -> Json<Value> {
    let mut v = json!({
        "status": "ok",
        "version": st.info.version,
        "channel": st.info.channel,
        "api": API,
        "schema": SCHEMA,
        "data_dir_id": st.info.data_dir_id,
    });
    if let Some(n) = q.challenge.filter(|n| !n.is_empty() && n.len() <= 256) {
        v["proof"] = json!(auth::hmac_sha256_hex(
            st.info.secret.as_bytes(),
            n.as_bytes()
        ));
    }
    Json(v)
}

async fn not_built() -> ApiError {
    ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        "not_built",
        "This part of Fonsviva is not built yet.",
    )
}

async fn hardware_route(State(st): S) -> ApiResult {
    Ok(Json(
        serde_json::to_value(st.hardware().await).map_err(anyhow::Error::from)?,
    ))
}

fn bench_for(id: &str) -> Option<Value> {
    let p = paths::bench_dir().join(format!("{}.json", fonsviva_core::util::slug(id)));
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

async fn catalog_route(State(st): S) -> ApiResult {
    let hw = st.hardware().await;
    let mut recs = catalog::recommend(&catalog::catalog(), &hw);
    for r in &mut recs {
        r.measured = bench_for(&format!("fonsviva:{}", r.entry.id)).map(|b| b["summary"].clone());
    }
    Ok(Json(
        json!({"hardware_backend": hw.backend, "models": recs}),
    ))
}

async fn models_route(State(st): S) -> ApiResult {
    Ok(Json(json!({"models": st.models().await})))
}

#[derive(Deserialize)]
struct PullReq {
    id: String,
}

fn new_job(kind: &str, target: &str) -> Job {
    Job {
        id: format!("job_{}", auth::random_hex(6)),
        kind: kind.into(),
        target: target.into(),
        state: "running".into(),
        done: 0,
        total: 0,
        fetched: 0,
        error: None,
        cancel: Cancel::default(),
        discard: None,
    }
}

impl AppState {
    async fn job_update(&self, id: &str, f: impl FnOnce(&mut Job)) {
        let snapshot = {
            let mut jobs = self.jobs.lock().await;
            let Some(j) = jobs.get_mut(id) else { return };
            f(j);
            serde_json::to_value(&*j).unwrap_or_default()
        };
        self.emit("job", snapshot);
    }

    /// Starts a job and returns its id; `work` gets the cancel flag and a progress sink.
    async fn spawn_job<F, Fut>(self: &Arc<Self>, mut job: Job, work: F) -> String
    where
        F: FnOnce(Arc<Self>, String, Cancel) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let id = job.id.clone();
        let cancel = job.cancel.clone();
        job.state = "running".into();
        self.jobs.lock().await.insert(id.clone(), job);
        self.job_update(&id, |_| {}).await;
        let st = self.clone();
        let jid = id.clone();
        tokio::spawn(async move {
            let r = work(st.clone(), jid.clone(), cancel.clone()).await;
            let discard = st
                .jobs
                .lock()
                .await
                .get(&jid)
                .and_then(|j| j.discard.clone());
            if cancel.is_cancelled()
                && let Some(p) = discard
            {
                let _ = tokio::fs::remove_file(fonsviva_core::download::part_path(&p)).await;
            }
            st.job_update(&jid, |j| match r {
                Ok(()) => j.state = "done".into(),
                Err(_) if cancel.is_cancelled() => j.state = "cancelled".into(),
                Err(e) => {
                    j.state = "failed".into();
                    j.error = Some(format!("{e:#}"));
                }
            })
            .await;
        });
        id
    }

    async fn progress(self: &Arc<Self>, id: &str, p: &fonsviva_core::download::Progress) {
        let (d, t, f) = (p.done, p.total, p.fetched);
        self.job_update(id, |j| {
            j.done = d;
            j.total = t;
            j.fetched = f;
        })
        .await;
    }
}

/// Bridges a sync progress callback to async job updates.
fn progress_sink(st: Arc<AppState>, id: String) -> impl FnMut(&fonsviva_core::download::Progress) {
    let (tx, mut rx) = mpsc::unbounded_channel::<fonsviva_core::download::Progress>();
    tokio::spawn(async move {
        while let Some(p) = rx.recv().await {
            st.progress(&id, &p).await;
        }
    });
    move |p| {
        let _ = tx.send(p.clone());
    }
}

async fn pull_route(State(st): S, Json(r): Json<PullReq>) -> ApiResult {
    let id = r.id.trim_start_matches("fonsviva:").to_string();
    let Some(entry) = catalog::catalog().models.into_iter().find(|m| m.id == id) else {
        return Err(
            bad("unknown_model", format!("{} is not in the catalog.", r.id))
                .fix("See GET /api/v1/catalog for the models Fonsviva can download."),
        );
    };
    // Same download already running: return it.
    if let Some(j) = st
        .jobs
        .lock()
        .await
        .values()
        .find(|j| j.kind == "pull" && j.target == id && j.state == "running")
    {
        return Ok(Json(json!({"job": j.id})));
    }
    let mut job = new_job("pull", &id);
    job.total = entry.bytes;
    job.discard = Some(paths::models_dir().join(&entry.id).join(&entry.file));
    let job = st
        .spawn_job(job, move |st, jid, cancel| async move {
            let sink = progress_sink(st.clone(), jid.clone());
            let (_, fetched) = store::pull(&entry, &paths::models_dir(), &cancel, sink).await?;
            st.job_update(&jid, |j| {
                j.done = entry.bytes;
                j.fetched = fetched;
            })
            .await;
            Ok(())
        })
        .await;
    Ok(Json(json!({"job": job})))
}

#[derive(Deserialize, Default)]
struct ConfirmReq {
    #[serde(default)]
    confirm: bool,
}

async fn remove_route(
    State(st): S,
    UrlPath(id): UrlPath<String>,
    body: Option<Json<ConfirmReq>>,
) -> ApiResult {
    let confirm = body.map(|b| b.confirm).unwrap_or(false);
    let models = st.models().await;
    let Some(m) = store::resolve(&models, &id).cloned() else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("No installed model {id}."),
        ));
    };
    if st.running.lock().await.contains_key(&m.id) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "running",
            format!("{} is loaded.", m.id),
        )
        .fix("Stop it first."));
    }
    match m.source.as_str() {
        "fonsviva" => {
            store::remove_fonsviva(&paths::models_dir(), m.catalog_id.as_deref().unwrap_or(""))?
        }
        "ollama" => {
            if !confirm {
                return Err(bad(
                    "confirm",
                    format!(
                        "{} belongs to Ollama. Removing it deletes it from Ollama too.",
                        m.name
                    ),
                )
                .fix("Send {\"confirm\":true} to remove it through Ollama."));
            }
            engine::ollama_delete(
                &engine::ollama_base(),
                m.ollama_name.as_deref().unwrap_or(&m.name),
            )
            .await?;
        }
        other => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "not_owned",
                format!(
                    "{} belongs to {other}; Fonsviva doesn't delete other tools' files.",
                    m.name
                ),
            )
            .fix(if other == "lmstudio" {
                "Remove it in LM Studio."
            } else {
                "Remove it with `hf cache delete`."
            }));
        }
    }
    st.emit(
        "job",
        json!({"kind": "remove", "target": m.id, "state": "done"}),
    );
    Ok(Json(json!({"removed": m.id})))
}

async fn jobs_route(State(st): S) -> ApiResult {
    let jobs: Vec<Job> = st.jobs.lock().await.values().cloned().collect();
    Ok(Json(json!({"jobs": jobs})))
}

#[derive(Deserialize, Default)]
struct CancelReq {
    #[serde(default)]
    discard: bool,
}

async fn cancel_route(
    State(st): S,
    UrlPath(id): UrlPath<String>,
    body: Option<Json<CancelReq>>,
) -> ApiResult {
    let discard = body.map(|b| b.discard).unwrap_or(false);
    let mut jobs = st.jobs.lock().await;
    let Some(j) = jobs.get_mut(&id) else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("No job {id}."),
        ));
    };
    if j.state != "running" {
        return Ok(Json(json!({"job": id, "state": j.state})));
    }
    if !discard {
        j.discard = None;
    }
    j.cancel.cancel();
    Ok(Json(
        json!({"job": id, "state": "cancelling", "partial_kept": !discard}),
    ))
}

/// The profile always derives from the app, so the body is `{"app"}` only.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyReq {
    app: String,
}

async fn keys_list(State(st): S) -> ApiResult {
    let keys = st.keys.lock().await;
    let list: Vec<Value> = keys
        .list
        .iter()
        .map(|k| json!({"id": k.id, "app": k.app, "profile": k.profile, "prefix": k.prefix, "created_at": k.created_at}))
        .collect();
    Ok(Json(json!({"keys": list})))
}

async fn keys_create(State(st): S, Json(r): Json<KeyReq>) -> ApiResult {
    let (rec, secret) = st
        .keys
        .lock()
        .await
        .create(&r.app)
        .map_err(|e| bad("app", e.to_string()))?;
    Ok(Json(
        json!({"id": rec.id, "app": rec.app, "profile": rec.profile, "key": secret,
        "note": "This key is shown once. Fonsviva keeps only its hash."}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginKeyReq {
    plugin: String,
}

async fn plugin_keys_create(State(st): S, Json(r): Json<PluginKeyReq>) -> ApiResult {
    let (rec, secret) = st
        .keys
        .lock()
        .await
        .create_plugin(&r.plugin)
        .map_err(|e| bad("plugin", e.to_string()))?;
    Ok(Json(
        json!({"id": rec.id, "secret": secret, "app": rec.app, "profile": rec.profile}),
    ))
}

async fn plugin_keys_revoke(State(st): S, UrlPath(id): UrlPath<String>) -> ApiResult {
    let mut keys = st.keys.lock().await;
    // This route revokes plugin keys only; other keys need the owner's /keys.
    if !keys.list.iter().any(|k| k.id == id && k.is_plugin()) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("No plugin key {id}."),
        ));
    }
    keys.revoke(&id)?;
    Ok(Json(json!({"revoked": id})))
}

async fn keys_revoke(State(st): S, UrlPath(id): UrlPath<String>) -> ApiResult {
    if st.keys.lock().await.revoke(&id)? {
        Ok(Json(json!({"revoked": id})))
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("No key {id}."),
        ))
    }
}

#[derive(Deserialize)]
struct TokenReq {
    scope: String,
    #[serde(default)]
    label: String,
}

async fn tokens_list(State(st): S) -> ApiResult {
    let t = st.tokens.lock().await;
    let list: Vec<Value> = t
        .list
        .iter()
        .map(
            |t| json!({"id": t.id, "scope": t.scope, "label": t.label, "created_at": t.created_at}),
        )
        .collect();
    Ok(Json(json!({"tokens": list})))
}

async fn tokens_create(State(st): S, Json(r): Json<TokenReq>) -> ApiResult {
    if r.scope != "watch" {
        return Err(bad("scope", "Only watch tokens can be minted."));
    }
    let (rec, secret) = st.tokens.lock().await.mint_watch(&r.label)?;
    Ok(Json(
        json!({"id": rec.id, "scope": "watch", "label": rec.label, "token": secret}),
    ))
}

async fn tokens_revoke(State(st): S, UrlPath(id): UrlPath<String>) -> ApiResult {
    if st.tokens.lock().await.revoke(&id)? {
        Ok(Json(json!({"revoked": id})))
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("No token {id}."),
        ))
    }
}

async fn engines_route(State(st): S) -> ApiResult {
    let hw = st.hardware().await;
    let tag = engine::llamacpp_tag();
    let choice = engine::choose_asset(&hw, &tag).ok();
    let inst = engine::installed(&paths::engines_dir(), &hw);
    let base = engine::ollama_base();
    let ollama = engine::ollama_version(&base).await;
    Ok(Json(json!({
        "llama.cpp": {"tag": tag, "build": choice, "installed": inst},
        "ollama": {"base_url": base, "running": ollama.is_some(), "version": ollama},
    })))
}

async fn engine_install_route(State(st): S) -> ApiResult {
    let hw = st.hardware().await;
    if let Some(i) = engine::installed(&paths::engines_dir(), &hw) {
        return Ok(Json(json!({"installed": i})));
    }
    if let Some(j) = st
        .jobs
        .lock()
        .await
        .values()
        .find(|j| j.kind == "engine" && j.state == "running")
    {
        return Ok(Json(json!({"job": j.id})));
    }
    let job = st
        .spawn_job(
            new_job("engine", "llama.cpp"),
            move |st, jid, cancel| async move {
                let sink = progress_sink(st.clone(), jid.clone());
                let (_, fetched) =
                    engine::install_llamacpp(&paths::engines_dir(), &hw, &cancel, sink).await?;
                st.job_update(&jid, |j| {
                    j.fetched = fetched;
                    j.done = j.total;
                })
                .await;
                Ok(())
            },
        )
        .await;
    Ok(Json(json!({"job": job})))
}

#[derive(Deserialize, serde::Serialize, Default, Clone)]
pub struct RunReq {
    pub model: String,
    pub engine: Option<String>,
    pub context: Option<u64>,
    pub gpu_layers: Option<u32>,
    #[serde(default)]
    pub embeddings: bool,
}

fn running_json(r: &Running) -> Value {
    let pid = r.loaded.pid;
    json!({
        "model": r.model.id,
        "name": r.model.name,
        "source": r.model.source,
        "path": r.model.path,
        "engine": r.loaded.engine,
        "engine_note": r.loaded.engine_note,
        "port": r.loaded.port,
        "pid": pid,
        "context": r.loaded.context,
        "gpu_layers": r.loaded.gpu_layers,
        "embeddings": r.loaded.embeddings,
        "resident_bytes": pid.and_then(stats::resident_bytes),
        "gpu_bytes": pid.and_then(stats::gpu_bytes),
        "started_at": rfc3339(r.started_at),
    })
}

impl AppState {
    /// Loads a model (or returns it if it is already loaded). Every setting is explicit.
    pub async fn run(self: &Arc<Self>, req: RunReq) -> ApiResult<Value> {
        let _guard = self.load_lock.lock().await;
        let models = self.models().await;
        let Some(model) = store::resolve(&models, &req.model).cloned() else {
            let in_catalog = catalog::catalog()
                .models
                .iter()
                .any(|m| m.id == req.model.trim_start_matches("fonsviva:"));
            let e = ApiError::new(
                StatusCode::NOT_FOUND,
                "not_installed",
                format!("{} is not installed.", req.model),
            );
            return Err(if in_catalog {
                e.fix(format!(
                    "Download it first: fonsviva models pull {}",
                    req.model
                ))
            } else {
                e.fix("See `fonsviva models list` for installed models.")
            });
        };
        if let Some(k) = self.live(&model.id).await
            && let Some(r) = self.running.lock().await.get(&k)
        {
            return Ok(running_json(r));
        }
        let hw = self.hardware().await;
        let Some(shape) = model.shape.clone() else {
            return Err(bad(
                "unreadable",
                format!("Couldn't read the GGUF header of {}.", model.path.display()),
            ));
        };
        let fit = catalog::fit_best(&shape, model.bytes, &hw, req.context);
        if fit.fit == Verdict::No {
            return Err(
                ApiError::new(StatusCode::CONFLICT, "no_fit", fit.reason.clone())
                    .fix("Pick a smaller model or a smaller context."),
            );
        }
        let spec = LoadSpec {
            context: fit.context,
            gpu_layers: req.gpu_layers.unwrap_or(fit.gpu_layers),
            embeddings: req.embeddings,
            model: model.clone(),
        };
        let mut engine_name = req.engine.clone().unwrap_or_else(|| "llama.cpp".into());
        let mut note = None;
        if engine_name == "llama.cpp" && model.has_projector && model.source == "ollama" {
            engine_name = "ollama".into();
            note = Some(
                "This Ollama model keeps a separate vision projector, so Ollama serves it."
                    .to_string(),
            );
        }
        let result = match engine_name.as_str() {
            "ollama" => {
                Ollama {
                    base: engine::ollama_base(),
                }
                .load(&spec)
                .await
            }
            "llama.cpp" => {
                let inst = match engine::installed(&paths::engines_dir(), &hw) {
                    Some(i) => i,
                    None => {
                        let (i, _) = engine::install_llamacpp(
                            &paths::engines_dir(),
                            &hw,
                            &Cancel::default(),
                            |_| {},
                        )
                        .await?;
                        i
                    }
                };
                let r = LlamaCpp {
                    server: inst.server,
                    logs: paths::logs_dir(),
                }
                .load(&spec)
                .await;
                match r {
                    Err(e)
                        if model.source == "ollama"
                            && engine::unsupported_by_llamacpp(&format!("{e:#}")) =>
                    {
                        note = Some(format!(
                            "llama.cpp {} can't load this model ({}), so Ollama serves it.",
                            engine::llamacpp_tag(),
                            first_line(&format!("{e:#}"))
                        ));
                        Ollama {
                            base: engine::ollama_base(),
                        }
                        .load(&spec)
                        .await
                    }
                    r => r,
                }
            }
            other => {
                return Err(bad(
                    "engine",
                    format!("Unknown engine {other}; use llama.cpp or ollama."),
                ));
            }
        };
        let (mut loaded, handle) = result
            .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, "load_failed", format!("{e:#}")))?;
        loaded.engine_note = note;
        let r = Running {
            model,
            loaded,
            handle,
            started_at: unix_now(),
        };
        let mut out = running_json(&r);
        out["fit"] = serde_json::to_value(&fit).unwrap_or_default();
        out["url"] = json!(format!("http://127.0.0.1:{}/v1", self.port));
        self.running.lock().await.insert(r.model.id.clone(), r);
        self.emit("run", json!({"state": "loaded", "model": out["model"], "context": out["context"], "engine": out["engine"]}));
        Ok(out)
    }

    /// The running entry that serves `want`. An engine that exited on its own
    /// is dropped here, so the caller loads it again instead of calling a
    /// dead port.
    async fn live(&self, want: &str) -> Option<String> {
        let mut running = self.running.lock().await;
        let k = running
            .iter()
            .find(|(k, r)| k.as_str() == want || model_matches(&r.model, want))
            .map(|(k, _)| k.clone())?;
        if !running.get_mut(&k)?.handle.exited() {
            return Some(k);
        }
        running.remove(&k);
        drop(running);
        self.emit("run", json!({"state": "exited", "model": k}));
        None
    }

    async fn stop_model(&self, id: Option<&str>) -> Vec<String> {
        let mut running = self.running.lock().await;
        let keys: Vec<String> = running
            .iter()
            .filter(|(k, r)| id.is_none_or(|w| model_matches(&r.model, w) || k.as_str() == w))
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = vec![];
        for k in keys {
            if let Some(r) = running.remove(&k) {
                let _ = r.handle.unload().await;
                out.push(k);
            }
        }
        drop(running);
        for k in &out {
            self.emit("run", json!({"state": "stopped", "model": k}));
        }
        out
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(160).collect()
}

fn model_matches(m: &Installed, want: &str) -> bool {
    m.id == want
        || m.catalog_id.as_deref() == Some(want)
        || m.ollama_name.as_deref() == Some(want)
        || m.ollama_name
            .as_deref()
            .map(|n| n.trim_end_matches(":latest"))
            == Some(want)
}

async fn run_route(State(st): S, Json(r): Json<RunReq>) -> ApiResult {
    st.run(r).await.map(Json)
}

async fn running_route(State(st): S) -> ApiResult {
    let mut running = st.running.lock().await;
    let mut gone = vec![];
    for (k, r) in running.iter_mut() {
        if r.handle.exited() {
            gone.push(k.clone());
        }
    }
    for k in &gone {
        running.remove(k);
    }
    let list: Vec<Value> = running.values().map(running_json).collect();
    Ok(Json(json!({"running": list})))
}

#[derive(Deserialize, Default)]
struct StopReq {
    model: Option<String>,
}

async fn stop_route(State(st): S, body: Option<Json<StopReq>>) -> ApiResult {
    let model = body.and_then(|b| b.0.model);
    let stopped = st.stop_model(model.as_deref()).await;
    Ok(Json(json!({"stopped": stopped})))
}

async fn stats_route(State(st): S) -> ApiResult {
    let stats = st.stats.lock().await;
    let running = st.running.lock().await;
    let mut out = vec![];
    for (id, r) in running.iter() {
        let roll = stats.get(id).cloned().unwrap_or_default();
        out.push(json!({
            "model": id,
            "engine": r.loaded.engine,
            "context": r.loaded.context,
            "last": roll.last,
            "rolling": roll.averages(),
            "requests": roll.count,
            "resident_bytes": r.loaded.pid.and_then(stats::resident_bytes),
            "gpu_bytes": r.loaded.pid.and_then(stats::gpu_bytes),
        }));
    }
    Ok(Json(json!({"models": out})))
}

async fn shutdown_route(State(st): S) -> ApiResult {
    st.stopping.send_replace(true);
    Ok(Json(json!({"stopping": true})))
}

async fn events_route(
    State(st): S,
) -> Sse<impl futures_util::Stream<Item = std::result::Result<Event, std::convert::Infallible>>> {
    let rx = st.events.subscribe();
    let mut stop = st.stopping.subscribe();
    let stream = BroadcastStream::new(rx)
        .filter_map(|m| async move {
            let (kind, data) = m.ok()?;
            Some(Ok(Event::default().event(kind).data(data.to_string())))
        })
        // The stream ends when shutdown starts, so it never holds the daemon open.
        .take_until(async move {
            let _ = stop.wait_for(|s| *s).await;
        });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ---------------------------------------------------------- measurements

/// The fixed benchmark prompt set.
pub const BENCH_PROMPTS: [&str; 3] = [
    "Name three primary colors.",
    "Explain in one paragraph why the sky is blue.",
    "Write a Python function that returns the n-th Fibonacci number, with a short docstring.",
];

#[derive(Deserialize)]
struct BenchReq {
    model: String,
}

async fn bench_route(State(st): S, Json(r): Json<BenchReq>) -> ApiResult {
    let loaded = st
        .run(RunReq {
            model: r.model.clone(),
            ..Default::default()
        })
        .await?;
    let id = loaded["model"].as_str().unwrap_or_default().to_string();
    let hw = st.hardware().await;
    let mut samples = vec![];
    for p in BENCH_PROMPTS {
        let body = json!({"model": id, "messages": [{"role": "user", "content": p}], "max_tokens": 128,
            "temperature": 0, "stream": true});
        let resp = forward(&st, &id, "/chat/completions", body).await?;
        // Drain the stream; the tap records the sample.
        let mut s = resp.into_body().into_data_stream();
        while s.next().await.is_some() {}
        if let Some(last) = st.stats.lock().await.get(&id).and_then(|r| r.last.clone()) {
            samples.push(last);
        }
    }
    let n = samples.len().max(1) as f64;
    let summary = json!({
        "ttft_ms": samples.iter().map(|s| s.ttft_ms).sum::<f64>() / n,
        "prompt_tps": samples.iter().map(|s| s.prompt_tps).sum::<f64>() / n,
        "gen_tps": samples.iter().map(|s| s.gen_tps).sum::<f64>() / n,
        "wall_gen_tps": samples.iter().map(|s| s.wall_gen_tps).sum::<f64>() / n,
    });
    let result = json!({
        "model": id,
        "engine": loaded["engine"],
        "llama_cpp": engine::llamacpp_tag(),
        "context": loaded["context"],
        "gpu_layers": loaded["gpu_layers"],
        "resident_bytes": loaded["resident_bytes"],
        "hardware": {"os": hw.os, "arch": hw.arch, "cpu": hw.cpu.brand, "backend": hw.backend,
                     "gpus": hw.gpus.iter().map(|g| g.name.clone()).collect::<Vec<_>>(), "ram_total": hw.ram_total},
        "date": rfc3339(unix_now()),
        "samples": samples,
        "summary": summary,
    });
    let dir = paths::bench_dir();
    std::fs::create_dir_all(&dir).map_err(anyhow::Error::from)?;
    std::fs::write(
        dir.join(format!("{}.json", fonsviva_core::util::slug(&id))),
        serde_json::to_vec_pretty(&result).unwrap_or_default(),
    )
    .map_err(anyhow::Error::from)?;
    Ok(Json(result))
}

// ------------------------------------------------------------- /v1 proxy

/// Cloud models routable through /v1. The cloud gate and pods belong to
/// fonsviva-runpod, which is not built yet, so there are none.
fn cloud_models() -> Vec<Value> {
    vec![]
}

/// A cloud model id, as fonsviva-runpod will name them.
fn is_cloud_model(id: &str) -> bool {
    id.starts_with("cloud:")
}

async fn v1_models(State(st): S, Extension(key): Extension<KeyRecord>) -> ApiResult {
    let running = st.running.lock().await;
    let models = st.models().await;
    let mut data: Vec<Value> = models
        .iter()
        .map(|m| {
            json!({
                "id": m.catalog_id.clone().or(m.ollama_name.clone()).unwrap_or(m.id.clone()),
                "object": "model",
                "owned_by": m.source,
                "fonsviva_id": m.id,
                "location": "local",
                "loaded": running.contains_key(&m.id),
                "context": running.get(&m.id).map(|r| r.loaded.context),
            })
        })
        .collect();
    // A plugin key never sees a cloud model.
    if !key.is_plugin() {
        data.extend(cloud_models());
    }
    Ok(Json(json!({"object": "list", "data": data})))
}

async fn v1_proxy(State(st): S, req: Request) -> ApiResult<Response> {
    let key = req.extensions().get::<KeyRecord>().cloned();
    let path = req.uri().path().trim_start_matches("/v1").to_string();
    let bytes = axum::body::to_bytes(req.into_body(), 64 << 20)
        .await
        .map_err(|e| bad("body", e.to_string()))?;
    let body: Value = serde_json::from_slice(&bytes)
        .map_err(|e| bad("json", format!("Body is not JSON: {e}")))?;
    let Some(want) = body["model"].as_str().map(str::to_string) else {
        return Err(bad("model", "Say which model to use in \"model\"."));
    };
    if is_cloud_model(&want) {
        if key.is_some_and(|k| k.is_plugin()) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "cloud_blocked",
                "Plugin keys reach local models only.",
            ));
        }
        // /v1 never starts, loads or resumes cloud capacity; the gate is fonsviva-runpod's.
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "not_built",
            "Cloud models are not built yet.",
        ));
    }
    let id = match st.live(&want).await {
        Some(id) => id,
        // Load on demand when it fits.
        None => st
            .run(RunReq {
                model: want.clone(),
                embeddings: path == "/embeddings",
                ..Default::default()
            })
            .await?["model"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    };
    forward(&st, &id, &path, body).await
}

/// Sends an OpenAI-style request to the engine serving `id`, streaming the
/// answer back while recording timings.
async fn forward(st: &Arc<AppState>, id: &str, path: &str, mut body: Value) -> ApiResult<Response> {
    let (engine_name, base, ollama_name, ctx, loaded) = {
        let running = st.running.lock().await;
        let r = running.get(id).ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "not_loaded",
                format!("{id} is not loaded."),
            )
        })?;
        (
            r.loaded.engine.clone(),
            r.loaded.base_url.clone(),
            r.model.ollama_name.clone(),
            r.loaded.context,
            r.loaded.clone(),
        )
    };
    let stream = body["stream"].as_bool().unwrap_or(false);
    let client = reqwest::Client::new();
    let sent = Instant::now();
    let model_label = body["model"].as_str().unwrap_or(id).to_string();
    if engine_name == "ollama" {
        let name = ollama_name.unwrap_or_default();
        let (url, payload) = match path {
            "/chat/completions" => (
                format!("{base}/api/chat"),
                engine::openai_to_ollama_chat(&body, &name, ctx),
            ),
            "/completions" => (
                format!("{base}/api/generate"),
                json!({"model": name, "prompt": body["prompt"], "stream": stream, "raw": true,
                       "options": {"num_ctx": ctx, "num_predict": body["max_tokens"], "temperature": body["temperature"]}}),
            ),
            "/embeddings" => (
                format!("{base}/api/embed"),
                json!({"model": name, "input": body["input"], "options": {"num_ctx": ctx}}),
            ),
            _ => return Err(bad("path", "unsupported")),
        };
        let resp = client
            .post(url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| upstream(&e))?;
        return Ok(ollama_response(
            st.clone(),
            id.to_string(),
            model_label,
            path.to_string(),
            resp,
            stream,
            sent,
        )
        .await);
    }
    if stream && path != "/embeddings" {
        // Ask llama-server for usage in the final chunk too.
        body["stream_options"] = json!({"include_usage": true});
    }
    let resp = loaded
        .authed(client.post(format!("{base}/v1{path}")))
        .json(&body)
        .send()
        .await
        .map_err(|e| upstream(&e))?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let ctype = resp.headers().get(header::CONTENT_TYPE).cloned();
    let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, std::io::Error>>(32);
    let st2 = st.clone();
    let id2 = id.to_string();
    tokio::spawn(async move {
        let mut tap = Tap::new(sent);
        let mut up = resp.bytes_stream();
        let mut whole = Vec::new();
        while let Some(chunk) = up.next().await {
            let Ok(chunk) = chunk else { break };
            if stream {
                tap.feed_sse(&chunk);
            } else {
                whole.extend_from_slice(&chunk);
            }
            if tx.send(Ok(chunk)).await.is_err() {
                return;
            }
        }
        if !stream && let Ok(v) = serde_json::from_slice::<Value>(&whole) {
            tap.first = tap.first.or(Some(Instant::now()));
            tap.last = Instant::now();
            tap.timings = v.get("timings").and_then(Timings::from_llama);
        }
        if let Some(s) = tap.sample() {
            st2.record(&id2, s).await;
        }
    });
    let mut r = Response::builder().status(status);
    if let Some(c) = ctype {
        r = r.header(header::CONTENT_TYPE, c);
    }
    Ok(r.body(Body::from_stream(ReceiverStream::new(rx)))
        .expect("response"))
}

fn upstream(e: &reqwest::Error) -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "engine",
        format!("The engine didn't answer: {e}"),
    )
}

impl AppState {
    async fn record(&self, id: &str, s: Sample) {
        self.stats
            .lock()
            .await
            .entry(id.to_string())
            .or_default()
            .push(s.clone());
        self.emit("stats", json!({"model": id, "last": s}));
    }
}

/// Watches an SSE stream for the first token and the final timings.
struct Tap {
    sent: Instant,
    first: Option<Instant>,
    last: Instant,
    buf: String,
    timings: Option<Timings>,
}

impl Tap {
    fn new(sent: Instant) -> Tap {
        Tap {
            sent,
            first: None,
            last: sent,
            buf: String::new(),
            timings: None,
        }
    }

    fn feed_sse(&mut self, chunk: &[u8]) {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        while let Some(i) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=i).collect();
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(data.trim()) else {
                continue;
            };
            self.observe(&v);
        }
    }

    fn observe(&mut self, v: &Value) {
        let d = &v["choices"][0]["delta"];
        let text = |k: &str| d[k].as_str().is_some_and(|s| !s.is_empty());
        let token = text("content")
            || text("reasoning_content")
            || d.get("tool_calls").is_some()
            || v["choices"][0]["text"]
                .as_str()
                .is_some_and(|s| !s.is_empty());
        if token {
            self.first.get_or_insert_with(Instant::now);
            self.last = Instant::now();
        }
        if let Some(t) = v.get("timings").and_then(Timings::from_llama) {
            self.timings = Some(t);
        }
    }

    fn sample(&self) -> Option<Sample> {
        let first = self.first?;
        let t = self.timings.clone()?;
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        Some(t.sample(
            ms(first - self.sent),
            ms(self.last.saturating_duration_since(first)),
            unix_now(),
        ))
    }
}

/// Converts Ollama's native answer to OpenAI's shape, with timings.
async fn ollama_response(
    st: Arc<AppState>,
    id: String,
    model: String,
    path: String,
    resp: reqwest::Response,
    stream: bool,
    sent: Instant,
) -> Response {
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return ApiError::new(
            StatusCode::BAD_GATEWAY,
            "engine",
            format!("Ollama said: {text}"),
        )
        .into_response();
    }
    let created = unix_now();
    let cid = format!("chatcmpl-{}", auth::random_hex(8));
    if path == "/embeddings" {
        let v: Value = resp.json().await.unwrap_or_default();
        let data: Vec<Value> = v["embeddings"]
            .as_array()
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(i, e)| json!({"object": "embedding", "index": i, "embedding": e}))
                    .collect()
            })
            .unwrap_or_default();
        return Json(json!({"object": "list", "data": data, "model": model})).into_response();
    }
    let chat = path == "/chat/completions";
    let to_openai = move |v: &Value, stream: bool| -> Value {
        if chat {
            engine::ollama_chat_to_openai(v, &cid, &model, created, stream)
        } else {
            let mut o = json!({"id": cid, "object": "text_completion", "created": created, "model": model,
                "choices": [{"index": 0, "text": v["response"], "finish_reason": v["done"].as_bool().unwrap_or(false).then_some("stop")}]});
            if v["done"].as_bool() == Some(true) {
                o["usage"] = json!({"prompt_tokens": v["prompt_eval_count"], "completion_tokens": v["eval_count"]});
            }
            o
        }
    };
    if !stream {
        let v: Value = resp.json().await.unwrap_or_default();
        let first = Instant::now();
        if let Some(t) = Timings::from_ollama(&v) {
            let ms = |d: Duration| d.as_secs_f64() * 1000.0;
            // Without streaming, the first token time is approximated by total minus generation.
            let ttft = (ms(first - sent) - t.gen_ms).max(0.0);
            st.record(&id, t.sample(ttft, t.gen_ms, unix_now())).await;
        }
        return Json(to_openai(&v, false)).into_response();
    }
    let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, std::io::Error>>(32);
    tokio::spawn(async move {
        let mut up = resp.bytes_stream();
        let mut buf = String::new();
        let mut first = None;
        let mut last = sent;
        while let Some(Ok(chunk)) = up.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(i) = buf.find('\n') {
                let line: String = buf.drain(..=i).collect();
                let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                let has_text = v["message"]["content"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                    || v["message"]["thinking"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty())
                    || v["response"].as_str().is_some_and(|s| !s.is_empty());
                if has_text {
                    first.get_or_insert_with(Instant::now);
                    last = Instant::now();
                }
                let out = format!("data: {}\n\n", to_openai(&v, true));
                if tx.send(Ok(Bytes::from(out))).await.is_err() {
                    return;
                }
                if v["done"].as_bool() == Some(true) {
                    if let (Some(f), Some(t)) = (first, Timings::from_ollama(&v)) {
                        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
                        st.record(
                            &id,
                            t.sample(
                                ms(f - sent),
                                ms(last.saturating_duration_since(f)),
                                unix_now(),
                            ),
                        )
                        .await;
                    }
                    let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
                }
            }
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .expect("response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_scopes() {
        let a = |m: Method, p: &str| action_for(&m, p);
        assert_eq!(a(Method::GET, "/api/v1/keys"), Action::Keys);
        assert_eq!(a(Method::POST, "/api/v1/plugin-keys"), Action::PluginKeys);
        assert_eq!(
            a(Method::DELETE, "/api/v1/plugin-keys/k1"),
            Action::PluginKeys
        );
        assert_eq!(a(Method::POST, "/api/v1/keys"), Action::Keys);
        assert_eq!(a(Method::POST, "/api/v1/tokens"), Action::MintWatch);
        assert_eq!(a(Method::DELETE, "/api/v1/tokens/tok_1"), Action::MintWatch);
        assert_eq!(a(Method::POST, "/api/v1/models/pull"), Action::Pull);
        assert_eq!(a(Method::DELETE, "/api/v1/models/ollama:x"), Action::Pull);
        assert_eq!(a(Method::POST, "/api/v1/run"), Action::Run);
        assert_eq!(a(Method::GET, "/api/v1/running"), Action::Read);
        assert_eq!(
            a(Method::POST, "/api/v1/cloud/runpod/pods"),
            Action::CloudSpend
        );
        assert_eq!(
            a(Method::DELETE, "/api/v1/cloud/runpod/pods/p1"),
            Action::CloudSpend
        );
        assert_eq!(
            a(Method::POST, "/api/v1/cloud/runpod/pods/p1/stop"),
            Action::PodStop
        );
        assert_eq!(a(Method::POST, "/api/v1/wire/pi"), Action::Wire);
        assert_eq!(a(Method::POST, "/api/v1/jobs/j/cancel"), Action::CancelJob);
    }

    #[test]
    fn tap_reads_llama_stream() {
        let sent = Instant::now();
        let mut t = Tap::new(sent);
        t.feed_sse(b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n");
        assert!(t.first.is_none(), "a role-only chunk is not a token");
        t.feed_sse(b"data: {\"choices\":[{\"delta\":{\"content\":\"Re");
        t.feed_sse(b"d\"}}]}\n\ndata: {\"choices\":[{\"delta\":{}, \"finish_reason\":\"stop\"}],\"timings\":{\"prompt_n\":12,\"prompt_ms\":60.0,\"predicted_n\":9,\"predicted_ms\":300.0}}\n\ndata: [DONE]\n\n");
        let s = t.sample().unwrap();
        assert_eq!(s.prompt_tokens, 12);
        assert_eq!(s.gen_tokens, 9);
        assert_eq!(s.gen_tps, 30.0);
        assert!(s.ttft_ms >= 0.0);
    }
}
