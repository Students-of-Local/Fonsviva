//! Every test runs against the in-process mock RunPod server with a fake
//! clock and a fake confirmation dialog. Nothing here reaches RunPod.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use fonsviva_runpod::mock::MockRunPod;
use fonsviva_runpod::{
    ApiKey, Caller, CapKind, Caps, Cloud, CloudError, CloudEvent, CloudSettings, ConfirmationView,
    Confirmer, DeadMan, Decision, DecisionFuture, FakeClock, PodState, PodStatus, Profile, Prompt,
    Refusal, RunPodClient, StartOutcome, StartRequest, StopReason, TokenScope,
};
use secrecy::{ExposeSecret, SecretString};

const MOCK_CREDENTIAL: &str = "only-the-mock-accepts-this";
const GPU: &str = "NVIDIA GeForce RTX 4090";

/// Stands in for the native dialog: answers from a script and takes
/// `think` of fake time to do it.
struct FakeConfirmer {
    clock: Arc<FakeClock>,
    answers: Mutex<VecDeque<Decision>>,
    think: Mutex<Duration>,
    asked: AtomicUsize,
    prompts: Mutex<Vec<Prompt>>,
}

impl Confirmer for FakeConfirmer {
    fn ask<'a>(&'a self, prompt: &'a Prompt) -> DecisionFuture<'a> {
        Box::pin(async move {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.prompts.lock().unwrap().push(prompt.clone());
            self.clock.advance(*self.think.lock().unwrap());
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Decision::Approved)
        })
    }
}

struct Rig {
    mock: MockRunPod,
    clock: Arc<FakeClock>,
    confirmer: Arc<FakeConfirmer>,
    cloud: Cloud,
    state_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn settings() -> CloudSettings {
    CloudSettings {
        enabled: true,
        profile: Profile::Personal,
        caps: Caps {
            session: 10.0,
            day: 20.0,
            month: 100.0,
        },
        idle_minutes: 15,
        dead_man: DeadMan::default(),
    }
}

fn owner() -> Caller {
    Caller::new(TokenScope::Owner, "owner-token-1").with_label("Fonsviva desktop")
}

fn close(actual: f64, expected: f64, within: f64) -> bool {
    (actual - expected).abs() <= within
}

impl Rig {
    async fn new() -> Self {
        Self::with(settings()).await
    }

    async fn with(settings: CloudSettings) -> Self {
        let mock = MockRunPod::start(MOCK_CREDENTIAL).await.unwrap();
        let clock = Arc::new(FakeClock::new(
            Utc.with_ymd_and_hms(2026, 11, 17, 12, 0, 0).unwrap(),
        ));
        let confirmer = Arc::new(FakeConfirmer {
            clock: clock.clone(),
            answers: Mutex::new(VecDeque::new()),
            think: Mutex::new(Duration::from_secs(5)),
            asked: AtomicUsize::new(0),
            prompts: Mutex::new(Vec::new()),
        });
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir
            .path()
            .join("state")
            .join("stable")
            .join("cloud")
            .join("runpod.json");
        let cloud = Self::cloud(&mock, &clock, &confirmer, &state_path, settings);
        cloud.refresh_prices().await.unwrap();
        Self {
            mock,
            clock,
            confirmer,
            cloud,
            state_path,
            _dir: dir,
        }
    }

    fn cloud(
        mock: &MockRunPod,
        clock: &Arc<FakeClock>,
        confirmer: &Arc<FakeConfirmer>,
        path: &std::path::Path,
        settings: CloudSettings,
    ) -> Cloud {
        let client = RunPodClient::with_base_url(
            mock.url(),
            ApiKey::new(SecretString::from(MOCK_CREDENTIAL)),
        );
        Cloud::new(
            client,
            settings,
            confirmer.clone(),
            clock.clone(),
            Some(path.to_path_buf()),
        )
        .unwrap()
    }

    /// Moves Fonsviva's clock and the mock's billing clock together.
    fn advance(&self, seconds: u64) {
        self.clock.advance(Duration::from_secs(seconds));
        self.mock.advance(seconds as f64);
    }

    fn request(&self, hours: f64, key: &str) -> StartRequest {
        let estimate = self.cloud.estimate("llama-cpp", GPU, hours).unwrap();
        StartRequest {
            template: "llama-cpp".into(),
            gpu: GPU.into(),
            model: None,
            planned_hours: hours,
            confirm_cost_per_hour: estimate.total_per_hour,
            idempotency_key: key.into(),
        }
    }

    fn pending(&self, request: &StartRequest) -> ConfirmationView {
        match self.cloud.request_start(&owner(), request).unwrap() {
            StartOutcome::Pending(view) => view,
            other => panic!("expected a pending confirmation, got {other:?}"),
        }
    }

    async fn start(&self, hours: f64) -> String {
        let view = self.pending(&self.request(hours, "start-1"));
        self.cloud
            .present_confirmation(&view.confirmation)
            .await
            .unwrap()
            .id
    }

    fn asked(&self) -> usize {
        self.confirmer.asked.load(Ordering::SeqCst)
    }
}

fn stopped_for(events: &[CloudEvent], reason: StopReason) -> bool {
    events
        .iter()
        .any(|event| matches!(event, CloudEvent::Stopped { reason: found, .. } if *found == reason))
}

#[tokio::test]
async fn estimate_shows_gpu_disk_and_stopped_volume_cost() {
    let rig = Rig::new().await;
    let estimate = rig.cloud.estimate("llama-cpp", GPU, 3.0).unwrap();
    assert_eq!(estimate.gpu_per_hour, 0.89);
    assert_eq!(estimate.container_disk_per_hour, 0.0028);
    assert_eq!(estimate.volume_per_hour_running, 0.0069);
    assert_eq!(estimate.total_per_hour, 0.8997);
    assert_eq!(estimate.planned_total, 2.6991);
    assert_eq!(estimate.stopped_volume_per_hour, 0.0139);
    assert_eq!(estimate.stopped_volume_per_month, 10.0);

    let templates = rig.cloud.templates();
    let llama = templates
        .iter()
        .find(|view| view.template.id == "llama-cpp")
        .unwrap();
    assert_eq!(
        llama.gpus.first().map(|gpu| gpu.id.as_str()),
        Some("NVIDIA RTX A5000"),
        "cheapest first"
    );
    let vllm = templates
        .iter()
        .find(|view| view.template.id == "vllm")
        .unwrap();
    assert!(vllm.gpus.iter().all(|gpu| gpu.memory_gb >= 48));
    let error = rig.cloud.estimate("vllm", GPU, 1.0).unwrap_err();
    assert_eq!(
        error.code(),
        "unknown_gpu",
        "a 24 GB card is too small for the vLLM template"
    );
}

#[tokio::test]
async fn start_without_matching_confirm_is_refused() {
    let rig = Rig::new().await;
    let mut request = rig.request(2.0, "price-1");
    request.confirm_cost_per_hour = 0.5;
    let error = rig.cloud.request_start(&owner(), &request).unwrap_err();
    assert_eq!(error.refusal(), Some(Refusal::PriceChanged));
    assert_eq!(error.code(), "price_changed");
    assert!(rig.mock.pod_ids().is_empty());
    assert_eq!(rig.asked(), 0);
}

#[tokio::test]
async fn start_is_a_request_until_the_dialog_approves() {
    let rig = Rig::new().await;
    let before = rig.mock.request_count();
    let request = rig.request(2.0, "start-1");
    let view = rig.pending(&request);
    assert_eq!(view.estimate.total_per_hour, 0.8997);
    assert_eq!(view.estimate.stopped_volume_per_hour, 0.0139);
    assert_eq!(
        rig.mock.request_count(),
        before,
        "a request sends nothing to RunPod"
    );
    assert!(rig.mock.pod_ids().is_empty());

    let replay = rig.cloud.request_start(&owner(), &request).unwrap();
    assert_eq!(
        replay,
        StartOutcome::Pending(view.clone()),
        "same key, same token: the same confirmation"
    );
    let other = Caller::new(TokenScope::Owner, "owner-token-2");
    assert!(matches!(
        rig.cloud.request_start(&other, &request),
        Err(CloudError::IdempotencyConflict)
    ));
    let watch = Caller::new(TokenScope::Watch, "phone");
    assert_eq!(
        rig.cloud
            .request_start(&watch, &request)
            .unwrap_err()
            .code(),
        "scope",
        "scope is checked before idempotency"
    );

    let pod = rig
        .cloud
        .present_confirmation(&view.confirmation)
        .await
        .unwrap();
    assert_eq!(rig.asked(), 1);
    assert_eq!(pod.state, PodState::Running);
    assert_eq!(rig.mock.pod_ids(), std::slice::from_ref(&pod.id));
    assert_eq!(rig.mock.pod_status(&pod.id), Some(PodStatus::Running));
    assert!(pod.url.ends_with(&format!("/_mock/proxy/{}/v1", pod.id)));

    match rig.cloud.request_start(&owner(), &request).unwrap() {
        StartOutcome::Started(again) => {
            assert_eq!(again.id, pod.id, "a started pod is returned on replay")
        }
        other => panic!("expected the started pod, got {other:?}"),
    }
    assert_eq!(rig.mock.pod_ids().len(), 1, "nothing new starts on replay");
    assert!(matches!(
        rig.cloud.present_confirmation(&view.confirmation).await,
        Err(CloudError::UnknownConfirmation)
    ));
}

#[tokio::test]
async fn declined_dialog_starts_nothing() {
    let rig = Rig::new().await;
    rig.confirmer
        .answers
        .lock()
        .unwrap()
        .push_back(Decision::Declined);
    let view = rig.pending(&rig.request(1.0, "decline-1"));
    assert!(matches!(
        rig.cloud.present_confirmation(&view.confirmation).await,
        Err(CloudError::Declined)
    ));
    assert!(rig.mock.pod_ids().is_empty());
}

#[tokio::test]
async fn confirmation_expires_after_five_minutes() {
    let rig = Rig::new().await;
    let request = rig.request(1.0, "late-1");
    let view = rig.pending(&request);
    rig.advance(301);
    assert!(matches!(
        rig.cloud.present_confirmation(&view.confirmation).await,
        Err(CloudError::ConfirmationExpired)
    ));
    assert_eq!(
        rig.asked(),
        0,
        "an expired confirmation never reaches the dialog"
    );
    let fresh = rig.pending(&request);
    assert_ne!(
        fresh.confirmation, view.confirmation,
        "an expired key runs as a new request"
    );

    *rig.confirmer.think.lock().unwrap() = Duration::from_secs(301);
    assert!(matches!(
        rig.cloud.present_confirmation(&fresh.confirmation).await,
        Err(CloudError::ConfirmationExpired)
    ));
    assert!(
        rig.mock.pod_ids().is_empty(),
        "a click after five minutes starts nothing"
    );
}

#[tokio::test]
async fn the_gate_runs_again_at_the_click() {
    let rig = Rig::new().await;
    let view = rig.pending(&rig.request(1.0, "regate-1"));
    rig.cloud
        .set_settings(CloudSettings {
            enabled: false,
            ..settings()
        })
        .unwrap();
    let error = rig
        .cloud
        .present_confirmation(&view.confirmation)
        .await
        .unwrap_err();
    assert_eq!(error.refusal(), Some(Refusal::CloudOff));

    rig.cloud.set_settings(settings()).unwrap();
    let view = rig.pending(&rig.request(1.0, "regate-2"));
    rig.mock.set_price(GPU, 1.29);
    let error = rig
        .cloud
        .present_confirmation(&view.confirmation)
        .await
        .unwrap_err();
    assert_eq!(error.refusal(), Some(Refusal::PriceChanged));
    assert!(rig.mock.pod_ids().is_empty());
}

#[tokio::test]
async fn running_cost_grows_per_second() {
    let rig = Rig::new().await;
    let pod = rig.start(2.0).await;
    rig.advance(600);
    rig.cloud.tick().await;
    let first = rig.cloud.pods()[0].running_cost_so_far;
    let billed = rig.mock.total_billed();
    assert!(
        close(billed, 0.89972 * 600.0 / 3600.0, 0.00001),
        "mock billed {billed}"
    );
    assert!(
        close(first, billed, 0.0001),
        "Fonsviva shows {first}, RunPod billed {billed}"
    );
    rig.advance(1);
    let one_second_later = rig.cloud.pods()[0].running_cost_so_far;
    assert!(one_second_later >= first, "billed per second");
    rig.cloud.record_request(&pod);
    rig.advance(299);
    rig.cloud.tick().await;
    let second = rig.cloud.pods()[0].running_cost_so_far;
    assert!(close(second, 0.89972 * 900.0 / 3600.0, 0.0001), "{second}");
    assert!(close(second, rig.mock.total_billed(), 0.0001));
}

#[tokio::test]
async fn idle_pod_is_stopped_after_fifteen_minutes() {
    let rig = Rig::new().await;
    let pod = rig.start(2.0).await;
    rig.advance(600);
    assert!(rig.cloud.record_request(&pod));
    rig.advance(600);
    let events = rig.cloud.tick().await;
    assert!(
        !stopped_for(&events, StopReason::Idle),
        "10 idle minutes is not enough"
    );
    assert_eq!(rig.mock.pod_status(&pod), Some(PodStatus::Running));
    rig.advance(300);
    let events = rig.cloud.tick().await;
    assert!(stopped_for(&events, StopReason::Idle), "{events:?}");
    assert_eq!(
        rig.mock.pod_status(&pod),
        Some(PodStatus::Exited),
        "the mock shows the pod stopped"
    );
    let view = &rig.cloud.pods()[0];
    assert_eq!(view.state, PodState::Stopped);
    assert_eq!(view.stop_reason, Some(StopReason::Idle));

    let at_stop = rig.mock.total_billed();
    rig.advance(3600);
    let volume_hour = rig.mock.total_billed() - at_stop;
    assert!(
        close(volume_hour, 50.0 * 0.20 / 720.0, 0.00001),
        "only the volume bills while stopped: {volume_hour}"
    );
}

#[tokio::test]
async fn budget_cap_stops_the_pod() {
    let rig = Rig::with(CloudSettings {
        idle_minutes: 600,
        caps: Caps {
            session: 1.0,
            day: 20.0,
            month: 100.0,
        },
        ..settings()
    })
    .await;
    let pod = rig.start(1.0).await;
    rig.advance(3600);
    let events = rig.cloud.tick().await;
    assert_eq!(rig.mock.pod_status(&pod), Some(PodStatus::Running));
    for percent in [50, 80] {
        assert!(
            events.iter().any(|event| matches!(event, CloudEvent::BudgetAlert { cap: CapKind::Session, percent: p, .. } if *p == percent)),
            "alert at {percent}%: {events:?}"
        );
    }
    rig.advance(600);
    let events = rig.cloud.tick().await;
    assert!(stopped_for(&events, StopReason::BudgetCap), "{events:?}");
    assert_eq!(rig.mock.pod_status(&pod), Some(PodStatus::Exited));
}

#[tokio::test]
async fn planned_hours_must_fit_the_caps() {
    let rig = Rig::with(CloudSettings {
        caps: Caps {
            session: 1.0,
            day: 3.0,
            month: 100.0,
        },
        ..settings()
    })
    .await;
    let error = rig
        .cloud
        .request_start(&owner(), &rig.request(2.0, "long-1"))
        .unwrap_err();
    assert_eq!(
        error.refusal(),
        Some(Refusal::BudgetCap),
        "2 h x $0.8997 is over the $1 session cap"
    );
    assert!(
        rig.cloud
            .request_start(&owner(), &rig.request(1.0, "short-1"))
            .is_ok()
    );
}

#[tokio::test]
async fn dead_man_stops_a_pod_fonsviva_cannot_reach() {
    let rig = Rig::new().await;
    let pod = rig.start(2.0).await;
    rig.mock.set_failing(&pod, true);
    rig.cloud.tick().await;
    rig.advance(60);
    rig.cloud.tick().await;
    assert_eq!(
        rig.mock.pod_status(&pod),
        Some(PodStatus::Running),
        "two misses in a minute is not dead yet"
    );
    rig.advance(60);
    let events = rig.cloud.tick().await;
    assert!(stopped_for(&events, StopReason::DeadMan), "{events:?}");
    assert_eq!(rig.mock.pod_status(&pod), Some(PodStatus::Exited));
}

#[tokio::test]
async fn clock_jump_stops_pods() {
    let rig = Rig::new().await;
    let pod = rig.start(2.0).await;
    rig.cloud.tick().await;
    rig.clock.jump_wall(chrono::Duration::minutes(10));
    let events = rig.cloud.tick().await;
    assert!(stopped_for(&events, StopReason::ClockJump), "{events:?}");
    assert_eq!(rig.mock.pod_status(&pod), Some(PodStatus::Exited));
}

#[tokio::test]
async fn cloud_block_refuses_before_any_network_call() {
    let rig = Rig::new().await;
    let request = rig.request(1.0, "blocked-1");
    let before = rig.mock.request_count();

    for scope in [TokenScope::Control, TokenScope::Watch] {
        let error = rig
            .cloud
            .request_start(&Caller::new(scope, "not-owner"), &request)
            .unwrap_err();
        assert_eq!(error.refusal(), Some(Refusal::Scope));
    }
    rig.cloud
        .set_settings(CloudSettings {
            enabled: false,
            ..settings()
        })
        .unwrap();
    assert_eq!(
        rig.cloud
            .request_start(&owner(), &request)
            .unwrap_err()
            .refusal(),
        Some(Refusal::CloudOff)
    );
    for profile in [Profile::UntoCaesar, Profile::Corunis, Profile::Gantrel] {
        rig.cloud
            .set_settings(CloudSettings {
                profile,
                ..settings()
            })
            .unwrap();
        let error = rig.cloud.request_start(&owner(), &request).unwrap_err();
        assert_eq!(error.refusal(), Some(Refusal::CloudBlocked), "{profile:?}");
        assert_eq!(
            rig.cloud.route_gate(profile, true),
            Err(Refusal::CloudBlocked)
        );
    }
    rig.cloud
        .set_settings(CloudSettings {
            caps: Caps {
                session: 0.5,
                day: 20.0,
                month: 100.0,
            },
            ..settings()
        })
        .unwrap();
    assert_eq!(
        rig.cloud
            .request_start(&owner(), &request)
            .unwrap_err()
            .refusal(),
        Some(Refusal::BudgetCap)
    );

    assert_eq!(
        rig.mock.request_count(),
        before,
        "every refusal happened before a network call"
    );
    assert_eq!(rig.asked(), 0);
    assert!(rig.mock.pod_ids().is_empty());

    rig.cloud.set_settings(settings()).unwrap();
    assert_eq!(
        rig.cloud.route_gate(Profile::Plugin, true),
        Err(Refusal::CloudBlocked),
        "plugin keys never reach cloud"
    );
    assert_eq!(
        rig.cloud.route_gate(Profile::Personal, false),
        Err(Refusal::Scope),
        "a key without cloud permission"
    );
    assert_eq!(rig.cloud.route_gate(Profile::Personal, true), Ok(()));
    let off = CloudSettings {
        enabled: true,
        caps: Caps {
            session: 0.0,
            day: 0.0,
            month: 0.0,
        },
        ..settings()
    };
    assert!(
        matches!(rig.cloud.set_settings(off), Err(CloudError::CapsNotSet)),
        "no cloud without caps"
    );
}

#[tokio::test]
async fn stop_reports_volume_cost_and_terminate_is_owner_only() {
    let rig = Rig::new().await;
    let pod = rig.start(2.0).await;
    let report = rig.cloud.stop(&pod).await.unwrap();
    assert_eq!(report.stopped_volume_per_hour, 0.0139);
    assert_eq!(report.stopped_volume_per_month, 10.0);
    assert!(report.note.contains("$0.0139 an hour"), "{}", report.note);
    assert_eq!(rig.mock.pod_status(&pod), Some(PodStatus::Exited));

    let asked = rig.asked();
    for scope in [TokenScope::Control, TokenScope::Watch] {
        let error = rig
            .cloud
            .terminate(&Caller::new(scope, "not-owner"), &pod)
            .await
            .unwrap_err();
        assert_eq!(error.refusal(), Some(Refusal::Scope));
    }
    assert_eq!(
        rig.asked(),
        asked,
        "a refused scope never reaches the dialog"
    );

    rig.confirmer
        .answers
        .lock()
        .unwrap()
        .push_back(Decision::Declined);
    assert!(matches!(
        rig.cloud.terminate(&owner(), &pod).await,
        Err(CloudError::Declined)
    ));
    assert_eq!(
        rig.mock.pod_status(&pod),
        Some(PodStatus::Exited),
        "only the click deletes anything"
    );
    match rig.confirmer.prompts.lock().unwrap().last() {
        Some(Prompt::Terminate(view)) => {
            assert_eq!(view.pod.id, pod);
            assert_eq!(view.volume_gb, 50);
            assert_eq!(view.requested_by.label.as_deref(), Some("Fonsviva desktop"));
            assert!(
                view.note.contains("deletes the 50 GB volume"),
                "{}",
                view.note
            );
        }
        other => panic!("expected the terminate dialog, got {other:?}"),
    }
    let view = rig.cloud.terminate(&owner(), &pod).await.unwrap();
    assert_eq!(view.state, PodState::Terminated);
    assert_eq!(rig.mock.pod_status(&pod), Some(PodStatus::Terminated));
    let billed = rig.mock.total_billed();
    rig.advance(3600);
    assert_eq!(
        rig.mock.total_billed(),
        billed,
        "terminate ends all billing"
    );
}

#[tokio::test]
async fn stopped_volume_counts_against_caps_and_is_never_deleted() {
    let caps = Caps {
        session: 1.0,
        day: 1.0,
        month: 1.0,
    };
    let rig = Rig::with(CloudSettings {
        idle_minutes: 600,
        caps,
        ..settings()
    })
    .await;
    let pod = rig.start(1.0).await;
    rig.advance(3000);
    rig.cloud.stop(&pod).await.unwrap();
    rig.advance(20 * 3600);
    let events = rig.cloud.tick().await;
    let reached = events.iter().find_map(|event| match event {
        CloudEvent::VolumeCapReached {
            cap, pods, offer, ..
        } => Some((*cap, pods.clone(), offer.clone())),
        _ => None,
    });
    assert_eq!(
        reached,
        Some((CapKind::Month, vec![pod.clone()], "terminate".to_string())),
        "{events:?}"
    );
    assert_eq!(
        rig.mock.pod_status(&pod),
        Some(PodStatus::Exited),
        "the volume is never deleted on its own"
    );
    let error = rig
        .cloud
        .request_start(&owner(), &rig.request(0.1, "after-cap"))
        .unwrap_err();
    assert_eq!(error.refusal(), Some(Refusal::BudgetCap));
    assert!(
        !rig.cloud
            .tick()
            .await
            .iter()
            .any(|event| matches!(event, CloudEvent::VolumeCapReached { .. })),
        "notified once"
    );
}

#[tokio::test]
async fn reconcile_stops_only_fonsviva_orphans() {
    let rig = Rig::new().await;
    let orphan = rig.mock.add_pod("fonsviva-lost1234", GPU);
    let other = rig.mock.add_pod("my-own-pod", GPU);
    let events = rig.cloud.reconcile().await.unwrap();
    assert!(stopped_for(&events, StopReason::Orphan));
    assert_eq!(rig.mock.pod_status(&orphan), Some(PodStatus::Exited));
    assert_eq!(
        rig.mock.pod_status(&other),
        Some(PodStatus::Running),
        "pods Fonsviva did not name are left alone"
    );
}

#[tokio::test]
async fn stop_all_turns_cloud_off() {
    let rig = Rig::new().await;
    let pod = rig.start(1.0).await;
    let events = rig.cloud.stop_all().await;
    assert!(stopped_for(&events, StopReason::EmergencyStop));
    assert_eq!(rig.mock.pod_status(&pod), Some(PodStatus::Exited));
    assert!(!rig.cloud.settings().enabled);
}

#[tokio::test]
async fn pods_and_idempotency_survive_a_restart() {
    let rig = Rig::new().await;
    let pod = rig.start(1.0).await;
    rig.advance(120);
    rig.cloud.tick().await;
    let restarted = Rig::cloud(
        &rig.mock,
        &rig.clock,
        &rig.confirmer,
        &rig.state_path,
        settings(),
    );
    let pods = restarted.pods();
    assert_eq!(pods.len(), 1);
    assert_eq!(pods[0].id, pod);
    restarted.refresh_prices().await.unwrap();
    match restarted
        .request_start(&owner(), &rig.request(1.0, "start-1"))
        .unwrap()
    {
        StartOutcome::Started(view) => assert_eq!(view.id, pod),
        other => panic!("expected the started pod after a restart, got {other:?}"),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&rig.state_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[tokio::test]
async fn client_never_shows_the_key() {
    let rig = Rig::new().await;
    let client = RunPodClient::with_base_url(
        rig.mock.url(),
        ApiKey::new(SecretString::from(MOCK_CREDENTIAL)),
    );
    assert!(!format!("{client:?}").contains(MOCK_CREDENTIAL));
    let wrong = RunPodClient::with_base_url(
        rig.mock.url(),
        ApiKey::new(SecretString::from("wrong-value")),
    );
    let error = wrong.list_pods().await.unwrap_err();
    assert!(error.to_string().contains("401"), "{error}");
    assert!(!error.to_string().contains("wrong-value"));
    assert_eq!(client.templates().await.unwrap().len(), 1);
    let gpus = client.gpu_types().await.unwrap();
    assert!(
        gpus.iter()
            .any(|gpu| gpu.id == GPU && gpu.price.secure == 0.89)
    );
}

#[tokio::test]
async fn demo_pod_answers_through_its_own_key() {
    let rig = Rig::new().await;
    let pod = rig.start(1.0).await;
    let models = rig.cloud.cloud_models();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, format!("cloud:runpod/{pod}"));
    assert_eq!(models[0].location, "cloud");
    assert_eq!(models[0].cost_per_hour, 0.8997);
    for profile in [Profile::Plugin, Profile::UntoCaesar] {
        let error = rig.cloud.route(&models[0].id, profile, true).unwrap_err();
        assert_eq!(error.refusal(), Some(Refusal::CloudBlocked), "{profile:?}");
    }
    let error = rig
        .cloud
        .route(&models[0].id, Profile::Personal, false)
        .unwrap_err();
    assert_eq!(
        error.refusal(),
        Some(Refusal::Scope),
        "a key without cloud permission"
    );
    rig.advance(600);
    let upstream = rig
        .cloud
        .route(&models[0].id, Profile::Personal, true)
        .unwrap();
    assert_eq!(
        rig.cloud.pods()[0].idle_minutes,
        0.0,
        "a routed request is activity"
    );
    assert_eq!(upstream.model, "ggml-org/gpt-oss-20b-GGUF");
    assert!(!format!("{upstream:?}").contains(upstream.api_key.expose_secret()));
    let http = reqwest::Client::new();
    let body = serde_json::json!({"model": upstream.model, "messages": [{"role": "user", "content": "hi"}]});
    let answer: serde_json::Value = http
        .post(format!("{}/chat/completions", upstream.base_url))
        .bearer_auth(upstream.api_key.expose_secret())
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        answer["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .contains("demo pod")
    );
    let refused = http
        .post(format!("{}/chat/completions", upstream.base_url))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401);
    rig.cloud.stop(&pod).await.unwrap();
    assert!(
        rig.cloud.upstream(&pod).is_none(),
        "a stopped pod is not routable"
    );
    assert!(rig.cloud.cloud_models().is_empty());
    assert!(matches!(
        rig.cloud.route(&models[0].id, Profile::Personal, true),
        Err(CloudError::NotFound(_))
    ));
}

#[tokio::test]
async fn only_one_confirmation_is_open_at_a_time() {
    let rig = Rig::new().await;
    let first = rig.pending(&rig.request(1.0, "first"));
    assert_eq!(first.requested_by.scope, TokenScope::Owner);
    assert_eq!(
        first.requested_by.label.as_deref(),
        Some("Fonsviva desktop")
    );
    let error = rig
        .cloud
        .request_start(&owner(), &rig.request(1.0, "second"))
        .unwrap_err();
    assert_eq!(error.code(), "pending");
    assert_eq!(
        rig.pending(&rig.request(1.0, "first")).confirmation,
        first.confirmation,
        "the open one can still be fetched again"
    );
    rig.advance(301);
    let second = rig.pending(&rig.request(1.0, "second"));
    assert_ne!(
        second.confirmation, first.confirmation,
        "an expired one no longer blocks"
    );
    rig.cloud
        .present_confirmation(&second.confirmation)
        .await
        .unwrap();
    assert!(
        matches!(
            rig.cloud
                .request_start(&owner(), &rig.request(1.0, "third")),
            Ok(StartOutcome::Pending(_))
        ),
        "an answered one no longer blocks"
    );
}

#[tokio::test]
async fn templates_route_shape_matches_the_api() {
    let rig = Rig::new().await;
    let json = serde_json::to_value(rig.cloud.templates()).unwrap();
    let llama = json
        .as_array()
        .unwrap()
        .iter()
        .find(|template| template["id"] == "llama-cpp")
        .unwrap();
    assert!(llama["name"].is_string());
    let gpu = llama["gpus"]
        .as_array()
        .unwrap()
        .iter()
        .find(|gpu| gpu["gpu"] == GPU)
        .unwrap();
    assert_eq!(
        gpu["per_hour"], 0.8997,
        "per_hour is the figure the start confirms"
    );
}

#[test]
fn key_profiles_map_to_the_gate() {
    assert_eq!(
        Profile::from_key_profile("personal"),
        Some(Profile::Personal)
    );
    assert_eq!(Profile::from_key_profile("plugin"), Some(Profile::Plugin));
    assert_eq!(
        Profile::from_key_profile("untocaesar"),
        Some(Profile::UntoCaesar)
    );
    assert_eq!(Profile::from_key_profile("anything-else"), None);
}
