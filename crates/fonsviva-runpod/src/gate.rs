//! The cloud gate's inputs and refusal codes (docs/API.md, "Cloud gate").
//! Every check here runs on data Fonsviva already holds, so a refusal never
//! costs a network call.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The scope of the token that made a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenScope {
    Owner,
    Control,
    Watch,
}

/// Who is asking: the token's scope, a stable id for the token that made
/// the request (never the token itself), which idempotency keys are bound
/// to, and the token's label, which the confirmation dialog shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub scope: TokenScope,
    pub requester: String,
    pub label: Option<String>,
}

impl Caller {
    pub fn new(scope: TokenScope, requester: impl Into<String>) -> Self {
        Self {
            scope,
            requester: requester.into(),
            label: None,
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

/// The active profile (docs/CLOUD.md), or the profile an app key holds.
/// Only `Personal` may use cloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Personal,
    UntoCaesar,
    Corunis,
    Gantrel,
    /// A Local Studio plugin key (docs/API.md, "Plugin keys"). It never
    /// reaches a cloud model, whatever the settings say.
    Plugin,
}

impl Profile {
    /// The profile named on a stored app key (`"personal"`, `"plugin"`, ...).
    /// An unknown name is `None`, which the daemon treats as no cloud.
    pub fn from_key_profile(name: &str) -> Option<Self> {
        match name {
            "personal" => Some(Profile::Personal),
            "untocaesar" => Some(Profile::UntoCaesar),
            "corunis" => Some(Profile::Corunis),
            "gantrel" => Some(Profile::Gantrel),
            "plugin" => Some(Profile::Plugin),
            _ => None,
        }
    }
}

/// Hard caps in USD (docs/COSTS.md).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Caps {
    pub session: f64,
    pub day: f64,
    pub month: f64,
}

impl Caps {
    pub fn are_set(&self) -> bool {
        [self.session, self.day, self.month]
            .iter()
            .all(|cap| cap.is_finite() && *cap > 0.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapKind {
    Session,
    Day,
    Month,
}

/// When the supervisor gives up on a pod it cannot reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadMan {
    /// Consecutive failed status reads before the stop...
    pub max_failures: u32,
    /// ...that also span at least this many seconds.
    pub after_secs: u64,
}

impl Default for DeadMan {
    fn default() -> Self {
        Self {
            max_failures: 3,
            after_secs: 120,
        }
    }
}

/// The settings the gate and supervisor read. The daemon owns them and
/// passes changes in with [`crate::Cloud::set_settings`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CloudSettings {
    /// Cloud is off until the person turns it on.
    pub enabled: bool,
    pub profile: Profile,
    pub caps: Caps,
    /// Stop a pod after this many minutes with no proxied request.
    pub idle_minutes: u32,
    pub dead_man: DeadMan,
}

pub const DEFAULT_IDLE_MINUTES: u32 = 10;

impl Default for CloudSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            profile: Profile::Personal,
            caps: Caps {
                session: 0.0,
                day: 0.0,
                month: 0.0,
            },
            idle_minutes: DEFAULT_IDLE_MINUTES,
            dead_man: DeadMan::default(),
        }
    }
}

/// Why the gate said no. `code()` is what the daemon puts in its error JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    Scope,
    CloudOff,
    CloudBlocked,
    BudgetCap,
    PriceChanged,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Refusal::Scope => "scope",
            Refusal::CloudOff => "cloud_off",
            Refusal::CloudBlocked => "cloud_blocked",
            Refusal::BudgetCap => "budget_cap",
            Refusal::PriceChanged => "price_changed",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Refusal::Scope => {
                "This token cannot do that. Starting or terminating a pod needs the owner token."
            }
            Refusal::CloudOff => "Cloud is off. Turn it on in Settings to use a rented GPU.",
            Refusal::CloudBlocked => {
                "Cloud is only for the personal profile. Tax and school work never runs on a rented GPU."
            }
            Refusal::BudgetCap => "This would go over a budget cap.",
            Refusal::PriceChanged => {
                "The price changed since you saw the estimate. Review the new estimate and confirm again."
            }
        }
    }

    pub fn fix(self) -> Option<&'static str> {
        match self {
            Refusal::CloudOff => Some("Settings > Cloud > Turn on"),
            Refusal::BudgetCap => {
                Some("Plan fewer hours, pick a cheaper GPU, or raise the cap in Settings")
            }
            Refusal::PriceChanged => Some("Request a new estimate and confirm its per-hour cost"),
            _ => None,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}
