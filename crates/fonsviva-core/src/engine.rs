//! Engines behind one trait: llama.cpp (`llama-server`, downloaded and run
//! by Fonsviva) and Ollama (adopted when it is already running).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::download::{self, Cancel, Progress};
use crate::hardware::Hardware;
use crate::store::Installed;

/// The llama.cpp release Fonsviva is tested with. `FONSVIVA_LLAMACPP_TAG` overrides it.
pub const LLAMACPP_TAG: &str = "b12477";

pub fn llamacpp_tag() -> String {
    std::env::var("FONSVIVA_LLAMACPP_TAG")
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| LLAMACPP_TAG.to_string())
}

/// What to load and how. Every field is explicit; nothing falls back to an
/// engine default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadSpec {
    pub model: Installed,
    pub context: u64,
    pub gpu_layers: u32,
    #[serde(default)]
    pub embeddings: bool,
}

/// A loaded model, with the settings the engine reports back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Loaded {
    /// llama.cpp or ollama
    pub engine: String,
    /// Why this engine, when it is not the one asked for.
    pub engine_note: Option<String>,
    /// OpenAI-compatible base for llama.cpp, Ollama's base URL for Ollama.
    pub base_url: String,
    pub port: u16,
    pub pid: Option<u32>,
    /// Context the engine actually has, read back from it.
    pub context: u64,
    pub gpu_layers: u32,
    pub embeddings: bool,
    pub log: Option<PathBuf>,
}

/// One engine. `load` returns a handle the caller keeps; `unload` ends it.
pub trait Engine {
    fn name(&self) -> &'static str;
    fn load(&self, spec: &LoadSpec) -> impl Future<Output = Result<(Loaded, Handle)>> + Send;
}

/// What keeps a loaded model alive.
pub enum Handle {
    // Boxed: tokio's Child is over 250 bytes on Windows, which trips clippy's large_enum_variant.
    Process(Box<tokio::process::Child>),
    Ollama { base: String, model: String },
}

impl Handle {
    pub async fn unload(self) -> Result<()> {
        match self {
            Handle::Process(mut c) => {
                let _ = c.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(10), c.wait()).await;
                Ok(())
            }
            Handle::Ollama { base, model } => {
                // keep_alive 0 unloads this model only; the Ollama service keeps running.
                ollama_client()
                    .post(format!("{base}/api/generate"))
                    .json(&json!({"model": model, "keep_alive": 0}))
                    .send()
                    .await?
                    .error_for_status()?;
                Ok(())
            }
        }
    }

    pub fn exited(&mut self) -> bool {
        match self {
            Handle::Process(c) => !matches!(c.try_wait(), Ok(None)),
            Handle::Ollama { .. } => false,
        }
    }
}

// ---------------------------------------------------------------- llama.cpp

/// The release asset that matches this machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetChoice {
    pub asset: String,
    /// CUDA runtime archive that Windows CUDA builds need beside them.
    pub extra: Option<String>,
    /// metal, cuda, vulkan or cpu
    pub backend: String,
}

pub fn choose_asset(hw: &Hardware, tag: &str) -> Result<AssetChoice> {
    let arch = match hw.arch.as_str() {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => bail!("no llama.cpp build for {other} CPUs"),
    };
    let b = hw.backend.as_str();
    let (asset, extra, backend) = match (hw.os.as_str(), arch) {
        ("macos", a) => (
            format!("llama-{tag}-bin-macos-{a}.tar.gz"),
            None,
            if a == "arm64" { "metal" } else { "cpu" },
        ),
        // CUDA 12.8 runs on the widest range of NVIDIA drivers and cards; arm64 only has 13.5.
        ("linux", a) if b == "cuda" => {
            let cu = if a == "x64" { "12.8" } else { "13.5" };
            (
                format!("llama-{tag}-bin-ubuntu-cuda-{cu}-{a}.tar.gz"),
                Some(format!(
                    "cudart-llama-{tag}-bin-ubuntu-cuda-{cu}-{a}.tar.gz"
                )),
                "cuda",
            )
        }
        ("linux", a) if b == "vulkan" => (
            format!("llama-{tag}-bin-ubuntu-vulkan-{a}.tar.gz"),
            None,
            "vulkan",
        ),
        ("linux", a) => (format!("llama-{tag}-bin-ubuntu-{a}.tar.gz"), None, "cpu"),
        ("windows", a) if b == "cuda" => {
            let cu = if a == "x64" { "12.4" } else { "13.5" };
            (
                format!("llama-{tag}-bin-win-cuda-{cu}-{a}.zip"),
                Some(format!("cudart-llama-bin-win-cuda-{cu}-{a}.zip")),
                "cuda",
            )
        }
        ("windows", a) if b == "vulkan" || b == "cuda" => (
            format!("llama-{tag}-bin-win-vulkan-{a}.zip"),
            None,
            "vulkan",
        ),
        ("windows", a) => (format!("llama-{tag}-bin-win-cpu-{a}.zip"), None, "cpu"),
        (os, _) => bail!("no llama.cpp build for {os}"),
    };
    Ok(AssetChoice {
        asset,
        extra,
        backend: backend.into(),
    })
}

/// Written after a verified install, in `engines/llama.cpp/<tag>/<backend>/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Install {
    pub tag: String,
    pub backend: String,
    pub asset: String,
    pub sha256: String,
    pub server: PathBuf,
    pub installed_at: String,
}

pub fn install_dir(engines: &Path, tag: &str, backend: &str) -> PathBuf {
    engines.join("llama.cpp").join(tag).join(backend)
}

/// The install for this machine, if it is already there.
pub fn installed(engines: &Path, hw: &Hardware) -> Option<Install> {
    let tag = llamacpp_tag();
    let c = choose_asset(hw, &tag).ok()?;
    let s = std::fs::read_to_string(install_dir(engines, &tag, &c.backend).join("install.json"))
        .ok()?;
    let i: Install = serde_json::from_str(&s).ok()?;
    i.server.exists().then_some(i)
}

/// Downloads, verifies (sha256 from the GitHub release API) and unpacks the
/// matching official build. Returns the install and bytes fetched.
pub async fn install_llamacpp(
    engines: &Path,
    hw: &Hardware,
    cancel: &Cancel,
    mut on_progress: impl FnMut(&Progress),
) -> Result<(Install, u64)> {
    if let Some(i) = installed(engines, hw) {
        return Ok((i, 0));
    }
    let tag = llamacpp_tag();
    let choice = choose_asset(hw, &tag)?;
    let api =
        std::env::var("FONSVIVA_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".into());
    let client = download::http_client();
    let rel: Value = client
        .get(format!(
            "{api}/repos/ggml-org/llama.cpp/releases/tags/{tag}"
        ))
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .context("reach GitHub for the llama.cpp release")?
        .error_for_status()?
        .json()
        .await?;
    let find = |name: &str| -> Result<(String, String, u64)> {
        let a = rel["assets"]
            .as_array()
            .and_then(|a| a.iter().find(|a| a["name"] == name))
            .with_context(|| format!("llama.cpp {tag} has no asset {name}"))?;
        let sha = a["digest"]
            .as_str()
            .and_then(|d| d.strip_prefix("sha256:"))
            .with_context(|| {
                format!("{name} has no published sha256; refusing to install it unverified")
            })?;
        Ok((
            a["browser_download_url"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            sha.to_string(),
            a["size"].as_u64().unwrap_or(0),
        ))
    };
    let mut wanted = vec![choice.asset.clone()];
    wanted.extend(choice.extra.clone());
    let assets: Vec<_> = wanted
        .iter()
        .map(|n| find(n).map(|f| (n.clone(), f)))
        .collect::<Result<_>>()?;
    let total: u64 = assets.iter().map(|(_, (_, _, s))| s).sum();

    let dir = install_dir(engines, &tag, &choice.backend);
    let dl = engines.join("llama.cpp").join(&tag).join("downloads");
    let mut fetched = 0;
    let mut base = 0;
    let mut main_sha = String::new();
    for (name, (url, sha, size)) in &assets {
        let dest = dl.join(name);
        if !dest.exists() {
            fetched += download::fetch(&client, url, &dest, sha, *size, cancel, |p| {
                on_progress(&Progress {
                    done: base + p.done,
                    total,
                    fetched: p.fetched,
                })
            })
            .await?;
        }
        base += size;
        if *name == choice.asset {
            main_sha = sha.clone();
        }
        // Extra archives (the CUDA runtime) unpack to the side, then join the server's libraries.
        let to = if *name == choice.asset {
            dir.clone()
        } else {
            dir.join(".extra")
        };
        let dest = dest.clone();
        tokio::task::spawn_blocking(move || unpack(&dest, &to)).await??;
    }
    let server = find_server(&dir).context("llama-server was not in the release archive")?;
    let extra = dir.join(".extra");
    if extra.exists() {
        let bin = server
            .parent()
            .context("llama-server has no folder")?
            .to_path_buf();
        tokio::task::spawn_blocking(move || {
            flatten_into(&extra, &bin)?;
            std::fs::remove_dir_all(&extra)
        })
        .await??;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755))?;
    }
    let inst = Install {
        tag,
        backend: choice.backend,
        asset: choice.asset,
        sha256: main_sha,
        server,
        installed_at: crate::util::rfc3339(crate::util::unix_now()),
    };
    std::fs::write(dir.join("install.json"), serde_json::to_vec_pretty(&inst)?)?;
    let _ = std::fs::remove_dir_all(&dl);
    Ok((inst, fetched))
}

/// Moves every file under `from` into `to`, dropping the folder structure.
fn flatten_into(from: &Path, to: &Path) -> std::io::Result<()> {
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let p = e.path();
        let t = e.file_type()?;
        if t.is_dir() {
            flatten_into(&p, to)?;
        } else {
            std::fs::rename(&p, to.join(e.file_name()))?;
        }
    }
    Ok(())
}

fn unpack(archive: &Path, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let f = std::fs::File::open(archive)?;
    let name = archive.to_string_lossy();
    if name.ends_with(".zip") {
        let mut z = zip::ZipArchive::new(f)?;
        z.extract(dir)?;
    } else {
        let mut t = tar::Archive::new(flate2::read::GzDecoder::new(f));
        t.set_preserve_permissions(true);
        // `unpack` refuses entries that escape `dir`.
        t.unpack(dir)?;
    }
    Ok(())
}

fn find_server(dir: &Path) -> Option<PathBuf> {
    let want = if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    };
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n == want) {
                return Some(p);
            }
        }
    }
    None
}

pub fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

pub struct LlamaCpp {
    pub server: PathBuf,
    pub logs: PathBuf,
}

/// The exact llama-server arguments for a load. Kept separate so tests can check them.
pub fn llama_args(spec: &LoadSpec, port: u16) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--model".into(),
        spec.model.path.to_string_lossy().into(),
        "--ctx-size".into(),
        spec.context.to_string(),
        "--n-gpu-layers".into(),
        spec.gpu_layers.to_string(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        // One slot, so the whole context belongs to one conversation.
        "--parallel".into(),
        "1".into(),
        "--jinja".into(),
        "--no-webui".into(),
    ];
    if spec.embeddings {
        a.push("--embeddings".into());
    }
    a
}

impl Engine for LlamaCpp {
    fn name(&self) -> &'static str {
        "llama.cpp"
    }

    async fn load(&self, spec: &LoadSpec) -> Result<(Loaded, Handle)> {
        let port = free_port()?;
        std::fs::create_dir_all(&self.logs)?;
        let log = self.logs.join(format!("llama-server-{port}.log"));
        let logf = std::fs::File::create(&log)?;
        let bin_dir = self.server.parent().unwrap_or(Path::new("."));
        let mut cmd = tokio::process::Command::new(&self.server);
        cmd.args(llama_args(spec, port))
            .current_dir(bin_dir)
            .stdin(std::process::Stdio::null())
            .stdout(logf.try_clone()?)
            .stderr(logf)
            .kill_on_drop(true);
        let lib_var = if cfg!(target_os = "macos") {
            "DYLD_LIBRARY_PATH"
        } else {
            "LD_LIBRARY_PATH"
        };
        if !cfg!(windows) {
            let mut v = bin_dir.as_os_str().to_owned();
            if let Some(old) = std::env::var_os(lib_var) {
                v.push(":");
                v.push(old);
            }
            cmd.env(lib_var, v);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("start {}", self.server.display()))?;
        let pid = child.id();
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();
        // Big models on slow disks take a while to map.
        let limit = Duration::from_secs(120 + spec.model.bytes / 50_000_000);
        let start = Instant::now();
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                bail!(
                    "llama-server exited ({status}) while loading: {}",
                    log_tail(&log)
                );
            }
            if let Ok(r) = client
                .get(format!("{base}/health"))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                && r.status().is_success()
            {
                break;
            }
            if start.elapsed() > limit {
                let _ = child.start_kill();
                bail!(
                    "llama-server did not become ready in {}s: {}",
                    limit.as_secs(),
                    log_tail(&log)
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // Read the context back from the server instead of trusting the flag.
        let props: Value = client
            .get(format!("{base}/props"))
            .send()
            .await?
            .json()
            .await
            .unwrap_or_default();
        let context = props["default_generation_settings"]["n_ctx"]
            .as_u64()
            .or_else(|| props["n_ctx"].as_u64())
            .unwrap_or(spec.context);
        // One token now, so a GPU's first-use kernel compile (over a minute on
        // some NVIDIA cards) happens during load, not in the first answer.
        if !spec.embeddings {
            let _ = client
                .post(format!("{base}/completion"))
                .timeout(Duration::from_secs(300))
                .json(&json!({"prompt": "Hi", "n_predict": 1, "cache_prompt": false}))
                .send()
                .await;
        }
        Ok((
            Loaded {
                engine: "llama.cpp".into(),
                engine_note: None,
                base_url: base,
                port,
                pid,
                context,
                gpu_layers: spec.gpu_layers,
                embeddings: spec.embeddings,
                log: Some(log),
            },
            Handle::Process(Box::new(child)),
        ))
    }
}

/// Last lines of a log, for error messages.
pub fn log_tail(path: &Path) -> String {
    let s = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(6)..].join(" | ")
}

/// True when a llama-server failure means it can't run this model file at all,
/// so Ollama (which has its own engine) should serve it instead.
pub fn unsupported_by_llamacpp(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("unknown model architecture")
        || e.contains("unknown architecture")
        || e.contains("failed to load model")
        || e.contains("error loading model")
}

// ------------------------------------------------------------------- Ollama

pub fn ollama_base() -> String {
    ollama_base_from(&std::env::var("OLLAMA_HOST").unwrap_or_default())
}

/// Ollama's base URL from an `OLLAMA_HOST` value.
pub fn ollama_base_from(h: &str) -> String {
    let h = h.trim().trim_end_matches('/');
    if h.is_empty() {
        return "http://127.0.0.1:11434".into();
    }
    let h = if h.starts_with("http") {
        h.to_string()
    } else {
        format!("http://{h}")
    };
    // 0.0.0.0 is a bind address; connect locally.
    let h = h.replace("://0.0.0.0", "://127.0.0.1");
    if h.rsplit(':')
        .next()
        .is_some_and(|p| p.chars().all(|c| c.is_ascii_digit()))
        && h.matches(':').count() >= 2
    {
        h
    } else {
        format!("{h}:11434")
    }
}

fn ollama_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .expect("client")
}

/// Ollama's version when it is running.
pub async fn ollama_version(base: &str) -> Option<String> {
    let v: Value = reqwest::Client::new()
        .get(format!("{base}/api/version"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    v["version"].as_str().map(str::to_string)
}

pub struct Ollama {
    pub base: String,
}

impl Engine for Ollama {
    fn name(&self) -> &'static str {
        "ollama"
    }

    async fn load(&self, spec: &LoadSpec) -> Result<(Loaded, Handle)> {
        let name = spec
            .model
            .ollama_name
            .clone()
            .context("Ollama can only serve models that are in Ollama")?;
        if ollama_version(&self.base).await.is_none() {
            bail!("Ollama is not running at {}", self.base);
        }
        // An empty prompt loads the model. num_ctx is always sent.
        ollama_client()
            .post(format!("{}/api/generate", self.base))
            .json(&json!({
                "model": name,
                "prompt": "",
                "keep_alive": "30m",
                "options": {"num_ctx": spec.context, "num_gpu": spec.gpu_layers}
            }))
            .send()
            .await?
            .error_for_status()
            .context("Ollama refused to load the model")?;
        let ps: Value = ollama_client()
            .get(format!("{}/api/ps", self.base))
            .send()
            .await?
            .json()
            .await?;
        let context = ps["models"]
            .as_array()
            .and_then(|m| {
                m.iter()
                    .find(|m| m["name"] == name.as_str() || m["model"] == name.as_str())
            })
            .and_then(|m| m["context_length"].as_u64())
            .unwrap_or(spec.context);
        let port = self
            .base
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(11434);
        Ok((
            Loaded {
                engine: "ollama".into(),
                engine_note: None,
                base_url: self.base.clone(),
                port,
                pid: None,
                context,
                gpu_layers: spec.gpu_layers,
                embeddings: spec.embeddings,
                log: None,
            },
            Handle::Ollama {
                base: self.base.clone(),
                model: name,
            },
        ))
    }
}

/// Removes a model through Ollama's own API (never by deleting a blob another
/// model may share).
pub async fn ollama_delete(base: &str, name: &str) -> Result<()> {
    let r = ollama_client()
        .delete(format!("{base}/api/delete"))
        .json(&json!({"model": name}))
        .send()
        .await
        .with_context(|| format!("reach Ollama at {base}"))?;
    if !r.status().is_success() {
        bail!(
            "Ollama refused to delete {name}: HTTP {} {}",
            r.status(),
            r.text().await.unwrap_or_default()
        );
    }
    Ok(())
}

/// OpenAI chat request to Ollama's native `/api/chat`, with num_ctx set.
pub fn openai_to_ollama_chat(body: &Value, model: &str, num_ctx: u64) -> Value {
    let mut options = json!({"num_ctx": num_ctx});
    for (from, to) in [
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("seed", "seed"),
        ("max_tokens", "num_predict"),
        ("max_completion_tokens", "num_predict"),
        ("stop", "stop"),
        ("presence_penalty", "presence_penalty"),
        ("frequency_penalty", "frequency_penalty"),
    ] {
        if let Some(v) = body.get(from) {
            options[to] = v.clone();
        }
    }
    let mut out = json!({
        "model": model,
        "messages": body.get("messages").cloned().unwrap_or(json!([])),
        "stream": body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        "options": options,
    });
    for k in ["tools", "think", "format"] {
        if let Some(v) = body.get(k) {
            out[k] = v.clone();
        }
    }
    out
}

/// One Ollama `/api/chat` message (stream line or whole reply) as an OpenAI chunk or completion.
pub fn ollama_chat_to_openai(
    v: &Value,
    id: &str,
    model: &str,
    created: u64,
    stream: bool,
) -> Value {
    let msg = &v["message"];
    let done = v["done"].as_bool().unwrap_or(false);
    let finish = done.then(|| v["done_reason"].as_str().unwrap_or("stop").to_string());
    let usage = done.then(|| {
        let p = v["prompt_eval_count"].as_u64().unwrap_or(0);
        let c = v["eval_count"].as_u64().unwrap_or(0);
        json!({"prompt_tokens": p, "completion_tokens": c, "total_tokens": p + c})
    });
    let mut content =
        json!({"role": "assistant", "content": msg["content"].as_str().unwrap_or("")});
    if let Some(t) = msg
        .get("thinking")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        content["reasoning_content"] = json!(t);
    }
    if let Some(tc) = msg.get("tool_calls") {
        content["tool_calls"] = tc.clone();
    }
    let mut out = if stream {
        json!({"id": id, "object": "chat.completion.chunk", "created": created, "model": model,
               "choices": [{"index": 0, "delta": content, "finish_reason": finish}]})
    } else {
        json!({"id": id, "object": "chat.completion", "created": created, "model": model,
               "choices": [{"index": 0, "message": content, "finish_reason": finish}]})
    };
    if let Some(u) = usage {
        out["usage"] = u;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{Cpu, Gpu};

    fn hw(os: &str, arch: &str, backend: &str) -> Hardware {
        Hardware {
            os: os.into(),
            os_version: None,
            arch: arch.into(),
            cpu: Cpu {
                brand: String::new(),
                physical_cores: 1,
                logical_cores: 1,
            },
            ram_total: 0,
            ram_available: 0,
            gpus: Vec::<Gpu>::new(),
            unified_memory: os == "macos",
            backend: backend.into(),
            notes: vec![],
        }
    }

    #[test]
    fn picks_the_matching_official_build() {
        let t = "b12477";
        let c = |os, arch, b| choose_asset(&hw(os, arch, b), t).unwrap();
        assert_eq!(
            c("macos", "aarch64", "metal").asset,
            "llama-b12477-bin-macos-arm64.tar.gz"
        );
        assert_eq!(c("macos", "aarch64", "metal").backend, "metal");
        assert_eq!(
            c("linux", "x86_64", "cpu").asset,
            "llama-b12477-bin-ubuntu-x64.tar.gz"
        );
        assert_eq!(
            c("linux", "x86_64", "vulkan").asset,
            "llama-b12477-bin-ubuntu-vulkan-x64.tar.gz"
        );
        assert_eq!(
            c("linux", "aarch64", "cpu").asset,
            "llama-b12477-bin-ubuntu-arm64.tar.gz"
        );
        let l = c("linux", "x86_64", "cuda");
        assert_eq!(l.asset, "llama-b12477-bin-ubuntu-cuda-12.8-x64.tar.gz");
        assert_eq!(l.backend, "cuda");
        assert_eq!(
            l.extra.as_deref(),
            Some("cudart-llama-b12477-bin-ubuntu-cuda-12.8-x64.tar.gz")
        );
        let w = c("windows", "x86_64", "cuda");
        assert_eq!(w.asset, "llama-b12477-bin-win-cuda-12.4-x64.zip");
        assert_eq!(
            w.extra.as_deref(),
            Some("cudart-llama-bin-win-cuda-12.4-x64.zip")
        );
        assert_eq!(
            c("windows", "x86_64", "vulkan").asset,
            "llama-b12477-bin-win-vulkan-x64.zip"
        );
        assert_eq!(
            c("windows", "x86_64", "cpu").asset,
            "llama-b12477-bin-win-cpu-x64.zip"
        );
        assert!(choose_asset(&hw("linux", "riscv64", "cpu"), t).is_err());
    }

    #[test]
    fn launch_args_are_explicit() {
        let m = Installed {
            id: "x".into(),
            name: "x".into(),
            source: "ollama".into(),
            path: "/blobs/sha256-ab".into(),
            bytes: 1,
            quant: None,
            params: None,
            architecture: None,
            context_max: None,
            sha256: None,
            duplicate_group: None,
            catalog_id: None,
            ollama_name: None,
            has_projector: false,
            shape: None,
        };
        let a = llama_args(
            &LoadSpec {
                model: m,
                context: 32768,
                gpu_layers: 0,
                embeddings: false,
            },
            5555,
        )
        .join(" ");
        assert!(a.contains("--model /blobs/sha256-ab"));
        assert!(a.contains("--ctx-size 32768"));
        assert!(a.contains("--n-gpu-layers 0"));
        assert!(a.contains("--port 5555"));
        assert!(a.contains("--host 127.0.0.1"));
        assert!(a.contains("--jinja"));
    }

    #[test]
    fn ollama_requests_always_carry_num_ctx() {
        let body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}], "stream": true, "max_tokens": 9});
        let o = openai_to_ollama_chat(&body, "tiny:latest", 32768);
        assert_eq!(o["options"]["num_ctx"], 32768);
        assert_eq!(o["options"]["num_predict"], 9);
        assert_eq!(o["model"], "tiny:latest");
        assert_eq!(o["stream"], true);

        let line = json!({"message": {"role": "assistant", "content": "Red"}, "done": false});
        let c = ollama_chat_to_openai(&line, "c1", "m", 1, true);
        assert_eq!(c["choices"][0]["delta"]["content"], "Red");
        let last = json!({"message": {"content": ""}, "done": true, "done_reason": "stop", "prompt_eval_count": 5, "eval_count": 7});
        let c = ollama_chat_to_openai(&last, "c1", "m", 1, true);
        assert_eq!(c["choices"][0]["finish_reason"], "stop");
        assert_eq!(c["usage"]["total_tokens"], 12);
    }

    #[test]
    fn ollama_host_forms() {
        let cases = [
            ("", "http://127.0.0.1:11434"),
            ("0.0.0.0", "http://127.0.0.1:11434"),
            ("127.0.0.1:11500", "http://127.0.0.1:11500"),
            ("http://box:8080/", "http://box:8080"),
        ];
        for (v, want) in cases {
            assert_eq!(ollama_base_from(v), want, "{v}");
        }
    }

    #[test]
    fn knows_when_llamacpp_cannot_run_a_file() {
        assert!(unsupported_by_llamacpp(
            "llama_model_load: error loading model: unknown model architecture: 'gemma3n_ollama'"
        ));
        assert!(!unsupported_by_llamacpp("address already in use"));
    }
}
