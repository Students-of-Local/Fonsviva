//! Finds the Fonsviva daemon or starts it, following "Discovery and ownership" in docs/API.md.
//! The window never talks to a daemon whose `data_dir_id` differs from this data dir's.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// Set by the release workflows; local builds are dev in debug and stable in release.
pub const CHANNEL: &str = match option_env!("FONSVIVA_CHANNEL") {
    Some(c) => c,
    None if cfg!(debug_assertions) => "dev",
    None => "stable",
};

const START_TIMEOUT: Duration = Duration::from_secs(20);

/// The API version this app speaks.
const API: u32 = 1;

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct DaemonFile {
    pub pid: u32,
    pub port: u16,
    pub channel: String,
    pub data_dir_id: String,
    pub secret: String,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct Health {
    pub channel: String,
    pub api: u32,
    pub data_dir_id: String,
    #[serde(default)]
    pub proof: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct DaemonInfo {
    pub base_url: String,
    pub owner_token: String,
    pub app_key: String,
}

/// The data dir for this channel: `FONSVIVA_HOME`, else the per-OS default.
pub fn data_dir(home_override: Option<PathBuf>, channel: &str) -> Option<PathBuf> {
    if let Some(h) = home_override.filter(|h| !h.as_os_str().is_empty()) {
        return Some(h);
    }
    let dev = channel == "dev";
    if cfg!(target_os = "linux") {
        dirs::data_dir().map(|d| d.join(if dev { "fonsviva-dev" } else { "fonsviva" }))
    } else if cfg!(target_os = "windows") {
        dirs::data_local_dir().map(|d| d.join(if dev { "Fonsviva Dev" } else { "Fonsviva" }))
    } else {
        dirs::data_dir().map(|d| d.join(if dev { "Fonsviva Dev" } else { "Fonsviva" }))
    }
}

pub fn read_daemon_file(dir: &Path) -> Option<DaemonFile> {
    let text = std::fs::read_to_string(dir.join("daemon.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What the daemon must answer: hex HMAC-SHA256(secret, nonce).
pub fn proof(secret: &str, nonce: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(nonce.as_bytes());
    hex(&mac.finalize().into_bytes())
}

/// What the daemon on `port` says about itself when challenged, if it answers quickly.
fn health(port: u16, nonce: &str) -> Option<Health> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(800)))
        .build()
        .into();
    let mut res = agent
        .get(format!("http://127.0.0.1:{port}/health?challenge={nonce}"))
        .call()
        .ok()?;
    res.body_mut().read_json::<Health>().ok()
}

enum Attach {
    Ready(DaemonFile),
    /// Nothing that proves it owns this data dir answers, so start one.
    Absent,
    /// The owner is real but cannot be used by this app; the message says why.
    Refused(String),
}

/// The daemon serving `dir`: `daemon.json` names it, `/health` proves it holds that file's
/// secret, and it runs this app's channel and API.
fn attach(dir: &Path, probe: &dyn Fn(u16, &str) -> Option<Health>) -> Attach {
    let Some(file) = read_daemon_file(dir) else {
        return Attach::Absent;
    };
    let mut raw = [0u8; 16];
    if getrandom::fill(&mut raw).is_err() {
        return Attach::Refused("This computer could not make a random number.".into());
    }
    let nonce = hex(&raw);
    let Some(h) = probe(file.port, &nonce) else {
        return Attach::Absent;
    };
    if h.data_dir_id != file.data_dir_id || h.proof != proof(&file.secret, &nonce) {
        return Attach::Absent;
    }
    if h.channel != CHANNEL {
        return Attach::Refused(format!(
            "Fonsviva {} is already running with this data. Quit it first, or open Fonsviva {}.",
            h.channel, h.channel
        ));
    }
    if h.api != API {
        return Attach::Refused(format!(
            "The running Fonsviva engine speaks API {} and this app speaks {API}. Update Fonsviva.",
            h.api
        ));
    }
    Attach::Ready(file)
}

/// The bundled `fonsviva` binary: next to this app (Tauri's sidecar spot), or `FONSVIVA_BIN`.
fn daemon_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FONSVIVA_BIN") {
        return Some(PathBuf::from(p));
    }
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) {
        "fonsviva.exe"
    } else {
        "fonsviva"
    };
    let p = exe.parent()?.join(name);
    p.exists().then_some(p)
}

fn spawn_daemon(bin: &Path, dir: &Path) -> std::io::Result<()> {
    let mut cmd = Command::new(bin);
    cmd.arg("serve")
        .env("FONSVIVA_HOME", dir)
        .env("FONSVIVA_CHANNEL", CHANNEL)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    // The daemon outlives the window on purpose: Local Studio and wired apps keep using it.
    cmd.spawn().map(|_| ())
}

fn read_secret(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Attaches to the running daemon, or starts one and waits for it.
pub fn ensure() -> Result<DaemonInfo, String> {
    let dir = data_dir(
        std::env::var_os("FONSVIVA_HOME").map(PathBuf::from),
        CHANNEL,
    )
    .ok_or("Fonsviva could not find a folder for its data on this computer.")?;
    let file = match attach(&dir, &health) {
        Attach::Ready(f) => f,
        Attach::Refused(why) => return Err(why),
        Attach::Absent => {
            let bin = daemon_binary().ok_or(
                "The Fonsviva engine is missing from this install. Reinstall Fonsviva to restore it.",
            )?;
            spawn_daemon(&bin, &dir)
                .map_err(|e| format!("The Fonsviva engine did not start: {e}"))?;
            let until = Instant::now() + START_TIMEOUT;
            loop {
                match attach(&dir, &health) {
                    Attach::Ready(f) => break f,
                    Attach::Refused(why) => return Err(why),
                    Attach::Absent => {}
                }
                if Instant::now() > until {
                    return Err(format!(
                        "The Fonsviva engine did not answer within {} seconds. Run `fonsviva doctor` to see why.",
                        START_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        }
    };
    Ok(DaemonInfo {
        base_url: format!("http://127.0.0.1:{}", file.port),
        owner_token: read_secret(&dir, "owner.token"),
        app_key: read_secret(&dir, "desktop.key"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, port: u16, id: &str) {
        std::fs::write(
            dir.join("daemon.json"),
            format!(r#"{{"pid":1,"port":{port},"version":"0.1.0","channel":"{CHANNEL}","schema":1,"data_dir_id":"{id}","secret":"s3","started_at":0}}"#),
        )
        .unwrap();
    }

    /// A daemon that holds `secret` and owns data dir `id`.
    fn daemon(
        id: &'static str,
        secret: &'static str,
        channel: &'static str,
        api: u32,
    ) -> impl Fn(u16, &str) -> Option<Health> {
        move |_, nonce| {
            Some(Health {
                channel: channel.into(),
                api,
                data_dir_id: id.into(),
                proof: proof(secret, nonce),
            })
        }
    }

    fn port(a: Attach) -> Option<u16> {
        match a {
            Attach::Ready(f) => Some(f.port),
            _ => None,
        }
    }

    #[test]
    fn home_override_wins_and_dev_is_separate() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(data_dir(Some(t.path().into()), "stable").unwrap(), t.path());
        let stable = data_dir(None, "stable").unwrap();
        let dev = data_dir(None, "dev").unwrap();
        assert_ne!(stable, dev);
        assert_eq!(data_dir(None, "nightly").unwrap(), stable);
    }

    #[test]
    fn attaches_only_to_the_daemon_holding_the_secret() {
        let t = tempfile::tempdir().unwrap();
        assert!(
            matches!(
                attach(t.path(), &daemon("a", "s3", CHANNEL, API)),
                Attach::Absent
            ),
            "no daemon.json"
        );
        write(t.path(), 7342, "a");
        assert_eq!(
            port(attach(t.path(), &daemon("a", "s3", CHANNEL, API))),
            Some(7342)
        );
        assert!(
            matches!(
                attach(t.path(), &daemon("b", "s3", CHANNEL, API)),
                Attach::Absent
            ),
            "another data dir"
        );
        assert!(
            matches!(
                attach(t.path(), &daemon("a", "guess", CHANNEL, API)),
                Attach::Absent
            ),
            "copied the public id only"
        );
        assert!(
            matches!(attach(t.path(), &|_, _| None), Attach::Absent),
            "nothing answers"
        );
    }

    #[test]
    fn refuses_another_channel_or_api() {
        let t = tempfile::tempdir().unwrap();
        write(t.path(), 7342, "a");
        let other = if CHANNEL == "nightly" {
            "stable"
        } else {
            "nightly"
        };
        assert!(matches!(
            attach(t.path(), &daemon("a", "s3", other, API)),
            Attach::Refused(_)
        ));
        assert!(matches!(
            attach(t.path(), &daemon("a", "s3", CHANNEL, API + 1)),
            Attach::Refused(_)
        ));
    }

    #[test]
    fn proof_matches_a_known_vector() {
        // RFC 4231 test case 2.
        assert_eq!(
            proof("Jefe", "what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn bad_daemon_file_is_ignored() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("daemon.json"), "{not json").unwrap();
        assert!(read_daemon_file(t.path()).is_none());
    }
}
