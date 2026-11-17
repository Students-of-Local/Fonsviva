//! Where Fonsviva keeps its data, per docs/API.md.

use std::path::PathBuf;

fn env_path(k: &str) -> Option<PathBuf> {
    std::env::var_os(k)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

pub fn home_dir() -> PathBuf {
    env_path("HOME")
        .or_else(|| env_path("USERPROFILE"))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Release channel, fixed at build time (`FONSVIVA_CHANNEL`). Builds from source are dev.
pub const CHANNEL: &str = match option_env!("FONSVIVA_CHANNEL") {
    Some(c) => c,
    None => "dev",
};

/// Version, fixed at build time (`FONSVIVA_VERSION`, such as `0.1.0-dev.202611171200+abc1234`
/// from the release workflow). Builds from source use the crate version.
pub const VERSION: &str = match option_env!("FONSVIVA_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => env!("CARGO_PKG_VERSION"),
};

pub fn is_dev() -> bool {
    !matches!(CHANNEL, "stable" | "nightly")
}

/// The daemon's fixed port: 7341 for stable and nightly, 7342 for dev.
/// `FONSVIVA_PORT` overrides it; 0 means pick a free port.
pub fn port() -> u16 {
    if let Some(p) = std::env::var("FONSVIVA_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
    {
        return p;
    }
    if is_dev() { 7342 } else { 7341 }
}

/// `FONSVIVA_HOME`, else the per-OS default. Dev builds never share stable's dir.
pub fn data_dir() -> PathBuf {
    if let Some(p) = env_path("FONSVIVA_HOME") {
        return p;
    }
    if cfg!(target_os = "macos") {
        let name = if is_dev() { "Fonsviva Dev" } else { "Fonsviva" };
        home_dir().join("Library/Application Support").join(name)
    } else if cfg!(windows) {
        env_path("LOCALAPPDATA")
            .unwrap_or_else(|| home_dir().join("AppData/Local"))
            .join(if is_dev() { "Fonsviva Dev" } else { "Fonsviva" })
    } else {
        env_path("XDG_DATA_HOME")
            .unwrap_or_else(|| home_dir().join(".local/share"))
            .join(if is_dev() { "fonsviva-dev" } else { "fonsviva" })
    }
}

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

pub fn engines_dir() -> PathBuf {
    data_dir().join("engines")
}

/// Per-channel state (settings, keys, tokens, benchmarks). Only `models/` and
/// `engines/` are shared between channels.
pub fn state_dir() -> PathBuf {
    data_dir().join("state").join(CHANNEL)
}

pub fn bench_dir() -> PathBuf {
    state_dir().join("bench")
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}
