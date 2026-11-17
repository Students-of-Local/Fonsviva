//! RunPod for Fonsviva, with the person's own key.
//!
//! - [`RunPodClient`]: RunPod's REST API v2 (pods, GPU types and prices,
//!   templates). The key comes from the OS keychain ([`ApiKey`]) and is never
//!   logged, printed or written in plain text.
//! - [`templates`]: the pod templates Fonsviva supports (llama.cpp and vLLM
//!   OpenAI-compatible servers).
//! - [`estimate()`]: per-hour and planned cost, including container disk and
//!   volume, shown before a start.
//! - [`Cloud`]: the cloud gate, start requests confirmed through a
//!   [`Confirmer`], stop and terminate, and the supervisor (per-second cost,
//!   idle auto-stop, budget caps, the dead-man rule).
//! - [`mock::MockRunPod`]: a mock RunPod server for tests and demo mode.

mod client;
mod clock;
mod cloud;
mod estimate;
mod gate;
mod key;
pub mod mock;
pub mod templates;

pub use client::{
    ClientError, CreatePod, DEFAULT_BASE_URL, GpuPrice, GpuType, MaxCount, Mounts, PersistentMount,
    Pod, PodGpu, PodStatus, RunPodClient, Template,
};
pub use clock::{Clock, FakeClock, SystemClock};
pub use cloud::{
    CLOCK_JUMP_TOLERANCE, CLOUD_MODEL_PREFIX, CONFIRMATION_TTL, Cloud, CloudError, CloudEvent,
    CloudModel, ConfirmationView, Confirmer, Decision, DecisionFuture, GpuOption, POD_PREFIX,
    PodState, PodView, Prompt, RequestedBy, StartOutcome, StartRequest, StopReason, StopReport,
    TemplateView, TerminateView, Upstream,
};
pub use estimate::{
    CONTAINER_DISK_PER_GB_MONTH, Estimate, HOURS_PER_MONTH, VOLUME_RUNNING_PER_GB_MONTH,
    VOLUME_STOPPED_PER_GB_MONTH, estimate, round2, round4,
};
pub use gate::{
    Caller, CapKind, Caps, CloudSettings, DEFAULT_IDLE_MINUTES, DeadMan, Profile, Refusal,
    TokenScope,
};
pub use key::{ApiKey, KEY_ACCOUNT, KEY_SERVICE};
