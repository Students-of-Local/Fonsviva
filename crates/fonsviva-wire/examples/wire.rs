//! Drives the wire writers the way the daemon will, for manual checks.
//!
//! ```text
//! FONSVIVA_HOME=/tmp/scratch/fonsviva PI_CODING_AGENT_DIR=/tmp/scratch/pi \
//! FONSVIVA_WIRE_BASE_URL=http://127.0.0.1:11434/v1 FONSVIVA_WIRE_MODEL=some/model \
//!   cargo run -p fonsviva-wire --example wire -- apply pi
//! ```
//!
//! Commands: `status [target]`, `apply <target> [--confirm]`,
//! `revert <target> [--confirm]`.
//!
//! The example never touches the OS keychain. `FONSVIVA_WIRE_KEY_FILE`
//! names a file holding the app key to wire; without it the endpoint has no
//! key. Keys are kept in memory, or, with `FONSVIVA_WIRE_KEY_DIR`, as 0600
//! files in that scratch directory: a stand-in for the keychain on a machine
//! with no Secret Service, which a scratch `secret-tool` can answer from.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use fonsviva_wire::{Endpoint, KeyStore, MemoryKeys, Model, Target, Wire, WireEnv, WireError};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;

/// One 0600 file per account in a scratch directory.
struct DirKeys(PathBuf);

impl KeyStore for DirKeys {
    fn set(&self, account: &str, secret: &SecretString) -> Result<(), String> {
        use std::io::Write;
        std::fs::create_dir_all(&self.0).map_err(|error| error.to_string())?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(self.0.join(account))
            .map_err(|error| error.to_string())?;
        file.write_all(secret.expose_secret().as_bytes())
            .map_err(|error| error.to_string())
    }

    fn get(&self, account: &str) -> Result<Option<SecretString>, String> {
        match std::fs::read_to_string(self.0.join(account)) {
            Ok(secret) => Ok(Some(SecretString::from(secret))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        match std::fs::remove_file(self.0.join(account)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.to_string()),
            _ => Ok(()),
        }
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn print(value: &impl Serialize) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("reports serialize")
    );
}

fn fail(error: &WireError) -> ExitCode {
    let body = serde_json::json!({ "code": error.code(), "message": error.to_string(), "fix": error.fix() });
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&body).expect("errors serialize")
    );
    ExitCode::FAILURE
}

fn endpoint() -> Endpoint {
    let model = env("FONSVIVA_WIRE_MODEL").unwrap_or_else(|| "local".into());
    let context_window = env("FONSVIVA_WIRE_CONTEXT")
        .and_then(|value| value.parse().ok())
        .unwrap_or(32_768);
    Endpoint {
        base_url: env("FONSVIVA_WIRE_BASE_URL")
            .unwrap_or_else(|| "http://127.0.0.1:7341/v1".into()),
        api_key: env("FONSVIVA_WIRE_KEY_FILE")
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|key| SecretString::from(key.trim().to_string())),
        models: vec![Model {
            id: model.clone(),
            name: model,
            context_window,
            max_tokens: 8_192,
            reasoning: false,
        }],
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(home) = env("FONSVIVA_HOME") else {
        eprintln!("Set FONSVIVA_HOME to a scratch directory first.");
        return ExitCode::FAILURE;
    };
    let keys: Arc<dyn KeyStore> = match env("FONSVIVA_WIRE_KEY_DIR") {
        Some(dir) => Arc::new(DirKeys(PathBuf::from(dir))),
        None => Arc::new(MemoryKeys::default()),
    };
    let wire = Wire::new(
        WireEnv::from_process(PathBuf::from(home).join("shared")),
        keys,
    );
    let confirm = args.iter().any(|arg| arg == "--confirm");
    let target = match args
        .get(1)
        .filter(|arg| !arg.starts_with("--"))
        .map(|arg| arg.parse::<Target>())
    {
        Some(Ok(target)) => Some(target),
        Some(Err(message)) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
        None => None,
    };
    let result = match (args.first().map(String::as_str), target) {
        (Some("status"), None) => {
            for status in wire.status_all() {
                match status {
                    Ok(status) => print(&status),
                    Err(error) => return fail(&error),
                }
            }
            return ExitCode::SUCCESS;
        }
        (Some("status"), Some(target)) => wire.status(target).map(|status| print(&status)),
        (Some("apply"), Some(target)) => wire
            .apply(target, &endpoint(), confirm)
            .map(|report| print(&report)),
        (Some("revert"), Some(target)) => wire.revert(target, confirm).map(|report| print(&report)),
        _ => {
            eprintln!(
                "usage: wire status [target] | apply <target> [--confirm] | revert <target> [--confirm]"
            );
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(&error),
    }
}
