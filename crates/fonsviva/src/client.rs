//! Finds the daemon for this data dir (starting one if asked) and talks to it.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::time::{Duration, Instant};

use crate::auth;
use crate::discovery::{self, API, DaemonInfo, Layout, SCHEMA};
use fonsviva_core::paths;

pub struct Client {
    pub base: String,
    pub info: DaemonInfo,
    /// owner.token, for /api/v1
    owner: String,
    /// cli.key, for /v1
    app: String,
    http: reqwest::Client,
}

/// Why an attach was refused, or `None` when the daemon is ours.
pub async fn verify(
    http: &reqwest::Client,
    l: &Layout,
    info: &DaemonInfo,
) -> Result<Option<String>> {
    let nonce = auth::random_hex(16);
    let url = format!("http://127.0.0.1:{}/health?challenge={nonce}", info.port);
    let v: Value = http
        .get(url)
        .timeout(Duration::from_secs(3))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let want_id = std::fs::read_to_string(l.data_dir_id()).unwrap_or_default();
    let proof = auth::hmac_sha256_hex(info.secret.as_bytes(), nonce.as_bytes());
    Ok(if v["proof"].as_str() != Some(proof.as_str()) {
        Some(format!(
            "The program on port {} could not prove it owns this data dir.",
            info.port
        ))
    } else if v["data_dir_id"].as_str() != Some(want_id.trim()) {
        Some(format!(
            "The daemon on port {} serves a different data dir.",
            info.port
        ))
    } else if v["channel"].as_str() != Some(paths::CHANNEL) {
        Some(format!(
            "The daemon on port {} is the {} channel, not {}.",
            info.port,
            v["channel"],
            paths::CHANNEL
        ))
    } else if v["api"].as_u64() != Some(API as u64) || v["schema"].as_u64() != Some(SCHEMA as u64) {
        Some(format!(
            "The daemon on port {} speaks api {} schema {}; this CLI needs api {API} schema {SCHEMA}. Restart it with `fonsviva stop --daemon`.",
            info.port, v["api"], v["schema"]
        ))
    } else {
        None
    })
}

impl Client {
    /// Attaches to the running daemon. With `start`, launches one when none answers.
    pub async fn attach(start: bool) -> Result<Client> {
        let l = Layout::current();
        let http = reqwest::Client::builder().build()?;
        if let Some(info) = discovery::read_info(&l) {
            match verify(&http, &l, &info).await {
                Ok(None) => return Client::new(&l, info, http),
                Ok(Some(why)) => bail!("{why}"),
                Err(_) => {} // stale daemon.json
            }
        }
        if !start {
            bail!(
                "The Fonsviva daemon is not running. Start it with `fonsviva serve` (or any command that needs it, such as `fonsviva run`)."
            );
        }
        spawn_daemon(&l)?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(info) = discovery::read_info(&l)
                && let Ok(None) = verify(&http, &l, &info).await
            {
                return Client::new(&l, info, http);
            }
            if Instant::now() > deadline {
                let log = paths::logs_dir().join("daemon.log");
                let tail = std::fs::read_to_string(&log).unwrap_or_default();
                let tail: Vec<&str> = tail.lines().rev().take(5).collect();
                bail!(
                    "The daemon did not start. Last lines of {}:\n{}",
                    log.display(),
                    tail.into_iter().rev().collect::<Vec<_>>().join("\n")
                );
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    fn new(l: &Layout, info: DaemonInfo, http: reqwest::Client) -> Result<Client> {
        let owner = std::fs::read_to_string(l.owner_token()).context("read owner.token")?;
        let app = std::fs::read_to_string(l.cli_key()).context("read cli.key")?;
        Ok(Client {
            base: format!("http://127.0.0.1:{}", info.port),
            info,
            owner: owner.trim().to_string(),
            app: app.trim().to_string(),
            http,
        })
    }

    async fn send(&self, rb: reqwest::RequestBuilder) -> Result<Value> {
        let r = rb.send().await.context("the daemon didn't answer")?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let e = &v["error"];
            let mut msg = e["message"]
                .as_str()
                .unwrap_or("request failed")
                .to_string();
            if let Some(f) = e["fix"].as_str() {
                msg.push_str("\n  ");
                msg.push_str(f);
            }
            bail!("{msg}");
        }
        Ok(v)
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.send(
            self.http
                .get(format!("{}/api/v1{path}", self.base))
                .bearer_auth(&self.owner),
        )
        .await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.send(
            self.http
                .post(format!("{}/api/v1{path}", self.base))
                .bearer_auth(&self.owner)
                .json(&body),
        )
        .await
    }

    pub async fn delete(&self, path: &str, body: Value) -> Result<Value> {
        self.send(
            self.http
                .delete(format!("{}/api/v1{path}", self.base))
                .bearer_auth(&self.owner)
                .json(&body),
        )
        .await
    }

    /// An OpenAI-compatible request with the CLI's app key.
    pub fn v1(&self, path: &str, body: &Value) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}/v1{path}", self.base))
            .bearer_auth(&self.app)
            .json(body)
    }

    /// Waits for a job, drawing a progress line.
    pub async fn wait_job(&self, id: &str, label: &str) -> Result<Value> {
        loop {
            let jobs = self.get("/jobs").await?;
            let j = jobs["jobs"]
                .as_array()
                .and_then(|a| a.iter().find(|j| j["id"] == id))
                .cloned()
                .unwrap_or_default();
            let (done, total) = (
                j["done"].as_u64().unwrap_or(0),
                j["total"].as_u64().unwrap_or(0),
            );
            eprint!("\r  {label}: {}", progress_words(done, total));
            match j["state"].as_str() {
                Some("done") => {
                    eprintln!();
                    return Ok(j);
                }
                Some("failed") => {
                    eprintln!();
                    bail!(
                        "{label} failed: {}",
                        j["error"].as_str().unwrap_or("unknown error")
                    );
                }
                Some("cancelled") => {
                    eprintln!();
                    bail!("{label} was cancelled.");
                }
                _ => tokio::time::sleep(Duration::from_millis(400)).await,
            }
        }
    }
}

pub fn progress_words(done: u64, total: u64) -> String {
    use fonsviva_core::util::human_bytes;
    if total > 0 {
        format!(
            "{} of {} ({}%)   ",
            human_bytes(done),
            human_bytes(total),
            done * 100 / total
        )
    } else {
        format!("{}   ", human_bytes(done))
    }
}

/// Starts `fonsviva serve` detached from this terminal, logging to logs/daemon.log.
fn spawn_daemon(l: &Layout) -> Result<()> {
    let logs = paths::logs_dir();
    std::fs::create_dir_all(&logs)?;
    std::fs::create_dir_all(&l.root)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join("daemon.log"))?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x8;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().context("start the daemon")?;
    Ok(())
}
