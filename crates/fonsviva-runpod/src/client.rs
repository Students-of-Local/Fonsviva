//! A small client for RunPod's REST API v2 (`https://api.runpod.io/v2`,
//! spec at `/v2/openapi.json`): pods, GPU types with current prices, and
//! templates. Every call sends the user's key as a bearer token; the key is
//! never logged, and `Pod` drops the pod's `env` so a pod key is never read
//! back into a struct that might be printed.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::key::ApiKey;

pub const DEFAULT_BASE_URL: &str = "https://api.runpod.io";

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("RunPod answered {status}: {detail}")]
    Api { status: u16, detail: String },
    #[error("could not reach RunPod: {0}")]
    Network(String),
    #[error("RunPod sent a reply Fonsviva could not read: {0}")]
    Decode(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PodStatus {
    Provisioning,
    Starting,
    Running,
    Exited,
    Error,
    Terminated,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Pod {
    pub id: String,
    pub name: String,
    pub status: PodStatus,
    /// The hourly rate RunPod bills for this pod's compute.
    #[serde(default)]
    pub cost: f64,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub gpu: Option<PodGpu>,
    #[serde(default)]
    pub disk: Option<u32>,
    #[serde(default)]
    pub mounts: Option<Mounts>,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PodGpu {
    pub id: String,
    #[serde(default = "one")]
    pub count: u32,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Mounts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent: Option<PersistentMount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistentMount {
    /// GB, at least 10.
    pub size: u32,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GpuType {
    pub id: String,
    pub name: String,
    /// GB of VRAM.
    #[serde(default)]
    pub memory: u32,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub community: bool,
    /// USD per hour for one GPU.
    pub price: GpuPrice,
    #[serde(default)]
    pub max_count: Option<MaxCount>,
    #[serde(default)]
    pub availability: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GpuPrice {
    pub secure: f64,
    pub community: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serverless: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MaxCount {
    #[serde(default)]
    pub secure: u32,
    #[serde(default)]
    pub community: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Template {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub serverless: bool,
}

/// The body of `POST /v2/pods`. `env` carries the pod's own API key, so
/// `Debug` lists only the variable names.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatePod {
    pub name: String,
    pub image: String,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    /// Container disk, GB.
    pub disk: u32,
    pub env: BTreeMap<String, String>,
    pub ports: Vec<String>,
    /// `SECURE` or `COMMUNITY`.
    pub cloud: String,
    pub gpu: PodGpu,
    pub mounts: Mounts,
}

impl fmt::Debug for CreatePod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreatePod")
            .field("name", &self.name)
            .field("image", &self.image)
            .field("entrypoint", &self.entrypoint)
            .field("cmd", &self.cmd)
            .field("disk", &self.disk)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("ports", &self.ports)
            .field("cloud", &self.cloud)
            .field("gpu", &self.gpu)
            .field("mounts", &self.mounts)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum PodAction {
    Start,
    Stop,
    Terminate,
}

#[derive(Deserialize)]
struct PodPage {
    pods: Vec<Pod>,
    #[serde(default)]
    pagination: Option<Pagination>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pagination {
    next_cursor: Option<String>,
    #[serde(default)]
    has_next_page: bool,
}

#[derive(Deserialize)]
struct GpuList {
    gpus: Vec<GpuType>,
}

#[derive(Deserialize)]
struct TemplateList {
    templates: Vec<Template>,
}

#[derive(Clone)]
pub struct RunPodClient {
    http: reqwest::Client,
    base: String,
    key: ApiKey,
}

impl fmt::Debug for RunPodClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunPodClient")
            .field("base", &self.base)
            .field("key", &self.key)
            .finish()
    }
}

impl RunPodClient {
    pub fn new(key: ApiKey) -> Self {
        Self::with_base_url(DEFAULT_BASE_URL, key)
    }

    /// A client for another server with the same API, such as [`crate::mock`].
    pub fn with_base_url(base: impl Into<String>, key: ApiKey) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("fonsviva/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            key,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// The OpenAI-compatible base URL of a pod's HTTP port. RunPod serves it
    /// through `https://<pod id>-<port>.proxy.runpod.net`; the mock serves it
    /// under its own address.
    pub fn proxy_url(&self, pod_id: &str, port: u16) -> String {
        if self.base == DEFAULT_BASE_URL {
            format!("https://{pod_id}-{port}.proxy.runpod.net/v1")
        } else {
            format!("{}/_mock/proxy/{pod_id}/v1", self.base)
        }
    }

    pub async fn list_pods(&self) -> Result<Vec<Pod>, ClientError> {
        let mut pods = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut request = self.http.get(format!("{}/v2/pods", self.base));
            if let Some(cursor) = &cursor {
                request = request.query(&[("cursor", cursor.as_str())]);
            }
            let page: PodPage = self.send(request).await?;
            pods.extend(page.pods);
            match page.pagination {
                Some(Pagination {
                    next_cursor: Some(next),
                    has_next_page: true,
                }) => cursor = Some(next),
                _ => return Ok(pods),
            }
        }
    }

    pub async fn get_pod(&self, id: &str) -> Result<Pod, ClientError> {
        self.send(self.http.get(format!("{}/v2/pods/{id}", self.base)))
            .await
    }

    /// Creates a pod. This starts billing, so only [`crate::Cloud`] calls it,
    /// after the confirmation dialog and the gate.
    pub async fn create_pod(&self, body: &CreatePod) -> Result<Pod, ClientError> {
        self.send(self.http.post(format!("{}/v2/pods", self.base)).json(body))
            .await
    }

    pub async fn start_pod(&self, id: &str) -> Result<(), ClientError> {
        self.action(id, PodAction::Start).await
    }

    pub async fn stop_pod(&self, id: &str) -> Result<(), ClientError> {
        self.action(id, PodAction::Stop).await
    }

    pub async fn terminate_pod(&self, id: &str) -> Result<(), ClientError> {
        self.action(id, PodAction::Terminate).await
    }

    /// GPU types with current per-hour list prices.
    pub async fn gpu_types(&self) -> Result<Vec<GpuType>, ClientError> {
        let list: GpuList = self
            .send(self.http.get(format!("{}/v2/catalog/gpus", self.base)))
            .await?;
        Ok(list.gpus)
    }

    /// RunPod's official pod templates.
    pub async fn templates(&self) -> Result<Vec<Template>, ClientError> {
        let request = self
            .http
            .get(format!("{}/v2/catalog/templates", self.base))
            .query(&[("source", "official")]);
        let list: TemplateList = self.send(request).await?;
        Ok(list
            .templates
            .into_iter()
            .filter(|template| !template.serverless)
            .collect())
    }

    async fn action(&self, id: &str, action: PodAction) -> Result<(), ClientError> {
        let request = self
            .http
            .post(format!("{}/v2/pods/{id}/action", self.base))
            .json(&serde_json::json!({ "action": action }));
        self.send_raw(request).await.map(|_| ())
    }

    async fn send<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, ClientError> {
        let body = self.send_raw(request).await?;
        serde_json::from_slice(&body).map_err(|error| ClientError::Decode(error.to_string()))
    }

    async fn send_raw(&self, request: reqwest::RequestBuilder) -> Result<Vec<u8>, ClientError> {
        let response = request
            .bearer_auth(self.key.expose())
            .send()
            .await
            .map_err(|error| ClientError::Network(error.without_url().to_string()))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|error| ClientError::Network(error.without_url().to_string()))?;
        if !status.is_success() {
            let detail = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .get("detail")
                        .or_else(|| value.get("title"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| status.canonical_reason().unwrap_or("error").to_string());
            return Err(ClientError::Api {
                status: status.as_u16(),
                detail,
            });
        }
        Ok(body.to_vec())
    }
}
