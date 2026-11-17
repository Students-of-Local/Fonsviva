//! Data dir ownership and discovery (docs/API.md "Discovery and ownership").

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};

use crate::auth::{random_hex, read_or_create, write_private};
use fonsviva_core::paths;

/// This build's state schema for `state/<channel>/`.
pub const SCHEMA: u32 = 1;
pub const API: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonInfo {
    pub pid: u32,
    pub port: u16,
    pub version: String,
    pub channel: String,
    pub schema: u32,
    pub data_dir_id: String,
    pub secret: String,
    pub started_at: String,
}

/// Fixed file names in the data dir.
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn current() -> Layout {
        Layout {
            root: paths::data_dir(),
        }
    }
    pub fn daemon_json(&self) -> PathBuf {
        self.root.join("daemon.json")
    }
    pub fn lock(&self) -> PathBuf {
        self.root.join("daemon.lock")
    }
    pub fn data_dir_id(&self) -> PathBuf {
        self.root.join("data_dir_id")
    }
    pub fn owner_token(&self) -> PathBuf {
        self.root.join("owner.token")
    }
    pub fn control_token(&self) -> PathBuf {
        self.root.join("control.token")
    }
    pub fn desktop_key(&self) -> PathBuf {
        self.root.join("desktop.key")
    }
    pub fn cli_key(&self) -> PathBuf {
        self.root.join("cli.key")
    }
    pub fn keys(&self) -> PathBuf {
        self.root.join("shared").join("keys.json")
    }
    pub fn state(&self) -> PathBuf {
        self.root.join("state").join(paths::CHANNEL)
    }
    pub fn tokens(&self) -> PathBuf {
        self.state().join("tokens.json")
    }
    pub fn schema(&self) -> PathBuf {
        self.state().join("schema.json")
    }
}

pub fn data_dir_id(l: &Layout) -> Result<String> {
    std::fs::create_dir_all(&l.root)?;
    read_or_create(&l.data_dir_id(), || format!("dd_{}", random_hex(12)))
}

/// Refuses state newer than this build knows; writes ours the first time.
pub fn check_schema(l: &Layout) -> Result<()> {
    let p = l.schema();
    if let Ok(s) = std::fs::read_to_string(&p) {
        let v: serde_json::Value = serde_json::from_str(&s).context("parse schema.json")?;
        let have = v["schema"].as_u64().unwrap_or(0) as u32;
        if have > SCHEMA {
            bail!(
                "This data dir's {} state is schema {have}, written by a newer Fonsviva. This build knows schema {SCHEMA}; update Fonsviva to a version that supports schema {have}.",
                paths::CHANNEL
            );
        }
        return Ok(());
    }
    std::fs::create_dir_all(l.state())?;
    std::fs::write(&p, serde_json::json!({"schema": SCHEMA}).to_string())?;
    Ok(())
}

/// Takes the data dir lock. The file handle must live as long as the daemon.
pub fn lock(l: &Layout) -> Result<File> {
    std::fs::create_dir_all(&l.root)?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(l.lock())?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(_) => {
            let who = read_info(l)
                .map(|i| format!(" (pid {}, port {})", i.pid, i.port))
                .unwrap_or_default();
            bail!(
                "Another Fonsviva daemon already owns {}{who}. Use it, or stop it with `fonsviva stop --daemon`.",
                l.root.display()
            )
        }
    }
}

pub fn read_info(l: &Layout) -> Option<DaemonInfo> {
    serde_json::from_str(&std::fs::read_to_string(l.daemon_json()).ok()?).ok()
}

pub fn write_info(l: &Layout, i: &DaemonInfo) -> Result<()> {
    write_private(&l.daemon_json(), &serde_json::to_vec_pretty(i)?)
}

/// Names the process listening on a port, for the "port taken" message.
pub fn port_holder(port: u16) -> Option<String> {
    if cfg!(target_os = "linux") {
        return linux_port_holder(Path::new("/proc"), port);
    }
    if cfg!(windows) {
        let out = fonsviva_core::hardware::run_probe("netstat", &["-ano", "-p", "TCP"])?;
        let pid = parse_netstat_pid(&out, port)?;
        return Some(format!(
            "{} (pid {pid})",
            fonsviva_core::stats::process_name(pid).unwrap_or_else(|| "a program".into())
        ));
    }
    let out = fonsviva_core::hardware::run_probe(
        "lsof",
        &["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpc"],
    )?;
    parse_lsof(&out)
}

/// `lsof -Fpc` prints `p<pid>` then `c<command>`.
pub fn parse_lsof(out: &str) -> Option<String> {
    let pid = out.lines().find_map(|l| l.strip_prefix('p'))?;
    let cmd = out
        .lines()
        .find_map(|l| l.strip_prefix('c'))
        .unwrap_or("a program");
    Some(format!("{cmd} (pid {pid})"))
}

pub fn parse_netstat_pid(out: &str, port: u16) -> Option<u32> {
    out.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() >= 5
            && f[0] == "TCP"
            && f[1].ends_with(&format!(":{port}"))
            && f[3] == "LISTENING")
            .then(|| f[4].parse().ok())?
    })
}

/// Finds the listening socket's inode in /proc/net/tcp{,6}, then the process
/// whose fd points at it.
pub fn linux_port_holder(proc_root: &Path, port: u16) -> Option<String> {
    let want = format!(":{port:04X}");
    let mut inode = None;
    for f in ["net/tcp", "net/tcp6"] {
        let s = std::fs::read_to_string(proc_root.join(f)).unwrap_or_default();
        for l in s.lines().skip(1) {
            let c: Vec<&str> = l.split_whitespace().collect();
            if c.len() > 9 && c[1].ends_with(&want) && c[3] == "0A" {
                inode = Some(c[9].to_string());
            }
        }
    }
    let target = format!("socket:[{}]", inode?);
    for e in std::fs::read_dir(proc_root).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(e.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if std::fs::read_link(fd.path()).is_ok_and(|l| l.to_string_lossy() == target) {
                let comm = std::fs::read_to_string(e.path().join("comm")).unwrap_or_default();
                return Some(format!("{} (pid {name})", comm.trim()));
            }
        }
    }
    Some("another user's program".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_newer_than_known_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout {
            root: d.path().to_path_buf(),
        };
        check_schema(&l).unwrap();
        assert!(l.schema().exists());
        std::fs::write(l.schema(), r#"{"schema": 99}"#).unwrap();
        let e = check_schema(&l).unwrap_err().to_string();
        assert!(
            e.contains("schema 99") && e.contains("update Fonsviva"),
            "{e}"
        );
    }

    #[test]
    fn one_daemon_per_data_dir() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout {
            root: d.path().to_path_buf(),
        };
        let held = lock(&l).unwrap();
        assert!(lock(&l).is_err());
        drop(held);
        assert!(lock(&l).is_ok());
    }

    #[test]
    fn data_dir_id_is_stable() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout {
            root: d.path().to_path_buf(),
        };
        let a = data_dir_id(&l).unwrap();
        assert_eq!(a, data_dir_id(&l).unwrap());
        assert!(a.starts_with("dd_"));
    }

    #[test]
    fn port_holder_parsers() {
        assert_eq!(
            parse_lsof("p812\ncollama\n").as_deref(),
            Some("ollama (pid 812)")
        );
        let ns = "  Proto  Local Address          Foreign Address        State           PID\n  TCP    127.0.0.1:7341         0.0.0.0:0              LISTENING       4410\n";
        assert_eq!(parse_netstat_pid(ns, 7341), Some(4410));
        assert_eq!(parse_netstat_pid(ns, 7342), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn finds_who_holds_a_port_on_linux() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let who = linux_port_holder(Path::new("/proc"), port).unwrap();
        assert!(
            who.contains(&format!("pid {}", std::process::id())),
            "{who}"
        );
    }
}
