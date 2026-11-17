//! One-click, reversible wiring of a Fonsviva endpoint into pi, T3 Code,
//! Local Studio's Eukhe agent, and any OpenAI-compatible client.
//!
//! [`Wire::apply`] backs up the user's file whenever the target is absent
//! (exact bytes, under `<shared dir>/backups/wire/<target>/`; while it stays
//! applied that backup is kept), splices in Fonsviva's entry without touching
//! any other byte, writes atomically with the file's mode, and reads it back.
//! [`Wire::revert`] restores that backup when the file still holds exactly
//! what Fonsviva wrote.
//!
//! A target is drifted when the file changed after Fonsviva wrote it. Apply
//! and revert then refuse with [`WireError::Drifted`] and the diff; sent again
//! with `confirm`, they first save the current file as a second backup, then
//! proceed (revert removes only Fonsviva's entries and keeps the edits).
//! Every apply and revert is appended to `<shared dir>/wire/journal.jsonl`.

mod fsio;
mod json_edit;
mod keys;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use json_edit::EditError;
pub use keys::{KeyStore, MemoryKeys, OsKeychain, SERVICE as KEY_SERVICE, read_command};

/// The provider id Fonsviva owns in pi's and Eukhe's `models.json`.
pub const PROVIDER_ID: &str = "fonsviva";
/// The install step T3 Code itself prints when pi is missing.
pub const PI_INSTALL: &str = "npm install -g --ignore-scripts @earendil-works/pi-coding-agent";
/// The `apiKey` value for an endpoint without a key (Local Studio's `NO_KEY`).
pub const NO_KEY: &str = "none";

/// One model the endpoint serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub context_window: u64,
    pub max_tokens: u64,
    pub reasoning: bool,
}

/// What to wire: an OpenAI-compatible base URL (ending in `/v1`), an
/// optional key, and the models to list.
#[derive(Clone)]
pub struct Endpoint {
    pub base_url: String,
    pub api_key: Option<SecretString>,
    pub models: Vec<Model>,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("models", &self.models)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Pi,
    T3code,
    Eukhe,
    Openai,
}

impl Target {
    pub const ALL: [Target; 4] = [Target::Pi, Target::T3code, Target::Eukhe, Target::Openai];

    pub fn as_str(self) -> &'static str {
        match self {
            Target::Pi => "pi",
            Target::T3code => "t3code",
            Target::Eukhe => "eukhe",
            Target::Openai => "openai",
        }
    }

    fn key_account(self) -> String {
        format!("wire-{}", self.as_str())
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Target {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Target::ALL
            .into_iter()
            .find(|target| target.as_str() == value)
            .ok_or_else(|| format!("unknown wire target \"{value}\" (pi, t3code, eukhe, openai)"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Fonsviva's entry is not in the file.
    Absent,
    /// The file holds exactly what Fonsviva wrote.
    Applied,
    /// Fonsviva's entry is there but someone changed it.
    Drifted,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub target: Target,
    pub state: State,
    /// The file this target writes; `None` for `openai`.
    pub path: Option<PathBuf>,
    /// The backup `revert` would restore.
    pub backup: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    pub target: Target,
    pub path: Option<PathBuf>,
    pub backup: Option<PathBuf>,
    /// The second backup a confirmed apply over a drifted file saved.
    pub drift_backup: Option<PathBuf>,
    /// False when the file already held exactly this entry.
    pub changed: bool,
    /// Changed lines only, with any key-like values redacted.
    pub diff: String,
    pub note: Option<String>,
    pub openai: Option<OpenAiSetup>,
}

/// What the `openai` target hands back instead of writing a file.
#[derive(Debug, Clone, Serialize)]
pub struct OpenAiSetup {
    pub base_url: String,
    /// Where the key lives, never the key.
    pub key_ref: String,
    pub env: String,
    pub curl: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RevertOutcome {
    /// The original bytes are back (or the file is gone if there was none).
    Restored,
    /// The user edited the file since; only Fonsviva's entries were removed.
    RemovedEntries,
    /// Nothing of Fonsviva's was there.
    NothingToRevert,
}

#[derive(Debug, Clone, Serialize)]
pub struct RevertReport {
    pub target: Target,
    pub path: Option<PathBuf>,
    pub outcome: RevertOutcome,
    /// The second backup a confirmed revert of a drifted file saved.
    pub drift_backup: Option<PathBuf>,
    pub diff: String,
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("{program} is not installed. Install it with: {install}")]
    NotInstalled {
        program: &'static str,
        install: &'static str,
    },
    #[error(
        "{path} is not plain JSON (comments or trailing commas?), so Fonsviva will not rewrite it. Edit it by hand."
    )]
    NotPlainJson { path: PathBuf },
    #[error("{path}: {problem}")]
    Unexpected { path: PathBuf, problem: String },
    #[error("cannot find the home directory (set {0})")]
    NoHome(&'static str),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("keychain: {0}")]
    Keychain(String),
    #[error("{0}")]
    InvalidEndpoint(String),
    #[error(
        "{path} changed after Fonsviva wrote it; send again with confirm to back it up and proceed"
    )]
    Drifted { path: PathBuf, diff: String },
}

impl WireError {
    /// A short code the daemon can put in its error JSON.
    pub fn code(&self) -> &'static str {
        match self {
            WireError::NotInstalled { .. } => "not_installed",
            WireError::NotPlainJson { .. } => "not_plain_json",
            WireError::Unexpected { .. } => "unexpected_file",
            WireError::NoHome(_) => "no_home",
            WireError::Io { .. } => "io",
            WireError::Keychain(_) => "keychain",
            WireError::InvalidEndpoint(_) => "invalid_endpoint",
            WireError::Drifted { .. } => "drifted",
        }
    }

    /// The fix to show next to the message, when there is one.
    pub fn fix(&self) -> Option<String> {
        match self {
            WireError::NotInstalled { install, .. } => Some((*install).to_string()),
            WireError::NotPlainJson { path } => Some(format!(
                "Remove comments and trailing commas from {}",
                path.display()
            )),
            WireError::Drifted { .. } => {
                Some("Review the diff, then send again with {\"confirm\":true}".to_string())
            }
            _ => None,
        }
    }
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> WireError + '_ {
    move |source| WireError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    MacOs,
    Windows,
}

impl Os {
    pub fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Os::MacOs,
            "windows" => Os::Windows,
            _ => Os::Linux,
        }
    }

    fn sep(self) -> char {
        if self == Os::Windows { '\\' } else { '/' }
    }
}

/// Everything path resolution reads, so tests can describe any machine.
#[derive(Clone)]
pub struct WireEnv {
    pub os: Os,
    pub vars: BTreeMap<String, String>,
    /// Where wiring state lives. The daemon passes `<data dir>/shared`, which
    /// stable and nightly share (docs/API.md), so a target wired from one
    /// channel can be reverted from the other. Backups go to
    /// `backups/wire/<target>/`, records and the journal to `wire/`.
    pub shared_dir: PathBuf,
}

impl WireEnv {
    pub fn from_process(shared_dir: PathBuf) -> Self {
        Self {
            os: Os::current(),
            vars: std::env::vars().collect(),
            shared_dir,
        }
    }

    fn var(&self, name: &str) -> Option<&str> {
        self.vars
            .get(name)
            .map(String::as_str)
            .filter(|value| !value.trim().is_empty())
    }

    /// Node's `os.homedir()`: `HOME` on Unix, `USERPROFILE` on Windows.
    fn node_home(&self) -> Option<String> {
        match self.os {
            Os::Windows => self.var("USERPROFILE").map(str::to_string).or_else(|| {
                Some(format!(
                    "{}{}",
                    self.var("HOMEDRIVE")?,
                    self.var("HOMEPATH")?
                ))
            }),
            _ => self.var("HOME").map(str::to_string),
        }
    }

    fn join(&self, base: &str, parts: &[&str]) -> PathBuf {
        let sep = self.os.sep();
        let mut out = base.trim_end_matches(['/', '\\']).to_string();
        if out.is_empty() {
            out.push(sep);
        }
        for part in parts {
            out.push(sep);
            out.push_str(part);
        }
        PathBuf::from(out)
    }

    fn expand_tilde(&self, raw: &str, home: Option<&str>) -> String {
        let raw = raw.trim();
        match home {
            Some(home) if raw == "~" => home.to_string(),
            Some(home) if raw.starts_with("~/") || raw.starts_with("~\\") => {
                self.join(home, &[&raw[2..]]).to_string_lossy().into_owned()
            }
            _ => raw.to_string(),
        }
    }

    /// pi: `$PI_CODING_AGENT_DIR` (with `~` expanded) or `~/.pi/agent`.
    pub fn pi_agent_dir(&self) -> Result<PathBuf, WireError> {
        let home = self.node_home();
        if let Some(dir) = self.var("PI_CODING_AGENT_DIR") {
            return Ok(PathBuf::from(self.expand_tilde(dir, home.as_deref())));
        }
        let home = home.ok_or(WireError::NoHome(if self.os == Os::Windows {
            "USERPROFILE"
        } else {
            "HOME"
        }))?;
        Ok(self.join(&home, &[".pi", "agent"]))
    }

    /// Eukhe (eukhe-types `agent_dir`): `$EUKHE_CODING_AGENT_DIR` or
    /// `$HOME/.eukhe`. Eukhe reads only `HOME`, on every OS.
    pub fn eukhe_agent_dir(&self) -> Result<PathBuf, WireError> {
        let home = self.var("HOME").map(str::to_string);
        if let Some(dir) = self.var("EUKHE_CODING_AGENT_DIR") {
            return Ok(PathBuf::from(self.expand_tilde(dir, home.as_deref())));
        }
        Ok(self.join(&home.ok_or(WireError::NoHome("HOME"))?, &[".eukhe"]))
    }

    /// T3 Code: `$T3CODE_HOME` or `~/.t3`, then `userdata/settings.json`, the
    /// same on Linux, macOS and Windows.
    pub fn t3_settings_path(&self) -> Result<PathBuf, WireError> {
        let home = self.node_home();
        let base = match self.var("T3CODE_HOME") {
            Some(dir) => self.expand_tilde(dir, home.as_deref()),
            None => self
                .join(
                    &home.ok_or(WireError::NoHome(if self.os == Os::Windows {
                        "USERPROFILE"
                    } else {
                        "HOME"
                    }))?,
                    &[".t3"],
                )
                .to_string_lossy()
                .into_owned(),
        };
        Ok(self.join(&base, &["userdata", "settings.json"]))
    }

    /// The file `target` writes, or `None` for `openai`.
    pub fn target_path(&self, target: Target) -> Result<Option<PathBuf>, WireError> {
        Ok(match target {
            Target::Pi => Some(self.pi_agent_dir()?.join("models.json")),
            Target::Eukhe => Some(self.eukhe_agent_dir()?.join("models.json")),
            Target::T3code => Some(self.t3_settings_path()?),
            Target::Openai => None,
        })
    }

    /// Finds `pi` on `PATH` the way a shell would.
    pub fn find_pi(&self) -> Option<PathBuf> {
        let path = self.vars.get("PATH").or_else(|| self.vars.get("Path"))?;
        let separator = if self.os == Os::Windows { ';' } else { ':' };
        let names: &[&str] = if self.os == Os::Windows {
            &["pi.exe", "pi.cmd", "pi.bat", "pi"]
        } else {
            &["pi"]
        };
        path.split(separator)
            .filter(|dir| !dir.is_empty())
            .find_map(|dir| {
                names
                    .iter()
                    .map(|name| Path::new(dir).join(name))
                    .find(|candidate| candidate.is_file())
            })
    }

    fn wire_dir(&self) -> PathBuf {
        self.shared_dir.join("wire")
    }

    fn backup_dir(&self, target: Target) -> PathBuf {
        self.shared_dir
            .join("backups")
            .join("wire")
            .join(target.as_str())
    }
}

/// What Fonsviva remembers about one applied target.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    path: Option<PathBuf>,
    /// The user's file before the first apply; `None` if it did not exist.
    original: Option<Original>,
    written_sha256: Option<String>,
    /// The text Fonsviva last wrote, to show what drifted.
    #[serde(default)]
    written: Option<String>,
    /// True while every write since the first apply started from the
    /// original or from Fonsviva's own last write.
    base_is_original: bool,
    /// The JSON path of Fonsviva's entry and the value written there.
    entry_path: Vec<String>,
    entry: Value,
    /// Parent objects Fonsviva created and should remove when empty.
    created_parents: Vec<String>,
    /// T3 Code: the instance was there already; restore its `enabled`.
    t3_previous_enabled: Option<Option<bool>>,
    key_account: Option<String>,
    applied_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Original {
    backup: PathBuf,
    sha256: String,
    mode: Option<u32>,
}

/// The wiring service. Cheap to clone; one per daemon.
#[derive(Clone)]
pub struct Wire {
    env: WireEnv,
    keys: Arc<dyn KeyStore>,
}

impl Wire {
    pub fn new(env: WireEnv, keys: Arc<dyn KeyStore>) -> Self {
        Self { env, keys }
    }

    pub fn env(&self) -> &WireEnv {
        &self.env
    }

    /// The state of every target.
    pub fn status_all(&self) -> Vec<Result<Status, WireError>> {
        Target::ALL
            .into_iter()
            .map(|target| self.status(target))
            .collect()
    }

    pub fn status(&self, target: Target) -> Result<Status, WireError> {
        let path = self.env.target_path(target)?;
        let record = self.load_record(target)?;
        let backup = record
            .as_ref()
            .and_then(|record| record.original.as_ref())
            .map(|original| original.backup.clone());
        let Some(path) = path else {
            let state = if record.is_some() {
                State::Applied
            } else {
                State::Absent
            };
            return Ok(Status {
                target,
                state,
                path: None,
                backup,
            });
        };
        let bytes = fsio::read_opt(&path).map_err(io_err(&path))?;
        let text = String::from_utf8_lossy(bytes.as_deref().unwrap_or_default()).into_owned();
        let doc = match json_edit::Doc::parse(&text) {
            Ok(doc) => doc,
            Err(_) => {
                return Ok(Status {
                    target,
                    state: State::Drifted,
                    path: Some(path),
                    backup,
                });
            }
        };
        let state = if drift(target, record.as_ref(), &doc, &text).is_some() {
            State::Drifted
        } else {
            match (target, &record) {
                (Target::T3code, None) => {
                    let instances = doc
                        .get(&["providerInstances"])
                        .and_then(|value| value.as_object().cloned());
                    let enabled_pi = instances.is_some_and(|instances| {
                        instances.values().any(|instance| {
                            instance["driver"] == "pi" && instance["enabled"] == true
                        })
                    });
                    if enabled_pi {
                        State::Applied
                    } else {
                        State::Absent
                    }
                }
                (_, Some(record)) if doc.get(&path_refs(&record.entry_path)).is_some() => {
                    State::Applied
                }
                _ => State::Absent,
            }
        };
        Ok(Status {
            target,
            state,
            path: Some(path),
            backup,
        })
    }

    /// Wires `target`. On a drifted target this refuses with
    /// [`WireError::Drifted`] unless `confirm` is set.
    pub fn apply(
        &self,
        target: Target,
        endpoint: &Endpoint,
        confirm: bool,
    ) -> Result<ApplyReport, WireError> {
        validate(target, endpoint)?;
        if target == Target::Openai {
            return self.apply_openai(endpoint);
        }
        let path = self.env.target_path(target)?.unwrap_or_default();
        let pi_binary = if target == Target::T3code {
            Some(self.env.find_pi().ok_or(WireError::NotInstalled {
                program: "pi",
                install: PI_INSTALL,
            })?)
        } else {
            None
        };
        let current = fsio::read_opt(&path).map_err(io_err(&path))?;
        let current_text = match &current {
            Some(bytes) => String::from_utf8(bytes.clone())
                .map_err(|_| WireError::NotPlainJson { path: path.clone() })?,
            None => String::new(),
        };
        let doc = json_edit::Doc::parse(&current_text).map_err(|error| edit_error(&path, error))?;
        let mut record = self.load_record(target)?;
        if record
            .as_ref()
            .is_some_and(|record| doc.get(&path_refs(&record.entry_path)).is_none())
        {
            record = None;
        }
        let drift_backup = match (
            drift(target, record.as_ref(), &doc, &current_text),
            &current,
        ) {
            (Some(diff), _) if !confirm => return Err(WireError::Drifted { path, diff }),
            (Some(_), Some(bytes)) if record.is_some() => Some(self.write_backup(target, bytes)?),
            _ => None,
        };

        let key_account = match (&endpoint.api_key, target) {
            (Some(key), Target::Pi | Target::Eukhe) => {
                let account = target.key_account();
                self.keys.set(&account, key).map_err(WireError::Keychain)?;
                Some(account)
            }
            _ => None,
        };

        let (entry_path, entry, previous_enabled) = match target {
            Target::T3code => t3_entry(&doc, record.as_ref(), pi_binary.as_deref())?,
            _ => (
                vec!["providers".to_string(), PROVIDER_ID.to_string()],
                provider_entry(endpoint, self.provider_key(key_account.as_deref())),
                None,
            ),
        };
        let parents_before: Vec<String> = entry_path[..entry_path.len() - 1]
            .iter()
            .scan(Vec::new(), |prefix: &mut Vec<String>, key| {
                prefix.push(key.clone());
                Some((key.clone(), doc.get(&path_refs(prefix)).is_some()))
            })
            .filter(|(_, existed)| !existed)
            .map(|(key, _)| key)
            .collect();
        let mut new_text = doc
            .upsert(&path_refs(&entry_path), &entry)
            .map_err(|error| edit_error(&path, error))?;
        if current.is_none() {
            new_text.push('\n');
        }
        let changed = current.as_deref() != Some(new_text.as_bytes());

        let backup = match record.as_ref().and_then(|record| record.original.clone()) {
            Some(original) => Some(original.backup),
            None if record.is_some() => None,
            None => match &current {
                Some(bytes) => Some(self.write_backup(target, bytes)?),
                None => None,
            },
        };
        let mode = match target {
            Target::Eukhe => 0o600,
            _ => fsio::mode_of(&path).unwrap_or(0o600),
        };
        if changed {
            fsio::write_atomic(&path, new_text.as_bytes(), mode).map_err(io_err(&path))?;
        }
        let written_sha = fsio::sha256(new_text.as_bytes());
        let current_sha = current.as_deref().map(fsio::sha256);
        let next = match record.take() {
            Some(mut previous) => {
                let unchanged_since_our_write =
                    current_sha.is_some() && current_sha == previous.written_sha256;
                previous.base_is_original = previous.base_is_original && unchanged_since_our_write;
                previous.written_sha256 = Some(written_sha.clone());
                previous.written = Some(new_text.clone());
                previous.entry = serde_json::to_value(&entry).unwrap_or_default();
                previous.key_account = key_account.or(previous.key_account);
                previous
            }
            None => Record {
                path: Some(path.clone()),
                original: match (&current, &backup) {
                    (Some(bytes), Some(backup)) => Some(Original {
                        backup: backup.clone(),
                        sha256: fsio::sha256(bytes),
                        mode: fsio::mode_of(&path).or(Some(mode)),
                    }),
                    _ => None,
                },
                written_sha256: Some(written_sha.clone()),
                written: Some(new_text.clone()),
                base_is_original: true,
                entry_path: entry_path.clone(),
                entry: serde_json::to_value(&entry).unwrap_or_default(),
                created_parents: parents_before,
                t3_previous_enabled: previous_enabled,
                key_account,
                applied_at: now(),
            },
        };
        self.save_record(target, &next)?;
        self.journal(
            target,
            "apply",
            Some(&path),
            current_sha.as_deref(),
            Some(&written_sha),
        )?;
        let note = (target == Target::T3code).then(|| {
            "T3 Code lists pi's models; apply the pi target too so Fonsviva's models show up."
                .to_string()
        });
        Ok(ApplyReport {
            target,
            path: Some(path),
            backup,
            drift_backup,
            changed,
            diff: diff(&current_text, &new_text),
            note,
            openai: None,
        })
    }

    /// Unwires `target`. On a drifted target this refuses with
    /// [`WireError::Drifted`] unless `confirm` is set.
    pub fn revert(&self, target: Target, confirm: bool) -> Result<RevertReport, WireError> {
        let record = self.load_record(target)?;
        let path = self.env.target_path(target)?;
        let Some(path) = path else {
            if let Some(account) = record
                .as_ref()
                .and_then(|record| record.key_account.clone())
            {
                self.keys.delete(&account).map_err(WireError::Keychain)?;
            }
            let outcome = if record.is_some() {
                RevertOutcome::Restored
            } else {
                RevertOutcome::NothingToRevert
            };
            self.clear_record(target)?;
            self.journal(target, "revert", None, None, None)?;
            return Ok(RevertReport {
                target,
                path: None,
                outcome,
                drift_backup: None,
                diff: String::new(),
            });
        };
        let current = fsio::read_opt(&path).map_err(io_err(&path))?;
        let current_sha = current.as_deref().map(fsio::sha256);
        let current_text =
            String::from_utf8_lossy(current.as_deref().unwrap_or_default()).into_owned();
        let drift_backup = match &current {
            Some(bytes) => {
                let doc = json_edit::Doc::parse(&current_text)
                    .map_err(|error| edit_error(&path, error))?;
                match drift(target, record.as_ref(), &doc, &current_text) {
                    Some(diff) if !confirm => return Err(WireError::Drifted { path, diff }),
                    Some(_) => Some(self.write_backup(target, bytes)?),
                    None => None,
                }
            }
            None => None,
        };

        let ours_untouched = record.as_ref().is_some_and(|record| {
            record.base_is_original && current_sha.is_some() && current_sha == record.written_sha256
        });
        let (outcome, new_text) = if ours_untouched {
            let record = record.as_ref().expect("checked above");
            match &record.original {
                Some(original) => {
                    let bytes =
                        std::fs::read(&original.backup).map_err(io_err(&original.backup))?;
                    if fsio::sha256(&bytes) != original.sha256 {
                        return Err(WireError::Unexpected {
                            path: original.backup.clone(),
                            problem: "the backup no longer matches its recorded checksum"
                                .to_string(),
                        });
                    }
                    fsio::write_atomic(&path, &bytes, original.mode.unwrap_or(0o600))
                        .map_err(io_err(&path))?;
                    (
                        RevertOutcome::Restored,
                        Some(String::from_utf8_lossy(&bytes).into_owned()),
                    )
                }
                None => {
                    std::fs::remove_file(&path).map_err(io_err(&path))?;
                    (RevertOutcome::Restored, None)
                }
            }
        } else if current.is_none() {
            (RevertOutcome::NothingToRevert, None)
        } else {
            let doc =
                json_edit::Doc::parse(&current_text).map_err(|error| edit_error(&path, error))?;
            match strip(&doc, target, record.as_ref()) {
                Some(stripped) => {
                    let mode = fsio::mode_of(&path).unwrap_or(0o600);
                    fsio::write_atomic(&path, stripped.as_bytes(), mode).map_err(io_err(&path))?;
                    (RevertOutcome::RemovedEntries, Some(stripped))
                }
                None => (RevertOutcome::NothingToRevert, Some(current_text.clone())),
            }
        };
        if let Some(account) = record
            .as_ref()
            .and_then(|record| record.key_account.clone())
        {
            self.keys.delete(&account).map_err(WireError::Keychain)?;
        }
        let after_sha = new_text
            .as_deref()
            .map(|text| fsio::sha256(text.as_bytes()));
        self.clear_record(target)?;
        self.journal(
            target,
            "revert",
            Some(&path),
            current_sha.as_deref(),
            after_sha.as_deref(),
        )?;
        Ok(RevertReport {
            target,
            path: Some(path),
            outcome,
            drift_backup,
            diff: diff(&current_text, new_text.as_deref().unwrap_or_default()),
        })
    }

    fn apply_openai(&self, endpoint: &Endpoint) -> Result<ApplyReport, WireError> {
        let target = Target::Openai;
        let account = target.key_account();
        let key_account = match &endpoint.api_key {
            Some(key) => {
                self.keys.set(&account, key).map_err(WireError::Keychain)?;
                Some(account.clone())
            }
            None => None,
        };
        let base = endpoint.base_url.trim_end_matches('/').to_string();
        let model = endpoint
            .models
            .first()
            .map_or("MODEL", |model| model.id.as_str());
        let (key_ref, read) = match &key_account {
            Some(account) => (
                format!(
                    "OS keychain, service \"{}\", account \"{account}\"",
                    keys::SERVICE
                ),
                Some(read_command(self.env.os, account)),
            ),
            None => ("none (this endpoint takes no key)".to_string(), None),
        };
        let env = match (self.env.os, &read) {
            (Os::Windows, Some(read)) => {
                format!("$env:OPENAI_BASE_URL = \"{base}\"\n$env:OPENAI_API_KEY = ({read})")
            }
            (Os::Windows, None) => {
                format!("$env:OPENAI_BASE_URL = \"{base}\"\n$env:OPENAI_API_KEY = \"{NO_KEY}\"")
            }
            (_, Some(read)) => {
                format!("export OPENAI_BASE_URL=\"{base}\"\nexport OPENAI_API_KEY=\"$({read})\"")
            }
            (_, None) => {
                format!("export OPENAI_BASE_URL=\"{base}\"\nexport OPENAI_API_KEY=\"{NO_KEY}\"")
            }
        };
        let body = json!({"model": model, "messages": [{"role": "user", "content": "Say hello in five words."}]});
        let curl = format!(
            "curl -s \"$OPENAI_BASE_URL/chat/completions\" -H \"Authorization: Bearer $OPENAI_API_KEY\" -H \"Content-Type: application/json\" -d '{body}'"
        );
        let record = Record {
            path: None,
            original: None,
            written_sha256: None,
            written: None,
            base_is_original: true,
            entry_path: Vec::new(),
            entry: json!({"baseUrl": base}),
            created_parents: Vec::new(),
            t3_previous_enabled: None,
            key_account,
            applied_at: now(),
        };
        self.save_record(target, &record)?;
        self.journal(target, "apply", None, None, None)?;
        Ok(ApplyReport {
            target,
            path: None,
            backup: None,
            drift_backup: None,
            changed: false,
            diff: String::new(),
            note: None,
            openai: Some(OpenAiSetup {
                base_url: base,
                key_ref,
                env,
                curl,
            }),
        })
    }

    fn provider_key(&self, account: Option<&str>) -> String {
        match account {
            Some(account) => format!("!{}", read_command(self.env.os, account)),
            None => NO_KEY.to_string(),
        }
    }

    fn write_backup(&self, target: Target, bytes: &[u8]) -> Result<PathBuf, WireError> {
        let dir = self.env.backup_dir(target);
        fsio::create_private_dir(&dir).map_err(io_err(&dir))?;
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ").to_string();
        let mut path = dir.join(&stamp);
        let mut n = 1;
        while path.exists() {
            path = dir.join(format!("{stamp}-{n}"));
            n += 1;
        }
        fsio::write_atomic(&path, bytes, 0o600).map_err(io_err(&path))
    }

    fn record_path(&self, target: Target) -> PathBuf {
        self.env.wire_dir().join(format!("{target}.json"))
    }

    fn load_record(&self, target: Target) -> Result<Option<Record>, WireError> {
        let path = self.record_path(target);
        match fsio::read_opt(&path).map_err(io_err(&path))? {
            Some(bytes) => {
                serde_json::from_slice(&bytes)
                    .map(Some)
                    .map_err(|error| WireError::Unexpected {
                        path,
                        problem: error.to_string(),
                    })
            }
            None => Ok(None),
        }
    }

    fn save_record(&self, target: Target, record: &Record) -> Result<(), WireError> {
        let path = self.record_path(target);
        let bytes = serde_json::to_vec_pretty(record).unwrap_or_default();
        fsio::write_atomic(&path, &bytes, 0o600).map_err(io_err(&path))?;
        Ok(())
    }

    fn clear_record(&self, target: Target) -> Result<(), WireError> {
        let path = self.record_path(target);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(WireError::Io {
                path,
                source: error,
            }),
        }
    }

    fn journal(
        &self,
        target: Target,
        action: &str,
        path: Option<&Path>,
        before: Option<&str>,
        after: Option<&str>,
    ) -> Result<(), WireError> {
        let journal = self.env.wire_dir().join("journal.jsonl");
        let line = json!({"at": now(), "target": target, "action": action, "path": path, "before_sha256": before, "after_sha256": after});
        fsio::append_line(&journal, &line.to_string()).map_err(io_err(&journal))
    }
}

/// Prints a stored wiring key, for `fonsviva key print <account>` on
/// Windows, where the OS has no command-line reader for Credential Manager.
pub fn reveal_key(keys: &dyn KeyStore, account: &str) -> Result<Option<SecretString>, String> {
    keys.get(account)
}

#[derive(Serialize)]
struct ProviderEntry<'a> {
    name: &'static str,
    #[serde(rename = "baseUrl")]
    base_url: &'a str,
    api: &'static str,
    #[serde(rename = "apiKey")]
    api_key: String,
    models: Vec<ModelEntry<'a>>,
}

#[derive(Serialize)]
struct ModelEntry<'a> {
    id: &'a str,
    name: &'a str,
    reasoning: bool,
    #[serde(rename = "contextWindow")]
    context_window: u64,
    #[serde(rename = "maxTokens")]
    max_tokens: u64,
}

fn provider_entry(endpoint: &Endpoint, api_key: String) -> Value {
    let entry = ProviderEntry {
        name: "Fonsviva",
        base_url: &endpoint.base_url,
        api: "openai-completions",
        api_key,
        models: endpoint
            .models
            .iter()
            .map(|model| ModelEntry {
                id: &model.id,
                name: if model.name.is_empty() {
                    &model.id
                } else {
                    &model.name
                },
                reasoning: model.reasoning,
                context_window: model.context_window,
                max_tokens: model.max_tokens,
            })
            .collect(),
    };
    serde_json::to_value(entry).unwrap_or_default()
}

type T3Entry = (Vec<String>, Value, Option<Option<bool>>);

/// Picks the T3 Code instance to enable or add: an existing `pi` driver
/// instance is enabled in place; otherwise a new `pi` instance is added.
fn t3_entry(
    doc: &json_edit::Doc,
    record: Option<&Record>,
    pi_binary: Option<&Path>,
) -> Result<T3Entry, WireError> {
    if let Some(record) = record {
        let mut entry = record.entry.clone();
        if record.t3_previous_enabled.is_some() {
            entry = doc.get(&path_refs(&record.entry_path)).unwrap_or(entry);
            entry["enabled"] = Value::Bool(true);
        }
        return Ok((record.entry_path.clone(), entry, record.t3_previous_enabled));
    }
    let instances = doc
        .get(&["providerInstances"])
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some((id, instance)) = instances
        .iter()
        .find(|(_, instance)| instance["driver"] == "pi")
    {
        let previous = instance.get("enabled").and_then(Value::as_bool);
        let mut enabled = instance.clone();
        enabled["enabled"] = Value::Bool(true);
        return Ok((
            vec!["providerInstances".to_string(), id.clone()],
            enabled,
            Some(previous),
        ));
    }
    let id = ["pi", "fonsviva-pi"]
        .into_iter()
        .find(|id| !instances.contains_key(*id))
        .unwrap_or("fonsviva-pi");
    let mut entry = json!({"driver": "pi", "enabled": true});
    if let Some(binary) = pi_binary {
        entry["config"] = json!({"binaryPath": binary.to_string_lossy()});
    }
    Ok((
        vec!["providerInstances".to_string(), id.to_string()],
        entry,
        None,
    ))
}

/// The diff to show when Fonsviva's entry is in the file but the file is not
/// what Fonsviva last wrote; `None` when the target has not drifted.
fn drift(
    target: Target,
    record: Option<&Record>,
    doc: &json_edit::Doc,
    text: &str,
) -> Option<String> {
    match record {
        Some(record) => {
            doc.get(&path_refs(&record.entry_path))?;
            if record.written_sha256.as_deref() == Some(fsio::sha256(text.as_bytes()).as_str()) {
                return None;
            }
            Some(diff(record.written.as_deref().unwrap_or_default(), text))
        }
        None if target == Target::T3code => None,
        None => {
            doc.get(&["providers", PROVIDER_ID])?;
            Some(diff(&strip(doc, target, None).unwrap_or_default(), text))
        }
    }
}

/// Removes Fonsviva's entries from `doc`, keeping everything else.
fn strip(doc: &json_edit::Doc, target: Target, record: Option<&Record>) -> Option<String> {
    let (entry_path, created_parents, previous_enabled) = match record {
        Some(record) => (
            record.entry_path.clone(),
            record.created_parents.clone(),
            record.t3_previous_enabled,
        ),
        None if target == Target::T3code => return None,
        None => (
            vec!["providers".to_string(), PROVIDER_ID.to_string()],
            Vec::new(),
            None,
        ),
    };
    let refs = path_refs(&entry_path);
    doc.get(&refs)?;
    if let Some(previous) = previous_enabled {
        let enabled_path: Vec<&str> = refs.iter().copied().chain(["enabled"]).collect();
        return match previous {
            Some(value) => doc.upsert(&enabled_path, &value).ok(),
            None => doc.remove(&enabled_path),
        };
    }
    let mut text = doc.remove(&refs)?;
    for depth in (1..entry_path.len()).rev() {
        let parent = &refs[..depth];
        if !created_parents.iter().any(|key| key == parent[depth - 1]) {
            break;
        }
        let parsed = json_edit::Doc::parse(&text).ok()?;
        if parsed
            .get(parent)
            .and_then(|value| value.as_object().map(serde_json::Map::is_empty))
            != Some(true)
        {
            break;
        }
        text = parsed.remove(parent)?;
    }
    Some(text)
}

fn validate(target: Target, endpoint: &Endpoint) -> Result<(), WireError> {
    let base = endpoint.base_url.trim();
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(WireError::InvalidEndpoint(format!(
            "base URL must start with http:// or https:// (got \"{base}\")"
        )));
    }
    if matches!(target, Target::Pi | Target::Eukhe) {
        if endpoint.models.is_empty() {
            return Err(WireError::InvalidEndpoint(
                "pick at least one model to wire".to_string(),
            ));
        }
        if let Some(model) = endpoint
            .models
            .iter()
            .find(|model| model.id.trim().is_empty())
        {
            return Err(WireError::InvalidEndpoint(format!(
                "model \"{}\" has an empty id",
                model.name
            )));
        }
    }
    Ok(())
}

fn edit_error(path: &Path, error: EditError) -> WireError {
    match error {
        EditError::NotJson => WireError::NotPlainJson {
            path: path.to_path_buf(),
        },
        other => WireError::Unexpected {
            path: path.to_path_buf(),
            problem: other.to_string(),
        },
    }
}

fn path_refs(path: &[String]) -> Vec<&str> {
    path.iter().map(String::as_str).collect()
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// A changed-lines diff. Lines that differ only by a trailing comma are
/// dropped, and values of key-like members are redacted, so the diff never
/// shows a user's secret.
pub fn diff(before: &str, after: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    if a.len() * b.len() > 4_000_000 {
        return "(file too large to diff)".to_string();
    }
    let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut removed = Vec::new();
    let mut added = Vec::new();
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
            added.push(b[j]);
            j += 1;
        } else {
            removed.push(a[i]);
            i += 1;
        }
    }
    let same =
        |x: &str, y: &str| x.trim_end().trim_end_matches(',') == y.trim_end().trim_end_matches(',');
    let mut out = String::new();
    for line in &removed {
        if !added.iter().any(|other| same(line, other)) {
            out.push_str(&format!("-{}\n", redact(line)));
        }
    }
    for line in &added {
        if !removed.iter().any(|other| same(line, other)) {
            out.push_str(&format!("+{}\n", redact(line)));
        }
    }
    out
}

fn redact(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let secretish = ["key\"", "token\"", "secret\"", "password\""]
        .iter()
        .any(|marker| lower.contains(marker));
    match line.find(':') {
        Some(colon)
            if secretish
                && !line[colon..].contains("\"!")
                && !line[colon..].contains(&format!("\"{NO_KEY}\"")) =>
        {
            format!(
                "{}: \"<redacted>\"{}",
                &line[..colon],
                if line.trim_end().ends_with(',') {
                    ","
                } else {
                    ""
                }
            )
        }
        _ => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_literal_keys_but_shows_references() {
        assert_eq!(
            redact("  \"apiKey\": \"sk-live-123\","),
            "  \"apiKey\": \"<redacted>\","
        );
        assert_eq!(
            redact("  \"apiKey\": \"!secret-tool lookup service fonsviva username wire-pi\""),
            "  \"apiKey\": \"!secret-tool lookup service fonsviva username wire-pi\""
        );
        assert_eq!(
            redact("  \"baseUrl\": \"http://x\""),
            "  \"baseUrl\": \"http://x\""
        );
    }

    #[test]
    fn endpoint_debug_hides_key() {
        let endpoint = Endpoint {
            base_url: "http://x/v1".into(),
            api_key: Some(SecretString::from("sk-hidden")),
            models: vec![],
        };
        assert!(!format!("{endpoint:?}").contains("sk-hidden"));
    }
}
