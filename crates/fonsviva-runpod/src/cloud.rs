//! The cloud service the daemon holds: the gate, pod start requests and
//! their confirmations, stop and terminate, and the supervisor that tracks
//! cost and stops pods on idle, at a cap, or when it loses sight of them.
//!
//! A start is a request. [`Cloud::request_start`] runs the gate and returns
//! a pending confirmation; nothing reaches RunPod. The daemon then calls
//! [`Cloud::present_confirmation`], which shows the request through the
//! [`Confirmer`] (the desktop's native dialog with the OS user-presence
//! check), and only an approval there, followed by a second full gate run,
//! creates the pod. No other path creates one.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::client::{ClientError, GpuType, PodStatus, RunPodClient};
use crate::clock::Clock;
use crate::estimate::{self, Estimate, round4};
use crate::gate::{Caller, CapKind, Caps, CloudSettings, Profile, Refusal, TokenScope};
use crate::templates::{self, PodTemplate};

/// How long a confirmation waits for the click.
pub const CONFIRMATION_TTL: Duration = Duration::from_secs(5 * 60);
/// Wall and monotonic time may drift this much between ticks before the
/// supervisor calls it a clock jump.
pub const CLOCK_JUMP_TOLERANCE: Duration = Duration::from_secs(60);
/// Every pod Fonsviva creates is named with this prefix, so orphan cleanup
/// can find them.
pub const POD_PREFIX: &str = "fonsviva-";
const ALERT_PERCENTS: [u8; 3] = [50, 80, 100];

#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    #[error("{0}")]
    Refused(Refusal),
    #[error("Fonsviva has not read RunPod's GPU prices yet")]
    PricesUnavailable,
    #[error("unknown template \"{0}\"")]
    UnknownTemplate(String),
    #[error("{0}")]
    UnknownGpu(String),
    #[error("{0}")]
    InvalidRequest(String),
    #[error("this idempotency key belongs to another token")]
    IdempotencyConflict,
    #[error("no pending confirmation with that id")]
    UnknownConfirmation,
    #[error("the confirmation expired after 5 minutes; request the pod again")]
    ConfirmationExpired,
    #[error(
        "another start is waiting for confirmation on screen; answer it first, or let it expire in 5 minutes"
    )]
    Pending,
    #[error("declined in the confirmation dialog")]
    Declined,
    #[error("no pod {0} started by Fonsviva")]
    NotFound(String),
    #[error("set the session, day and month caps before turning cloud on")]
    CapsNotSet,
    #[error(transparent)]
    RunPod(#[from] ClientError),
    #[error("{0}")]
    Internal(String),
}

impl CloudError {
    /// The code the daemon puts in its error JSON.
    pub fn code(&self) -> &'static str {
        match self {
            CloudError::Refused(refusal) => refusal.code(),
            CloudError::PricesUnavailable => "prices_unavailable",
            CloudError::UnknownTemplate(_) => "unknown_template",
            CloudError::UnknownGpu(_) => "unknown_gpu",
            CloudError::InvalidRequest(_) => "invalid_request",
            CloudError::IdempotencyConflict => "idempotency_conflict",
            CloudError::UnknownConfirmation => "unknown_confirmation",
            CloudError::ConfirmationExpired => "confirmation_expired",
            CloudError::Pending => "pending",
            CloudError::Declined => "declined",
            CloudError::NotFound(_) => "not_found",
            CloudError::CapsNotSet => "caps_not_set",
            CloudError::RunPod(_) => "runpod",
            CloudError::Internal(_) => "internal",
        }
    }

    pub fn refusal(&self) -> Option<Refusal> {
        match self {
            CloudError::Refused(refusal) => Some(*refusal),
            _ => None,
        }
    }
}

impl From<Refusal> for CloudError {
    fn from(refusal: Refusal) -> Self {
        CloudError::Refused(refusal)
    }
}

/// The body of `POST /api/v1/cloud/runpod/pods`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartRequest {
    pub template: String,
    pub gpu: String,
    /// The model to serve; the template's default when absent.
    #[serde(default)]
    pub model: Option<String>,
    pub planned_hours: f64,
    pub confirm_cost_per_hour: f64,
    pub idempotency_key: String,
}

/// Who asked, as the dialog names them: the token's scope and label. The
/// daemon adds the calling program where the OS reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestedBy {
    pub scope: TokenScope,
    pub label: Option<String>,
}

impl From<&Caller> for RequestedBy {
    fn from(caller: &Caller) -> Self {
        Self {
            scope: caller.scope,
            label: caller.label.clone(),
        }
    }
}

/// Everything the start dialog shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfirmationView {
    pub confirmation: String,
    pub requested_by: RequestedBy,
    pub template: String,
    pub model: String,
    pub estimate: Estimate,
    pub caps: Caps,
    pub spent_today: f64,
    pub spent_month: f64,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approved,
    Declined,
}

pub type DecisionFuture<'a> = Pin<Box<dyn Future<Output = Decision> + Send + 'a>>;

/// What the terminate dialog shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TerminateView {
    pub requested_by: RequestedBy,
    pub pod: PodView,
    /// The volume that terminating deletes, with the model on it.
    pub volume_gb: u32,
    pub note: String,
}

/// The two things only the person can approve.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Prompt {
    Start(ConfirmationView),
    Terminate(TerminateView),
}

/// Asks the person. The daemon implements this with a native OS dialog and
/// the OS user-presence check (Touch ID or the login password on macOS,
/// Windows Hello, polkit on Linux); it must return `Approved` only when the
/// person clicked and passed that check. Tests use a fake.
pub trait Confirmer: Send + Sync {
    fn ask<'a>(&'a self, prompt: &'a Prompt) -> DecisionFuture<'a>;
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StartOutcome {
    /// Waiting for the dialog; the HTTP layer answers 202 with this.
    Pending(ConfirmationView),
    /// A replayed request whose pod already started.
    Started(PodView),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PodState {
    Running,
    Stopped,
    Terminated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    User,
    Idle,
    BudgetCap,
    DeadMan,
    ClockJump,
    EmergencyStop,
    Orphan,
    /// RunPod reports the pod stopped or failed on its own.
    Exited,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PodView {
    pub id: String,
    pub name: String,
    pub template: String,
    pub model: String,
    pub gpu: String,
    pub gpu_name: String,
    pub state: PodState,
    pub cost_per_hour: f64,
    /// Compute and disk billed while running, per second.
    pub running_cost_so_far: f64,
    /// Volume billed while stopped.
    pub stopped_cost_so_far: f64,
    pub stopped_volume_per_hour: f64,
    pub started_at: DateTime<Utc>,
    pub idle_minutes: f64,
    pub auto_stop_at: Option<DateTime<Utc>>,
    pub stop_reason: Option<StopReason>,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StopReport {
    pub pod: PodView,
    pub stopped_volume_per_hour: f64,
    pub stopped_volume_per_month: f64,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TemplateView {
    #[serde(flatten)]
    pub template: PodTemplate,
    pub gpus: Vec<GpuOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GpuOption {
    #[serde(rename = "gpu")]
    pub id: String,
    pub name: String,
    pub memory_gb: u32,
    pub gpu_per_hour: f64,
    /// GPU, disk and volume: the figure `confirm_cost_per_hour` must match.
    #[serde(rename = "per_hour")]
    pub total_per_hour: f64,
    pub stopped_volume_per_hour: f64,
    pub availability: Option<String>,
}

/// The prefix of every cloud model id in `/v1/models`, followed by the pod id.
pub const CLOUD_MODEL_PREFIX: &str = "cloud:runpod/";

/// A running pod as a `/v1/models` entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CloudModel {
    pub id: String,
    pub object: &'static str,
    pub owned_by: &'static str,
    pub location: &'static str,
    /// The model the pod serves.
    pub model: String,
    pub pod: String,
    pub cost_per_hour: f64,
}

/// Where the daemon's `/v1` proxy sends a cloud model's requests.
#[derive(Debug, Clone)]
pub struct Upstream {
    pub base_url: String,
    pub api_key: SecretString,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CloudEvent {
    ConfirmationRequested {
        confirmation: String,
    },
    Started {
        pod: String,
        cost_per_hour: f64,
    },
    Stopped {
        pod: String,
        reason: StopReason,
        stopped_volume_per_hour: f64,
    },
    Terminated {
        pod: String,
    },
    StopFailed {
        pod: String,
        reason: StopReason,
        error: String,
    },
    Unreachable {
        pod: String,
        failures: u32,
    },
    BudgetAlert {
        cap: CapKind,
        percent: u8,
        spent: f64,
        limit: f64,
    },
    /// Stopped volumes alone reached a cap. Fonsviva never deletes a volume
    /// by itself; it offers to terminate.
    VolumeCapReached {
        cap: CapKind,
        pods: Vec<String>,
        stopped_volume_per_hour: f64,
        offer: String,
    },
    StateNotSaved {
        error: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
struct ManagedPod {
    id: String,
    name: String,
    template: String,
    model: String,
    estimate: Estimate,
    state: PodState,
    started_at: DateTime<Utc>,
    #[serde(skip)]
    last_activity: Duration,
    running_cost: f64,
    stopped_cost: f64,
    stop_reason: Option<StopReason>,
    stopped_at: Option<DateTime<Utc>>,
    #[serde(skip)]
    failures: u32,
    #[serde(skip)]
    first_failure: Option<Duration>,
    /// RunPod's compute rate for the pod, when it reports one.
    remote_rate: f64,
    url: String,
    /// The key the pod's server requires; only the proxy reads it.
    pod_key: String,
}

impl ManagedPod {
    fn rate(&self) -> f64 {
        match self.state {
            PodState::Running => {
                let storage =
                    self.estimate.container_disk_per_hour + self.estimate.volume_per_hour_running;
                self.estimate.total_per_hour.max(self.remote_rate + storage)
            }
            PodState::Stopped => self.estimate.stopped_volume_per_hour,
            PodState::Terminated => 0.0,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct StartedKey {
    requester: String,
    pod_id: String,
}

/// What survives a daemon restart, in `<state dir>/cloud/runpod.json`.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    saved_at: Option<DateTime<Utc>>,
    pods: BTreeMap<String, ManagedPod>,
    started: BTreeMap<String, StartedKey>,
    /// USD spent per UTC day, `YYYY-MM-DD`.
    ledger: BTreeMap<String, f64>,
    alerted: BTreeSet<String>,
}

#[derive(Clone)]
struct Pending {
    id: String,
    requester: String,
    requested_by: RequestedBy,
    idempotency_key: String,
    template: &'static PodTemplate,
    model: String,
    estimate: Estimate,
    created: Duration,
    expires_at: DateTime<Utc>,
    presenting: bool,
}

struct Inner {
    settings: CloudSettings,
    prices: BTreeMap<String, GpuType>,
    pending: BTreeMap<String, Pending>,
    pending_keys: BTreeMap<String, String>,
    saved: Saved,
    last_accrual: Duration,
    last_tick: Option<(DateTime<Utc>, Duration)>,
}

#[derive(Clone)]
pub struct Cloud {
    client: RunPodClient,
    confirmer: Arc<dyn Confirmer>,
    clock: Arc<dyn Clock>,
    state_path: Option<PathBuf>,
    inner: Arc<Mutex<Inner>>,
    events: broadcast::Sender<CloudEvent>,
}

impl Cloud {
    /// `state_path` is where pods, spend and idempotency keys persist
    /// (`<data dir>/state/<channel>/cloud/runpod.json`); `None` keeps them in
    /// memory only. Time the daemon was down is billed to pods it knew.
    pub fn new(
        client: RunPodClient,
        settings: CloudSettings,
        confirmer: Arc<dyn Confirmer>,
        clock: Arc<dyn Clock>,
        state_path: Option<PathBuf>,
    ) -> Result<Self, CloudError> {
        let mut saved: Saved = match state_path.as_deref().map(std::fs::read) {
            Some(Ok(bytes)) => serde_json::from_slice(&bytes).map_err(|error| {
                CloudError::Internal(format!("cannot read cloud state: {error}"))
            })?,
            Some(Err(error)) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(CloudError::Internal(format!(
                    "cannot read cloud state: {error}"
                )));
            }
            _ => Saved::default(),
        };
        let now = clock.now();
        let mono = clock.monotonic();
        if let Some(saved_at) = saved.saved_at {
            let offline = (now - saved_at).num_milliseconds().max(0) as f64 / 1000.0;
            accrue_secs(&mut saved, offline, &day_key(now));
        }
        for pod in saved.pods.values_mut() {
            pod.last_activity = mono;
        }
        let inner = Inner {
            settings,
            prices: BTreeMap::new(),
            pending: BTreeMap::new(),
            pending_keys: BTreeMap::new(),
            saved,
            last_accrual: mono,
            last_tick: None,
        };
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            client,
            confirmer,
            clock,
            state_path,
            inner: Arc::new(Mutex::new(inner)),
            events,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CloudEvent> {
        self.events.subscribe()
    }

    pub fn settings(&self) -> CloudSettings {
        self.lock().settings
    }

    /// Cloud cannot be turned on until all three caps are set.
    pub fn set_settings(&self, settings: CloudSettings) -> Result<(), CloudError> {
        if settings.enabled && !settings.caps.are_set() {
            return Err(CloudError::CapsNotSet);
        }
        self.lock().settings = settings;
        Ok(())
    }

    /// Reads current GPU prices from RunPod (a read; it starts nothing).
    pub async fn refresh_prices(&self) -> Result<(), CloudError> {
        let gpus = self.client.gpu_types().await?;
        self.lock().prices = gpus.into_iter().map(|gpu| (gpu.id.clone(), gpu)).collect();
        Ok(())
    }

    pub fn templates(&self) -> Vec<TemplateView> {
        let inner = self.lock();
        templates::TEMPLATES
            .iter()
            .map(|template| {
                let mut gpus: Vec<GpuOption> = inner
                    .prices
                    .values()
                    .filter(|gpu| {
                        gpu.secure && gpu.price.secure > 0.0 && gpu.memory >= template.min_vram_gb
                    })
                    .map(|gpu| {
                        let estimate = estimate::estimate(template, gpu, 1.0);
                        GpuOption {
                            id: gpu.id.clone(),
                            name: gpu.name.clone(),
                            memory_gb: gpu.memory,
                            gpu_per_hour: estimate.gpu_per_hour,
                            total_per_hour: estimate.total_per_hour,
                            stopped_volume_per_hour: estimate.stopped_volume_per_hour,
                            availability: gpu.availability.clone(),
                        }
                    })
                    .collect();
                gpus.sort_by(|a, b| a.total_per_hour.total_cmp(&b.total_per_hour));
                TemplateView {
                    template: *template,
                    gpus,
                }
            })
            .collect()
    }

    /// The cost per hour and for `planned_hours`, from the prices already read.
    pub fn estimate(
        &self,
        template: &str,
        gpu: &str,
        planned_hours: f64,
    ) -> Result<Estimate, CloudError> {
        estimate_in(&self.lock(), template, gpu, planned_hours).map(|(_, estimate)| estimate)
    }

    /// Requests a pod. Runs the scope check, then idempotency, then the
    /// one-open-confirmation rule, then the gate, all without a network
    /// call, and returns a pending confirmation.
    pub fn request_start(
        &self,
        caller: &Caller,
        request: &StartRequest,
    ) -> Result<StartOutcome, CloudError> {
        if caller.scope != TokenScope::Owner {
            return Err(Refusal::Scope.into());
        }
        let key = request.idempotency_key.trim();
        if key.is_empty() || key.len() > 200 {
            return Err(CloudError::InvalidRequest(
                "idempotency_key must be 1 to 200 characters".to_string(),
            ));
        }
        let view = {
            let mut inner = self.lock();
            self.accrue(&mut inner);
            let now = self.clock.monotonic();
            if let Some(started) = inner.saved.started.get(key) {
                if started.requester != caller.requester {
                    return Err(CloudError::IdempotencyConflict);
                }
                if let Some(pod) = inner.saved.pods.get(&started.pod_id) {
                    return Ok(StartOutcome::Started(self.view(&inner, pod)));
                }
            }
            if let Some(id) = inner.pending_keys.get(key).cloned() {
                match inner.pending.get(&id) {
                    Some(pending) if now.saturating_sub(pending.created) < CONFIRMATION_TTL => {
                        if pending.requester != caller.requester {
                            return Err(CloudError::IdempotencyConflict);
                        }
                        return Ok(StartOutcome::Pending(
                            self.confirmation_view(&inner, pending),
                        ));
                    }
                    _ => {
                        inner.pending.remove(&id);
                        inner.pending_keys.remove(key);
                    }
                }
            }
            if inner
                .pending
                .values()
                .any(|open| now.saturating_sub(open.created) < CONFIRMATION_TTL)
            {
                return Err(CloudError::Pending);
            }
            let (template, model, estimate) =
                gate(&inner, self.clock.as_ref(), caller.scope, request)?;
            let pending = Pending {
                id: random_hex(16)?,
                requester: caller.requester.clone(),
                requested_by: caller.into(),
                idempotency_key: key.to_string(),
                template,
                model,
                estimate,
                created: now,
                expires_at: self.clock.now()
                    + chrono::Duration::from_std(CONFIRMATION_TTL).unwrap_or_default(),
                presenting: false,
            };
            let view = self.confirmation_view(&inner, &pending);
            inner
                .pending_keys
                .insert(key.to_string(), pending.id.clone());
            inner.pending.insert(pending.id.clone(), pending);
            view
        };
        self.emit(CloudEvent::ConfirmationRequested {
            confirmation: view.confirmation.clone(),
        });
        Ok(StartOutcome::Pending(view))
    }

    /// Shows a pending confirmation through the [`Confirmer`] and, on
    /// approval, re-runs the whole gate and creates the pod. The daemon calls
    /// this right after answering 202; it never starts a pod without the
    /// Confirmer's approval, so no route needs to expose it.
    pub async fn present_confirmation(&self, id: &str) -> Result<PodView, CloudError> {
        let view = {
            let mut inner = self.lock();
            self.accrue(&mut inner);
            let now = self.clock.monotonic();
            let Some(pending) = inner.pending.get_mut(id) else {
                return Err(CloudError::UnknownConfirmation);
            };
            if now.saturating_sub(pending.created) >= CONFIRMATION_TTL {
                let key = pending.idempotency_key.clone();
                inner.pending.remove(id);
                inner.pending_keys.remove(&key);
                return Err(CloudError::ConfirmationExpired);
            }
            if pending.presenting {
                return Err(CloudError::InvalidRequest(
                    "this confirmation is already on screen".to_string(),
                ));
            }
            pending.presenting = true;
            let pending = pending.clone();
            self.confirmation_view(&inner, &pending)
        };
        let decision = self.confirmer.ask(&Prompt::Start(view)).await;
        let pending = {
            let mut inner = self.lock();
            let pending = inner
                .pending
                .remove(id)
                .ok_or(CloudError::UnknownConfirmation)?;
            inner.pending_keys.remove(&pending.idempotency_key);
            pending
        };
        if decision != Decision::Approved {
            return Err(CloudError::Declined);
        }
        if self.clock.monotonic().saturating_sub(pending.created) >= CONFIRMATION_TTL {
            return Err(CloudError::ConfirmationExpired);
        }
        // The dialog may have been open for minutes: read prices again (a
        // read; nothing starts) and run the whole gate before creating.
        self.refresh_prices().await?;
        let request = StartRequest {
            template: pending.template.id.to_string(),
            gpu: pending.estimate.gpu.clone(),
            model: Some(pending.model.clone()),
            planned_hours: pending.estimate.planned_hours,
            confirm_cost_per_hour: pending.estimate.total_per_hour,
            idempotency_key: pending.idempotency_key.clone(),
        };
        let (template, model, estimate) = {
            let mut inner = self.lock();
            self.accrue(&mut inner);
            gate(&inner, self.clock.as_ref(), TokenScope::Owner, &request)?
        };
        let pod_key = random_hex(32)?;
        let name = format!("{POD_PREFIX}{}", &pending.id[..8]);
        let body = template.create_request(&name, &estimate.gpu, &model, &pod_key);
        let pod = self.client.create_pod(&body).await?;
        let managed = ManagedPod {
            id: pod.id.clone(),
            name: pod.name.clone(),
            template: template.id.to_string(),
            model,
            url: self.client.proxy_url(&pod.id, template.port),
            remote_rate: pod.cost,
            estimate,
            state: PodState::Running,
            started_at: self.clock.now(),
            last_activity: self.clock.monotonic(),
            running_cost: 0.0,
            stopped_cost: 0.0,
            stop_reason: None,
            stopped_at: None,
            failures: 0,
            first_failure: None,
            pod_key,
        };
        let cost_per_hour = round4(managed.rate());
        let view = {
            let mut inner = self.lock();
            inner.saved.started.insert(
                pending.idempotency_key.clone(),
                StartedKey {
                    requester: pending.requester.clone(),
                    pod_id: pod.id.clone(),
                },
            );
            let view = self.view(&inner, &managed);
            inner.saved.pods.insert(pod.id.clone(), managed);
            self.save_or_warn(&mut inner);
            view
        };
        self.emit(CloudEvent::Started {
            pod: pod.id,
            cost_per_hour,
        });
        Ok(view)
    }

    pub fn pods(&self) -> Vec<PodView> {
        let mut inner = self.lock();
        self.accrue(&mut inner);
        inner
            .saved
            .pods
            .values()
            .map(|pod| self.view(&inner, pod))
            .collect()
    }

    /// Marks traffic to a pod, which resets its idle timer. The daemon's
    /// `/v1` proxy calls this for every request it forwards.
    pub fn record_request(&self, pod_id: &str) -> bool {
        let now = self.clock.monotonic();
        let mut inner = self.lock();
        match inner.saved.pods.get_mut(pod_id) {
            Some(pod) if pod.state == PodState::Running => {
                pod.last_activity = now;
                true
            }
            _ => false,
        }
    }

    /// Where to send a running pod's `/v1` traffic.
    pub fn upstream(&self, pod_id: &str) -> Option<Upstream> {
        let inner = self.lock();
        let pod = inner
            .saved
            .pods
            .get(pod_id)
            .filter(|pod| pod.state == PodState::Running)?;
        Some(Upstream {
            base_url: pod.url.clone(),
            api_key: SecretString::from(pod.pod_key.clone()),
            model: pod.model.clone(),
        })
    }

    /// Running pods as `/v1/models` entries. The daemon leaves them out for
    /// plugin keys.
    pub fn cloud_models(&self) -> Vec<CloudModel> {
        let mut inner = self.lock();
        self.accrue(&mut inner);
        inner
            .saved
            .pods
            .values()
            .filter(|pod| pod.state == PodState::Running)
            .map(|pod| CloudModel {
                id: format!("{CLOUD_MODEL_PREFIX}{}", pod.id),
                object: "model",
                owned_by: "runpod",
                location: "cloud",
                model: pod.model.clone(),
                pod: pod.id.clone(),
                cost_per_hour: round4(pod.rate()),
            })
            .collect()
    }

    /// What the daemon's `/v1` proxy calls for a `cloud:` model: runs
    /// [`Cloud::route_gate`], counts the request as activity for idle
    /// auto-stop, and returns where to send it. Never starts anything.
    pub fn route(
        &self,
        model_id: &str,
        key_profile: Profile,
        key_has_cloud: bool,
    ) -> Result<Upstream, CloudError> {
        let pod_id = model_id
            .strip_prefix(CLOUD_MODEL_PREFIX)
            .ok_or_else(|| CloudError::NotFound(model_id.to_string()))?;
        self.route_gate(key_profile, key_has_cloud)?;
        let upstream = self
            .upstream(pod_id)
            .ok_or_else(|| CloudError::NotFound(pod_id.to_string()))?;
        self.record_request(pod_id);
        Ok(upstream)
    }

    /// The `/v1` gate for a cloud model: refused for a plugin key whatever
    /// the settings, then when cloud is off, the key's profile is not
    /// personal, the key has no cloud permission, or a day or month cap is
    /// reached.
    pub fn route_gate(&self, key_profile: Profile, key_has_cloud: bool) -> Result<(), Refusal> {
        if key_profile == Profile::Plugin {
            return Err(Refusal::CloudBlocked);
        }
        let mut inner = self.lock();
        self.accrue(&mut inner);
        if !inner.settings.enabled {
            return Err(Refusal::CloudOff);
        }
        if key_profile != Profile::Personal {
            return Err(Refusal::CloudBlocked);
        }
        if !key_has_cloud {
            return Err(Refusal::Scope);
        }
        let now = self.clock.now();
        let caps = inner.settings.caps;
        if spent_day(&inner.saved, now) >= caps.day || spent_month(&inner.saved, now) >= caps.month
        {
            return Err(Refusal::BudgetCap);
        }
        Ok(())
    }

    /// Stops a pod. Any token scope may stop. The volume keeps billing while
    /// stopped, and the report says how much.
    pub async fn stop(&self, pod_id: &str) -> Result<StopReport, CloudError> {
        let state = self.lock().saved.pods.get(pod_id).map(|pod| pod.state);
        match state {
            None => Err(CloudError::NotFound(pod_id.to_string())),
            Some(PodState::Running) => {
                let report = self.stop_with(pod_id, StopReason::User).await?;
                self.emit(stopped_event(&report, StopReason::User));
                Ok(report)
            }
            Some(_) => {
                let inner = self.lock();
                let pod = &inner.saved.pods[pod_id];
                Ok(stop_report(self.view(&inner, pod), &pod.estimate))
            }
        }
    }

    /// Terminates a pod: all billing ends and its volume is deleted. Owner
    /// only, and only after the person approves the same native dialog and
    /// presence check as a start.
    pub async fn terminate(&self, caller: &Caller, pod_id: &str) -> Result<PodView, CloudError> {
        if caller.scope != TokenScope::Owner {
            return Err(Refusal::Scope.into());
        }
        let prompt = {
            let mut inner = self.lock();
            self.accrue(&mut inner);
            let Some(pod) = inner.saved.pods.get(pod_id) else {
                return Err(CloudError::NotFound(pod_id.to_string()));
            };
            Prompt::Terminate(TerminateView {
                requested_by: caller.into(),
                pod: self.view(&inner, pod),
                volume_gb: pod.estimate.volume_gb,
                note: format!(
                    "Terminating ends all billing and deletes the {} GB volume and the model on it. It cannot be undone.",
                    pod.estimate.volume_gb
                ),
            })
        };
        if self.confirmer.ask(&prompt).await != Decision::Approved {
            return Err(CloudError::Declined);
        }
        self.client.terminate_pod(pod_id).await?;
        let view = {
            let mut inner = self.lock();
            self.accrue(&mut inner);
            let now = self.clock.now();
            let pod = inner
                .saved
                .pods
                .get_mut(pod_id)
                .ok_or_else(|| CloudError::NotFound(pod_id.to_string()))?;
            pod.state = PodState::Terminated;
            pod.stopped_at.get_or_insert(now);
            let pod = pod.clone();
            self.save_or_warn(&mut inner);
            self.view(&inner, &pod)
        };
        self.emit(CloudEvent::Terminated {
            pod: pod_id.to_string(),
        });
        Ok(view)
    }

    /// One supervisor pass: bill the time since the last pass, then stop
    /// pods that went idle, reached a cap, or that Fonsviva lost sight of.
    /// The daemon runs it every few seconds ([`Cloud::spawn_supervisor`]).
    pub async fn tick(&self) -> Vec<CloudEvent> {
        let mut events = Vec::new();
        let (running, jumped, dead_man) = {
            let mut inner = self.lock();
            self.accrue(&mut inner);
            let now = self.clock.now();
            let mono = self.clock.monotonic();
            let expired: Vec<(String, String)> = inner
                .pending
                .values()
                .filter(|pending| {
                    !pending.presenting && mono.saturating_sub(pending.created) >= CONFIRMATION_TTL
                })
                .map(|pending| (pending.id.clone(), pending.idempotency_key.clone()))
                .collect();
            for (id, key) in expired {
                inner.pending.remove(&id);
                inner.pending_keys.remove(&key);
            }
            let jumped = inner.last_tick.is_some_and(|(wall, last)| {
                let wall_secs = (now - wall).num_milliseconds() as f64 / 1000.0;
                let mono_secs = mono.saturating_sub(last).as_secs_f64();
                (wall_secs - mono_secs).abs() > CLOCK_JUMP_TOLERANCE.as_secs_f64()
            });
            inner.last_tick = Some((now, mono));
            (running_ids(&inner), jumped, inner.settings.dead_man)
        };

        if jumped {
            for id in &running {
                events.push(self.stop_event(id, StopReason::ClockJump).await);
            }
        } else {
            for id in &running {
                match self.client.get_pod(id).await {
                    Ok(remote) => {
                        let mut failed = false;
                        {
                            let mut inner = self.lock();
                            let now = self.clock.now();
                            if let Some(pod) = inner.saved.pods.get_mut(id) {
                                pod.failures = 0;
                                pod.first_failure = None;
                                pod.remote_rate = remote.cost;
                                match remote.status {
                                    PodStatus::Exited | PodStatus::Terminated => {
                                        pod.state = if remote.status == PodStatus::Terminated {
                                            PodState::Terminated
                                        } else {
                                            PodState::Stopped
                                        };
                                        pod.stop_reason = Some(StopReason::Exited);
                                        pod.stopped_at = Some(now);
                                        events.push(CloudEvent::Stopped {
                                            pod: id.clone(),
                                            reason: StopReason::Exited,
                                            stopped_volume_per_hour: pod
                                                .estimate
                                                .stopped_volume_per_hour,
                                        });
                                    }
                                    PodStatus::Error => failed = true,
                                    _ => {}
                                }
                            }
                        }
                        if failed {
                            events.push(self.stop_event(id, StopReason::Exited).await);
                        }
                    }
                    Err(_) => {
                        let mono = self.clock.monotonic();
                        let (failures, dead) = {
                            let mut inner = self.lock();
                            match inner.saved.pods.get_mut(id) {
                                Some(pod) => {
                                    pod.failures += 1;
                                    let first = *pod.first_failure.get_or_insert(mono);
                                    let dead = pod.failures >= dead_man.max_failures
                                        && mono.saturating_sub(first)
                                            >= Duration::from_secs(dead_man.after_secs);
                                    (pod.failures, dead)
                                }
                                None => (0, false),
                            }
                        };
                        events.push(CloudEvent::Unreachable {
                            pod: id.clone(),
                            failures,
                        });
                        if dead {
                            events.push(self.stop_event(id, StopReason::DeadMan).await);
                        }
                    }
                }
            }
        }

        let (budget_stops, idle_stops) = {
            let mut inner = self.lock();
            self.accrue(&mut inner);
            let now = self.clock.now();
            let mono = self.clock.monotonic();
            let caps = inner.settings.caps;
            let period_capped = spent_day(&inner.saved, now) >= caps.day
                || spent_month(&inner.saved, now) >= caps.month;
            let idle_limit = Duration::from_secs(u64::from(inner.settings.idle_minutes) * 60);
            let mut budget = Vec::new();
            let mut idle = Vec::new();
            for pod in inner
                .saved
                .pods
                .values()
                .filter(|pod| pod.state == PodState::Running)
            {
                if period_capped || pod.running_cost >= caps.session {
                    budget.push(pod.id.clone());
                } else if mono.saturating_sub(pod.last_activity) >= idle_limit {
                    idle.push(pod.id.clone());
                }
            }
            (budget, idle)
        };
        for id in &budget_stops {
            events.push(self.stop_event(id, StopReason::BudgetCap).await);
        }
        for id in &idle_stops {
            events.push(self.stop_event(id, StopReason::Idle).await);
        }

        {
            let mut inner = self.lock();
            self.accrue(&mut inner);
            events.extend(self.budget_alerts(&mut inner));
            self.save_or_warn(&mut inner);
        }
        for event in &events {
            self.emit(event.clone());
        }
        events
    }

    /// Stops pods named `fonsviva-*` that RunPod runs but Fonsviva does not
    /// track (a start whose answer was lost, or state from another machine),
    /// and marks tracked pods RunPod no longer lists as terminated.
    pub async fn reconcile(&self) -> Result<Vec<CloudEvent>, CloudError> {
        let remote = self.client.list_pods().await?;
        let mut events = Vec::new();
        let known: BTreeMap<String, PodState> = {
            let inner = self.lock();
            inner
                .saved
                .pods
                .iter()
                .map(|(id, pod)| (id.clone(), pod.state))
                .collect()
        };
        for pod in &remote {
            let live = matches!(
                pod.status,
                PodStatus::Running | PodStatus::Starting | PodStatus::Provisioning
            );
            let ours_running = known.get(&pod.id) == Some(&PodState::Running);
            if pod.name.starts_with(POD_PREFIX) && live && !ours_running {
                match self.client.stop_pod(&pod.id).await {
                    Ok(()) => events.push(CloudEvent::Stopped {
                        pod: pod.id.clone(),
                        reason: StopReason::Orphan,
                        stopped_volume_per_hour: 0.0,
                    }),
                    Err(error) => events.push(CloudEvent::StopFailed {
                        pod: pod.id.clone(),
                        reason: StopReason::Orphan,
                        error: error.to_string(),
                    }),
                }
            }
        }
        {
            let mut inner = self.lock();
            self.accrue(&mut inner);
            for pod in inner.saved.pods.values_mut() {
                if pod.state != PodState::Terminated
                    && !remote.iter().any(|remote| remote.id == pod.id)
                {
                    pod.state = PodState::Terminated;
                    events.push(CloudEvent::Terminated {
                        pod: pod.id.clone(),
                    });
                }
            }
            self.save_or_warn(&mut inner);
        }
        for event in &events {
            self.emit(event.clone());
        }
        Ok(events)
    }

    /// The emergency stop: turns cloud off and stops every running pod.
    pub async fn stop_all(&self) -> Vec<CloudEvent> {
        let running = {
            let mut inner = self.lock();
            inner.settings.enabled = false;
            running_ids(&inner)
        };
        let mut events = Vec::new();
        for id in &running {
            events.push(self.stop_event(id, StopReason::EmergencyStop).await);
        }
        for event in &events {
            self.emit(event.clone());
        }
        events
    }

    /// Runs [`Cloud::tick`] every `every` until the returned task is aborted.
    pub fn spawn_supervisor(&self, every: Duration) -> tokio::task::JoinHandle<()> {
        let cloud = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(every);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                cloud.tick().await;
            }
        })
    }

    async fn stop_event(&self, id: &str, reason: StopReason) -> CloudEvent {
        match self.stop_with(id, reason).await {
            Ok(report) => stopped_event(&report, reason),
            Err(error) => CloudEvent::StopFailed {
                pod: id.to_string(),
                reason,
                error: error.to_string(),
            },
        }
    }

    async fn stop_with(&self, id: &str, reason: StopReason) -> Result<StopReport, CloudError> {
        self.client.stop_pod(id).await?;
        let mut inner = self.lock();
        self.accrue(&mut inner);
        let now = self.clock.now();
        let pod = inner
            .saved
            .pods
            .get_mut(id)
            .ok_or_else(|| CloudError::NotFound(id.to_string()))?;
        if pod.state == PodState::Running {
            pod.state = PodState::Stopped;
            pod.stop_reason = Some(reason);
            pod.stopped_at = Some(now);
        }
        let pod = pod.clone();
        self.save_or_warn(&mut inner);
        Ok(stop_report(self.view(&inner, &pod), &pod.estimate))
    }

    fn budget_alerts(&self, inner: &mut Inner) -> Vec<CloudEvent> {
        let now = self.clock.now();
        let caps = inner.settings.caps;
        let day = day_key(now);
        let month = day[..7].to_string();
        let mut checks = vec![
            (
                CapKind::Day,
                format!("day:{day}"),
                spent_day(&inner.saved, now),
                caps.day,
            ),
            (
                CapKind::Month,
                format!("month:{month}"),
                spent_month(&inner.saved, now),
                caps.month,
            ),
        ];
        for pod in inner
            .saved
            .pods
            .values()
            .filter(|pod| pod.state == PodState::Running)
        {
            checks.push((
                CapKind::Session,
                format!("session:{}", pod.id),
                pod.running_cost,
                caps.session,
            ));
        }
        let nothing_running = !inner
            .saved
            .pods
            .values()
            .any(|pod| pod.state == PodState::Running);
        let stopped: Vec<(String, f64)> = inner
            .saved
            .pods
            .values()
            .filter(|pod| pod.state == PodState::Stopped)
            .map(|pod| (pod.id.clone(), pod.rate()))
            .collect();
        let mut events = Vec::new();
        for (cap, key, spent, limit) in checks {
            if limit <= 0.0 {
                continue;
            }
            for percent in ALERT_PERCENTS {
                if spent >= limit * f64::from(percent) / 100.0
                    && inner.saved.alerted.insert(format!("{key}:{percent}"))
                {
                    events.push(CloudEvent::BudgetAlert {
                        cap,
                        percent,
                        spent: round4(spent),
                        limit,
                    });
                }
            }
            if cap != CapKind::Session
                && spent >= limit
                && nothing_running
                && !stopped.is_empty()
                && inner.saved.alerted.insert(format!("volume:{key}"))
            {
                events.push(CloudEvent::VolumeCapReached {
                    cap,
                    pods: stopped.iter().map(|(id, _)| id.clone()).collect(),
                    stopped_volume_per_hour: round4(stopped.iter().map(|(_, rate)| rate).sum()),
                    offer: "terminate".to_string(),
                });
            }
        }
        events
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn accrue(&self, inner: &mut Inner) {
        let mono = self.clock.monotonic();
        let secs = mono.saturating_sub(inner.last_accrual).as_secs_f64();
        inner.last_accrual = mono;
        accrue_secs(&mut inner.saved, secs, &day_key(self.clock.now()));
    }

    fn view(&self, inner: &Inner, pod: &ManagedPod) -> PodView {
        let idle = self.clock.monotonic().saturating_sub(pod.last_activity);
        let limit = Duration::from_secs(u64::from(inner.settings.idle_minutes) * 60);
        let auto_stop_at = (pod.state == PodState::Running).then(|| {
            self.clock.now()
                + chrono::Duration::from_std(limit.saturating_sub(idle)).unwrap_or_default()
        });
        PodView {
            id: pod.id.clone(),
            name: pod.name.clone(),
            template: pod.template.clone(),
            model: pod.model.clone(),
            gpu: pod.estimate.gpu.clone(),
            gpu_name: pod.estimate.gpu_name.clone(),
            state: pod.state,
            cost_per_hour: round4(pod.rate()),
            running_cost_so_far: round4(pod.running_cost),
            stopped_cost_so_far: round4(pod.stopped_cost),
            stopped_volume_per_hour: pod.estimate.stopped_volume_per_hour,
            started_at: pod.started_at,
            idle_minutes: (idle.as_secs_f64() / 6.0).round() / 10.0,
            auto_stop_at,
            stop_reason: pod.stop_reason,
            url: pod.url.clone(),
        }
    }

    fn confirmation_view(&self, inner: &Inner, pending: &Pending) -> ConfirmationView {
        let now = self.clock.now();
        ConfirmationView {
            confirmation: pending.id.clone(),
            requested_by: pending.requested_by.clone(),
            template: pending.template.id.to_string(),
            model: pending.model.clone(),
            estimate: pending.estimate.clone(),
            caps: inner.settings.caps,
            spent_today: round4(spent_day(&inner.saved, now)),
            spent_month: round4(spent_month(&inner.saved, now)),
            expires_at: pending.expires_at,
        }
    }

    fn save_or_warn(&self, inner: &mut Inner) {
        if let Err(error) = self.save(inner) {
            self.emit(CloudEvent::StateNotSaved { error });
        }
    }

    fn save(&self, inner: &mut Inner) -> Result<(), String> {
        let Some(path) = &self.state_path else {
            return Ok(());
        };
        inner.saved.saved_at = Some(self.clock.now());
        let bytes = serde_json::to_vec_pretty(&inner.saved).map_err(|error| error.to_string())?;
        write_private(path, &bytes).map_err(|error| format!("{}: {error}", path.display()))
    }

    fn emit(&self, event: CloudEvent) {
        let _ = self.events.send(event);
    }
}

/// The gate, in the order docs/API.md lists it. Reads only what is in
/// memory, so a refusal never costs a network call.
fn gate(
    inner: &Inner,
    clock: &dyn Clock,
    scope: TokenScope,
    request: &StartRequest,
) -> Result<(&'static PodTemplate, String, Estimate), CloudError> {
    if scope != TokenScope::Owner {
        return Err(Refusal::Scope.into());
    }
    if !inner.settings.enabled {
        return Err(Refusal::CloudOff.into());
    }
    if inner.settings.profile != Profile::Personal {
        return Err(Refusal::CloudBlocked.into());
    }
    let (template, estimate) = estimate_in(
        inner,
        &request.template,
        &request.gpu,
        request.planned_hours,
    )?;
    let model = request
        .model
        .clone()
        .unwrap_or_else(|| template.default_model.to_string());
    if !templates::valid_model(&model) {
        return Err(CloudError::InvalidRequest(format!(
            "\"{model}\" is not a Hugging Face model id like owner/name or owner/name:tag"
        )));
    }
    let caps = inner.settings.caps;
    let now = clock.now();
    // Every pod Fonsviva still pays for, running or stopped, keeps billing
    // for the planned hours too.
    let others_per_hour: f64 = inner.saved.pods.values().map(ManagedPod::rate).sum();
    let projected = (estimate.total_per_hour + others_per_hour) * estimate.planned_hours;
    if !caps.are_set()
        || estimate.planned_total > caps.session
        || spent_day(&inner.saved, now) + projected > caps.day
        || spent_month(&inner.saved, now) + projected > caps.month
    {
        return Err(Refusal::BudgetCap.into());
    }
    if round4(request.confirm_cost_per_hour) != estimate.total_per_hour {
        return Err(Refusal::PriceChanged.into());
    }
    Ok((template, model, estimate))
}

fn estimate_in(
    inner: &Inner,
    template: &str,
    gpu: &str,
    planned_hours: f64,
) -> Result<(&'static PodTemplate, Estimate), CloudError> {
    let template = templates::template(template)
        .ok_or_else(|| CloudError::UnknownTemplate(template.to_string()))?;
    if !(planned_hours.is_finite() && planned_hours > 0.0 && planned_hours <= 24.0 * 31.0) {
        return Err(CloudError::InvalidRequest(
            "planned_hours must be more than 0 and at most 744".to_string(),
        ));
    }
    if inner.prices.is_empty() {
        return Err(CloudError::PricesUnavailable);
    }
    let found = inner
        .prices
        .get(gpu)
        .ok_or_else(|| CloudError::UnknownGpu(format!("RunPod has no GPU type \"{gpu}\"")))?;
    if !found.secure || found.price.secure <= 0.0 {
        return Err(CloudError::UnknownGpu(format!(
            "{} is not offered in Secure Cloud",
            found.name
        )));
    }
    if found.memory < template.min_vram_gb {
        return Err(CloudError::UnknownGpu(format!(
            "{} has {} GB; {} needs at least {} GB",
            found.name, found.memory, template.name, template.min_vram_gb
        )));
    }
    Ok((template, estimate::estimate(template, found, planned_hours)))
}

fn accrue_secs(saved: &mut Saved, secs: f64, day: &str) {
    if secs <= 0.0 {
        return;
    }
    let mut total = 0.0;
    for pod in saved.pods.values_mut() {
        let cost = pod.rate() * secs / 3600.0;
        match pod.state {
            PodState::Running => pod.running_cost += cost,
            PodState::Stopped => pod.stopped_cost += cost,
            PodState::Terminated => {}
        }
        total += cost;
    }
    if total > 0.0 {
        *saved.ledger.entry(day.to_string()).or_default() += total;
    }
}

fn running_ids(inner: &Inner) -> Vec<String> {
    inner
        .saved
        .pods
        .values()
        .filter(|pod| pod.state == PodState::Running)
        .map(|pod| pod.id.clone())
        .collect()
}

fn day_key(now: DateTime<Utc>) -> String {
    now.format("%Y-%m-%d").to_string()
}

fn spent_day(saved: &Saved, now: DateTime<Utc>) -> f64 {
    saved.ledger.get(&day_key(now)).copied().unwrap_or(0.0)
}

fn spent_month(saved: &Saved, now: DateTime<Utc>) -> f64 {
    let month = now.format("%Y-%m-").to_string();
    saved
        .ledger
        .iter()
        .filter(|(day, _)| day.starts_with(&month))
        .map(|(_, spent)| spent)
        .sum()
}

fn stop_report(pod: PodView, estimate: &Estimate) -> StopReport {
    let note = format!(
        "Stopped. The {} GB volume still bills ${:.4} an hour (${:.2} a month) until you terminate the pod.",
        estimate.volume_gb, estimate.stopped_volume_per_hour, estimate.stopped_volume_per_month
    );
    StopReport {
        pod,
        stopped_volume_per_hour: estimate.stopped_volume_per_hour,
        stopped_volume_per_month: estimate.stopped_volume_per_month,
        note,
    }
}

fn stopped_event(report: &StopReport, reason: StopReason) -> CloudEvent {
    CloudEvent::Stopped {
        pod: report.pod.id.clone(),
        reason,
        stopped_volume_per_hour: report.stopped_volume_per_hour,
    }
}

fn random_hex(bytes: usize) -> Result<String, CloudError> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer)
        .map_err(|error| CloudError::Internal(format!("no OS random source: {error}")))?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Writes `bytes` atomically with mode 0600 (Unix).
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)
}
