//! A mock RunPod server (axum) for tests and the desktop app's demo mode.
//! It serves every REST v2 endpoint [`crate::RunPodClient`] calls, checks the
//! bearer key, and bills each pod per second on its own clock, the way
//! RunPod does: GPU, container disk and volume while running, the volume at
//! the stopped rate after a stop, nothing after terminate.
//!
//! Tests move its clock with [`MockRunPod::advance`]; demo mode runs it in
//! real time. `/_mock/*` routes expose the same hooks over HTTP, and
//! `/_mock/proxy/{pod}/v1` answers like a pod's OpenAI-compatible server.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client::{GpuPrice, GpuType, MaxCount, Mounts, PersistentMount, Pod, PodGpu, PodStatus};
use crate::estimate::{
    CONTAINER_DISK_PER_GB_MONTH, HOURS_PER_MONTH, VOLUME_RUNNING_PER_GB_MONTH,
    VOLUME_STOPPED_PER_GB_MONTH,
};

/// The catalog the mock serves: Secure and Community list prices per GPU
/// from runpod.io/pricing on 2026-11-17.
pub const MOCK_GPUS: [(&str, &str, u32, f64, f64); 9] = [
    ("NVIDIA RTX A5000", "RTX A5000", 24, 0.27, 0.16),
    ("NVIDIA GeForce RTX 3090", "RTX 3090", 24, 0.50, 0.22),
    ("NVIDIA L4", "L4", 24, 0.69, 0.44),
    ("NVIDIA GeForce RTX 4090", "RTX 4090", 24, 0.89, 0.34),
    ("NVIDIA A40", "A40", 48, 0.59, 0.35),
    ("NVIDIA RTX A6000", "RTX A6000", 48, 0.69, 0.33),
    ("NVIDIA L40S", "L40S", 48, 1.09, 0.79),
    ("NVIDIA A100 80GB PCIe", "A100 PCIe", 80, 2.09, 1.19),
    ("NVIDIA H100 80GB HBM3", "H100 SXM", 80, 4.39, 2.69),
];

/// Top-level keys RunPod's `CreatePodRequest` accepts; anything else is a
/// client bug the real API would refuse.
const CREATE_KEYS: [&str; 21] = [
    "name",
    "image",
    "entrypoint",
    "cmd",
    "args",
    "disk",
    "env",
    "ports",
    "cloud",
    "gpu",
    "mounts",
    "templateId",
    "dataCenterIds",
    "cpu",
    "globalNetworking",
    "startJupyter",
    "startSsh",
    "queue",
    "registry",
    "costCenter",
    "tags",
];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MockBilling {
    pub pod: String,
    pub name: String,
    pub status: PodStatus,
    pub running_seconds: f64,
    pub stopped_seconds: f64,
    /// USD billed so far.
    pub amount: f64,
}

struct MockPod {
    pod: Pod,
    gpu_rate: f64,
    disk_gb: u32,
    volume_gb: u32,
    running_seconds: f64,
    stopped_seconds: f64,
    settled_at: f64,
    server_key: Option<String>,
}

impl MockPod {
    fn amount(&self) -> f64 {
        let disk = f64::from(self.disk_gb) * CONTAINER_DISK_PER_GB_MONTH / HOURS_PER_MONTH;
        let volume_running =
            f64::from(self.volume_gb) * VOLUME_RUNNING_PER_GB_MONTH / HOURS_PER_MONTH;
        let volume_stopped =
            f64::from(self.volume_gb) * VOLUME_STOPPED_PER_GB_MONTH / HOURS_PER_MONTH;
        self.running_seconds / 3600.0 * (self.gpu_rate + disk + volume_running)
            + self.stopped_seconds / 3600.0 * volume_stopped
    }

    fn billing(&self) -> MockBilling {
        MockBilling {
            pod: self.pod.id.clone(),
            name: self.pod.name.clone(),
            status: self.pod.status,
            running_seconds: self.running_seconds,
            stopped_seconds: self.stopped_seconds,
            amount: self.amount(),
        }
    }
}

struct MockState {
    key: String,
    real_time: Option<Instant>,
    offset: f64,
    gpus: Vec<GpuType>,
    pods: BTreeMap<String, MockPod>,
    failing: BTreeSet<String>,
    requests: u64,
    next_id: u64,
}

impl MockState {
    fn now(&self) -> f64 {
        self.offset
            + self
                .real_time
                .map_or(0.0, |start| start.elapsed().as_secs_f64())
    }

    /// Bills every pod up to now.
    fn settle(&mut self) {
        let now = self.now();
        for pod in self.pods.values_mut() {
            let elapsed = (now - pod.settled_at).max(0.0);
            match pod.pod.status {
                PodStatus::Running | PodStatus::Starting | PodStatus::Provisioning => {
                    pod.running_seconds += elapsed
                }
                PodStatus::Exited => pod.stopped_seconds += elapsed,
                _ => {}
            }
            pod.settled_at = now;
        }
    }
}

type Shared = Arc<Mutex<MockState>>;

fn lock(state: &Shared) -> MutexGuard<'_, MockState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A running mock server. Dropping it shuts the server down.
pub struct MockRunPod {
    url: String,
    state: Shared,
    task: tokio::task::JoinHandle<()>,
}

impl MockRunPod {
    /// Starts a mock on 127.0.0.1 with its clock stopped; `key` is the only
    /// bearer token it accepts.
    pub async fn start(key: &str) -> std::io::Result<Self> {
        Self::launch(key, false).await
    }

    /// Starts a mock whose clock follows real time, for demo mode.
    pub async fn start_real_time(key: &str) -> std::io::Result<Self> {
        Self::launch(key, true).await
    }

    async fn launch(key: &str, real_time: bool) -> std::io::Result<Self> {
        let gpus = MOCK_GPUS
            .iter()
            .map(|(id, name, memory, secure, community)| GpuType {
                id: id.to_string(),
                name: name.to_string(),
                memory: *memory,
                secure: true,
                community: true,
                price: GpuPrice {
                    secure: *secure,
                    community: *community,
                    serverless: None,
                },
                max_count: Some(MaxCount {
                    secure: 8,
                    community: 8,
                }),
                availability: Some("HIGH".to_string()),
            })
            .collect();
        let state = Arc::new(Mutex::new(MockState {
            key: key.to_string(),
            real_time: real_time.then(Instant::now),
            offset: 0.0,
            gpus,
            pods: BTreeMap::new(),
            failing: BTreeSet::new(),
            requests: 0,
            next_id: 1,
        }));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let url = format!("http://{}", listener.local_addr()?);
        let app = router(state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { url, state, task })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Moves the mock's billing clock forward.
    pub fn advance(&self, seconds: f64) {
        let mut state = lock(&self.state);
        state.settle();
        state.offset += seconds.max(0.0);
        state.settle();
    }

    pub fn billing(&self) -> Vec<MockBilling> {
        let mut state = lock(&self.state);
        state.settle();
        state.pods.values().map(MockPod::billing).collect()
    }

    pub fn total_billed(&self) -> f64 {
        self.billing().iter().map(|bill| bill.amount).sum()
    }

    pub fn pod_status(&self, id: &str) -> Option<PodStatus> {
        lock(&self.state).pods.get(id).map(|pod| pod.pod.status)
    }

    pub fn pod_ids(&self) -> Vec<String> {
        lock(&self.state).pods.keys().cloned().collect()
    }

    /// Makes `GET /v2/pods/{id}` fail, as if Fonsviva lost sight of the pod.
    pub fn set_failing(&self, id: &str, failing: bool) {
        let mut state = lock(&self.state);
        if failing {
            state.failing.insert(id.to_string());
        } else {
            state.failing.remove(id);
        }
    }

    pub fn set_price(&self, gpu: &str, secure: f64) {
        let mut state = lock(&self.state);
        if let Some(found) = state.gpus.iter_mut().find(|found| found.id == gpu) {
            found.price.secure = secure;
        }
    }

    /// Every request the RunPod API routes have received, authorized or not.
    pub fn request_count(&self) -> u64 {
        lock(&self.state).requests
    }

    /// Adds a running pod as if another session had created it.
    pub fn add_pod(&self, name: &str, gpu: &str) -> String {
        let mut state = lock(&self.state);
        state.settle();
        let rate = state
            .gpus
            .iter()
            .find(|found| found.id == gpu)
            .map_or(0.0, |found| found.price.secure);
        insert_pod(&mut state, name, gpu, 1, rate, 20, 20, None).id
    }
}

impl Drop for MockRunPod {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn router(state: Shared) -> Router {
    Router::new()
        .route("/v2/pods", get(list_pods).post(create_pod))
        .route("/v2/pods/{id}", get(get_pod).delete(delete_pod))
        .route("/v2/pods/{id}/action", post(pod_action))
        .route("/v2/catalog/gpus", get(list_gpus))
        .route("/v2/catalog/templates", get(list_templates))
        .route("/_mock/advance", post(hook_advance))
        .route("/_mock/billing", get(hook_billing))
        .route("/_mock/fail", post(hook_fail))
        .route("/_mock/price", post(hook_price))
        .route("/_mock/proxy/{id}/v1/models", get(proxy_models))
        .route("/_mock/proxy/{id}/v1/chat/completions", post(proxy_chat))
        .with_state(state)
}

fn problem(status: StatusCode, detail: &str) -> Response {
    let title = status.canonical_reason().unwrap_or("Error");
    (
        status,
        Json(json!({"title": title, "status": status.as_u16(), "detail": detail})),
    )
        .into_response()
}

/// Counts the request and checks the bearer key.
fn authorize<'a>(
    state: &'a Shared,
    headers: &HeaderMap,
) -> Result<MutexGuard<'a, MockState>, Box<Response>> {
    let mut guard = lock(state);
    guard.requests += 1;
    let expected = format!("Bearer {}", guard.key);
    match headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) if value == expected => {
            guard.settle();
            Ok(guard)
        }
        _ => Err(Box::new(problem(
            StatusCode::UNAUTHORIZED,
            "missing or wrong API key",
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_pod(
    state: &mut MockState,
    name: &str,
    gpu: &str,
    count: u32,
    gpu_rate: f64,
    disk_gb: u32,
    volume_gb: u32,
    server_key: Option<String>,
) -> Pod {
    let id = format!("mockpod{:04}", state.next_id);
    state.next_id += 1;
    let pod = Pod {
        id: id.clone(),
        name: name.to_string(),
        status: PodStatus::Running,
        cost: gpu_rate * f64::from(count),
        image: None,
        gpu: Some(PodGpu {
            id: gpu.to_string(),
            count,
        }),
        disk: Some(disk_gb),
        mounts: (volume_gb > 0).then(|| Mounts {
            persistent: Some(PersistentMount {
                size: volume_gb,
                path: "/workspace".into(),
            }),
        }),
        ports: Vec::new(),
        created_at: None,
        started_at: None,
    };
    let now = state.now();
    state.pods.insert(
        id,
        MockPod {
            pod: pod.clone(),
            gpu_rate: gpu_rate * f64::from(count),
            disk_gb,
            volume_gb,
            running_seconds: 0.0,
            stopped_seconds: 0.0,
            settled_at: now,
            server_key,
        },
    );
    pod
}

async fn list_pods(State(state): State<Shared>, headers: HeaderMap) -> Response {
    let state = match authorize(&state, &headers) {
        Ok(state) => state,
        Err(response) => return *response,
    };
    let pods: Vec<&Pod> = state
        .pods
        .values()
        .filter(|pod| pod.pod.status != PodStatus::Terminated)
        .map(|pod| &pod.pod)
        .collect();
    Json(json!({"pods": pods, "pagination": {"nextCursor": null, "hasNextPage": false}}))
        .into_response()
}

async fn create_pod(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let mut state = match authorize(&state, &headers) {
        Ok(state) => state,
        Err(response) => return *response,
    };
    let Some(fields) = body.as_object() else {
        return problem(StatusCode::BAD_REQUEST, "body must be an object");
    };
    if let Some(unknown) = fields
        .keys()
        .find(|key| !CREATE_KEYS.contains(&key.as_str()))
    {
        return problem(
            StatusCode::BAD_REQUEST,
            &format!("unknown property \"{unknown}\""),
        );
    }
    let name = body["name"].as_str().unwrap_or_default();
    if name.is_empty() || body["image"].as_str().unwrap_or_default().is_empty() {
        return problem(StatusCode::BAD_REQUEST, "name and image are required");
    }
    let gpu = body["gpu"]["id"].as_str().unwrap_or_default();
    let Some(rate) = state
        .gpus
        .iter()
        .find(|found| found.id == gpu)
        .map(|found| found.price.secure)
    else {
        return problem(
            StatusCode::BAD_REQUEST,
            &format!("unknown GPU type \"{gpu}\""),
        );
    };
    let count = body["gpu"]["count"].as_u64().unwrap_or(1).clamp(1, 8) as u32;
    let volume = body["mounts"]["persistent"]["size"].as_u64().unwrap_or(0) as u32;
    if volume != 0 && volume < 10 {
        return problem(
            StatusCode::BAD_REQUEST,
            "mounts.persistent.size must be at least 10",
        );
    }
    let disk = body["disk"].as_u64().unwrap_or(20) as u32;
    let server_key = body["env"]
        .as_object()
        .and_then(|env| env.iter().find(|(key, _)| key.ends_with("_API_KEY")))
        .and_then(|(_, value)| value.as_str())
        .map(str::to_string);
    let mut pod = insert_pod(&mut state, name, gpu, count, rate, disk, volume, server_key);
    pod.image = body["image"].as_str().map(str::to_string);
    pod.ports = body["ports"]
        .as_array()
        .map(|ports| {
            ports
                .iter()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if let Some(stored) = state.pods.get_mut(&pod.id) {
        stored.pod = pod.clone();
    }
    (StatusCode::CREATED, Json(pod)).into_response()
}

async fn get_pod(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let state = match authorize(&state, &headers) {
        Ok(state) => state,
        Err(response) => return *response,
    };
    if state.failing.contains(&id) {
        return problem(StatusCode::SERVICE_UNAVAILABLE, "pod status unavailable");
    }
    match state.pods.get(&id) {
        Some(pod) if pod.pod.status != PodStatus::Terminated => Json(&pod.pod).into_response(),
        _ => problem(StatusCode::NOT_FOUND, "pod not found"),
    }
}

async fn delete_pod(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let mut state = match authorize(&state, &headers) {
        Ok(state) => state,
        Err(response) => return *response,
    };
    match state.pods.get_mut(&id) {
        Some(pod) if pod.pod.status != PodStatus::Terminated => {
            pod.pod.status = PodStatus::Terminated;
            StatusCode::NO_CONTENT.into_response()
        }
        _ => problem(StatusCode::NOT_FOUND, "pod not found"),
    }
}

#[derive(Deserialize)]
struct ActionBody {
    action: String,
}

async fn pod_action(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ActionBody>,
) -> Response {
    let mut state = match authorize(&state, &headers) {
        Ok(state) => state,
        Err(response) => return *response,
    };
    let Some(pod) = state
        .pods
        .get_mut(&id)
        .filter(|pod| pod.pod.status != PodStatus::Terminated)
    else {
        return problem(StatusCode::NOT_FOUND, "pod not found");
    };
    pod.pod.status = match body.action.as_str() {
        "start" | "restart" => PodStatus::Running,
        "stop" => PodStatus::Exited,
        "terminate" => PodStatus::Terminated,
        other => {
            return problem(
                StatusCode::BAD_REQUEST,
                &format!("unknown action \"{other}\""),
            );
        }
    };
    Json(pod.pod.clone()).into_response()
}

async fn list_gpus(State(state): State<Shared>, headers: HeaderMap) -> Response {
    match authorize(&state, &headers) {
        Ok(state) => Json(json!({"gpus": state.gpus})).into_response(),
        Err(response) => *response,
    }
}

async fn list_templates(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }
    Json(json!({
        "templates": [{"id": "runpod-torch-v280", "name": "Runpod Pytorch 2.8.0", "image": "runpod/pytorch:2.8.0-py3.11-cuda12.8.1-cudnn-devel-ubuntu22.04", "serverless": false}],
        "pagination": {"nextCursor": null, "hasNextPage": false}
    }))
    .into_response()
}

#[derive(Deserialize)]
struct AdvanceBody {
    seconds: f64,
}

async fn hook_advance(State(state): State<Shared>, Json(body): Json<AdvanceBody>) -> Response {
    let mut state = lock(&state);
    state.settle();
    state.offset += body.seconds.max(0.0);
    state.settle();
    Json(json!({"now": state.now()})).into_response()
}

async fn hook_billing(State(state): State<Shared>) -> Response {
    let mut state = lock(&state);
    state.settle();
    let pods: Vec<MockBilling> = state.pods.values().map(MockPod::billing).collect();
    let total: f64 = pods.iter().map(|bill| bill.amount).sum();
    Json(json!({"pods": pods, "total": total})).into_response()
}

#[derive(Deserialize)]
struct FailBody {
    pod: String,
    failing: bool,
}

async fn hook_fail(State(state): State<Shared>, Json(body): Json<FailBody>) -> Response {
    let mut state = lock(&state);
    if body.failing {
        state.failing.insert(body.pod);
    } else {
        state.failing.remove(&body.pod);
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct PriceBody {
    gpu: String,
    secure: f64,
}

async fn hook_price(State(state): State<Shared>, Json(body): Json<PriceBody>) -> Response {
    let mut state = lock(&state);
    match state.gpus.iter_mut().find(|found| found.id == body.gpu) {
        Some(found) => {
            found.price.secure = body.secure;
            StatusCode::NO_CONTENT.into_response()
        }
        None => problem(StatusCode::NOT_FOUND, "unknown GPU type"),
    }
}

/// Checks a demo pod's own key, as llama-server and vLLM do.
fn pod_server<'a>(
    state: &'a MockState,
    id: &str,
    headers: &HeaderMap,
) -> Result<&'a MockPod, Box<Response>> {
    let pod = state
        .pods
        .get(id)
        .filter(|pod| pod.pod.status == PodStatus::Running);
    let Some(pod) = pod else {
        return Err(Box::new(problem(
            StatusCode::BAD_GATEWAY,
            "pod is not running",
        )));
    };
    let sent = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    match &pod.server_key {
        Some(key) if sent != Some(format!("Bearer {key}").as_str()) => {
            Err(Box::new(problem(StatusCode::UNAUTHORIZED, "wrong pod key")))
        }
        _ => Ok(pod),
    }
}

async fn proxy_models(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let state = lock(&state);
    match pod_server(&state, &id, &headers) {
        Ok(pod) => Json(json!({"object": "list", "data": [{"id": pod.pod.name, "object": "model", "owned_by": "fonsviva-demo"}]})).into_response(),
        Err(response) => *response,
    }
}

async fn proxy_chat(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let state = lock(&state);
    match pod_server(&state, &id, &headers) {
        Ok(_) => Json(json!({
            "id": "chatcmpl-demo",
            "object": "chat.completion",
            "model": body["model"],
            "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": "This answer came from Fonsviva's demo pod. No GPU was rented."}}],
            "usage": {"prompt_tokens": 0, "completion_tokens": 12, "total_tokens": 12}
        }))
        .into_response(),
        Err(response) => *response,
    }
}
