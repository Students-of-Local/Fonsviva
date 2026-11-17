//! Runs the real `fonsviva serve` against a scratch HOME and FONSVIVA_HOME on
//! a free port, with a mock Ollama. Nothing here touches the user's state.

use axum::extract::State;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

struct Daemon {
    child: Child,
    _dir: tempfile::TempDir,
    root: PathBuf,
    ollama_models: PathBuf,
    base: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap().trim().to_string()
}

impl Daemon {
    fn owner(&self) -> String {
        read(&self.root.join("owner.token"))
    }
    fn control(&self) -> String {
        read(&self.root.join("control.token"))
    }
    fn desktop(&self) -> String {
        read(&self.root.join("desktop.key"))
    }
}

fn command(dir: &Path, root: &Path, ollama: &str, port: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_fonsviva"));
    c.arg("serve")
        .env("HOME", dir.join("home"))
        .env("USERPROFILE", dir.join("home"))
        .env("FONSVIVA_HOME", root)
        .env("FONSVIVA_PORT", port)
        .env("OLLAMA_HOST", ollama)
        .env("OLLAMA_MODELS", dir.join("ollama"))
        .env("HF_HOME", dir.join("hf"))
        .env("HF_HUB_CACHE", dir.join("hf/hub"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("share"))
        .env("LOCALAPPDATA", dir.join("local"))
        .env("APPDATA", dir.join("roaming"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    c
}

fn start(ollama: &str) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let ollama_models = dir.path().join("ollama");
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    std::fs::create_dir_all(&ollama_models).unwrap();
    write_ollama_model(&ollama_models, "tiny", "latest");
    let child = command(dir.path(), &root, ollama, "0").spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let port = loop {
        if let Ok(s) = std::fs::read_to_string(root.join("daemon.json"))
            && let Ok(v) = serde_json::from_str::<Value>(&s)
        {
            break v["port"].as_u64().unwrap();
        }
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(50));
    };
    Daemon {
        child,
        _dir: dir,
        root,
        ollama_models,
        base: format!("http://127.0.0.1:{port}"),
    }
}

/// A minimal GGUF v3 header for a two-layer llama.
fn tiny_gguf() -> Vec<u8> {
    let mut b = b"GGUF".to_vec();
    b.extend(3u32.to_le_bytes());
    b.extend(0u64.to_le_bytes());
    let s = |b: &mut Vec<u8>, x: &str| {
        b.extend((x.len() as u64).to_le_bytes());
        b.extend(x.as_bytes());
    };
    let kv: Vec<(&str, u32)> = vec![
        ("llama.block_count", 2),
        ("llama.embedding_length", 64),
        ("llama.attention.head_count", 4),
        ("llama.attention.head_count_kv", 4),
        ("llama.context_length", 8192),
    ];
    b.extend((kv.len() as u64 + 2).to_le_bytes());
    s(&mut b, "general.architecture");
    b.extend(8u32.to_le_bytes());
    s(&mut b, "llama");
    for (k, v) in kv {
        s(&mut b, k);
        b.extend(4u32.to_le_bytes());
        b.extend(v.to_le_bytes());
    }
    s(&mut b, "tokenizer.ggml.tokens");
    b.extend(9u32.to_le_bytes());
    b.extend(8u32.to_le_bytes());
    b.extend(2u64.to_le_bytes());
    s(&mut b, "a");
    s(&mut b, "b");
    b.extend(vec![0u8; 4096]);
    b
}

fn write_ollama_model(root: &Path, model: &str, tag: &str) -> PathBuf {
    let blob = tiny_gguf();
    let sha = hex(&Sha256::digest(&blob));
    let blobs = root.join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    let path = blobs.join(format!("sha256-{sha}"));
    std::fs::write(&path, &blob).unwrap();
    let mdir = root
        .join("manifests/registry.ollama.ai/library")
        .join(model);
    std::fs::create_dir_all(&mdir).unwrap();
    let manifest = json!({"schemaVersion": 2, "layers": [
        {"mediaType": "application/vnd.ollama.image.model", "digest": format!("sha256:{sha}"), "size": blob.len()}
    ]});
    std::fs::write(mdir.join(tag), manifest.to_string()).unwrap();
    path
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hmac(key: &[u8], msg: &[u8]) -> String {
    let mut k = [0u8; 64];
    k[..key.len()].copy_from_slice(key);
    let pad = |x: u8| k.iter().map(|b| b ^ x).collect::<Vec<u8>>();
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(msg)
        .finalize();
    hex(&Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize())
}

/// A mock Ollama that records what it was asked.
async fn mock_ollama() -> (String, Calls) {
    let calls: Calls = Arc::default();
    async fn rec(State(c): State<Calls>, path: &str, v: Value) {
        c.lock().unwrap().push((path.to_string(), v));
    }
    let app = Router::new()
        .route("/api/version", get(|| async { Json(json!({"version": "0.99.0-mock"})) }))
        .route(
            "/api/generate",
            post(|s: State<Calls>, Json(v): Json<Value>| async move {
                rec(s, "generate", v).await;
                Json(json!({"done": true}))
            }),
        )
        .route(
            "/api/ps",
            get(|State(c): State<Calls>| async move {
                let ctx = c
                    .lock()
                    .unwrap()
                    .iter()
                    .rev()
                    .find(|(p, _)| p == "generate")
                    .and_then(|(_, v)| v["options"]["num_ctx"].as_u64());
                Json(json!({"models": [{"name": "tiny:latest", "model": "tiny:latest", "context_length": ctx}]}))
            }),
        )
        .route(
            "/api/chat",
            post(|s: State<Calls>, Json(v): Json<Value>| async move {
                rec(s, "chat", v).await;
                let lines = [
                    json!({"model": "tiny:latest", "message": {"role": "assistant", "content": "Red"}, "done": false}),
                    json!({"model": "tiny:latest", "message": {"role": "assistant", "content": ", blue, yellow"}, "done": false}),
                    json!({"model": "tiny:latest", "message": {"role": "assistant", "content": ""}, "done": true,
                           "done_reason": "stop", "prompt_eval_count": 10, "prompt_eval_duration": 50_000_000u64,
                           "eval_count": 4, "eval_duration": 200_000_000u64}),
                ];
                lines.iter().map(|l| format!("{l}\n")).collect::<String>()
            }),
        )
        .route(
            "/api/delete",
            delete(|s: State<Calls>, Json(v): Json<Value>| async move {
                rec(s, "delete", v).await;
                Json(json!({}))
            }),
        )
        .with_state(calls.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), calls)
}

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

#[tokio::test]
async fn health_proves_ownership_and_secrets_are_private() {
    let d = start("127.0.0.1:9");
    let info: Value = serde_json::from_str(&read(&d.root.join("daemon.json"))).unwrap();
    let v: Value = http()
        .get(format!("{}/health?challenge=n0nce", d.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(
        v["data_dir_id"].as_str().unwrap(),
        read(&d.root.join("data_dir_id"))
    );
    assert_eq!(v["channel"], "dev");
    assert_eq!(v["api"], 1);
    assert_eq!(v["schema"], 1);
    let secret = info["secret"].as_str().unwrap();
    assert_eq!(
        v["proof"].as_str().unwrap(),
        hmac(secret.as_bytes(), b"n0nce")
    );
    let plain: Value = http()
        .get(format!("{}/health", d.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(plain.get("proof").is_none() && plain.get("secret").is_none());
    #[cfg(unix)]
    for f in [
        "daemon.json",
        "owner.token",
        "control.token",
        "desktop.key",
        "cli.key",
    ] {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(d.root.join(f))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{f}");
    }
    // Per-channel state and shared keys.
    assert!(d.root.join("state/dev/schema.json").exists());
    assert!(d.root.join("shared/keys.json").exists());
}

#[tokio::test]
async fn host_origin_and_keys_are_enforced() {
    let d = start("127.0.0.1:9");
    let c = http();
    let api = format!("{}/api/v1/hardware", d.base);
    assert_eq!(c.get(&api).send().await.unwrap().status(), 401);
    assert_eq!(
        c.get(&api)
            .bearer_auth(d.owner())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let r = c
        .get(&api)
        .bearer_auth(d.owner())
        .header("host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = c
        .get(&api)
        .bearer_auth(d.owner())
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = c
        .get(&api)
        .bearer_auth(d.owner())
        .header("origin", "http://localhost:1430")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v1 = format!("{}/v1/models", d.base);
    assert_eq!(c.get(&v1).send().await.unwrap().status(), 401);
    assert_eq!(
        c.get(&v1)
            .bearer_auth(d.owner())
            .send()
            .await
            .unwrap()
            .status(),
        401,
        "a control token is not an app key"
    );
    let r = c
        .get(&v1)
        .bearer_auth(d.desktop())
        .header("origin", "https://any.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "/v1 accepts any Origin with a valid key");
    let r = c
        .get(&v1)
        .bearer_auth(d.desktop())
        .header("host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "the Host check covers /v1 too");
}

#[tokio::test]
async fn token_scopes_keys_and_watch_tokens() {
    let d = start("127.0.0.1:9");
    let c = http();
    let u = |p: &str| format!("{}/api/v1{p}", d.base);
    // control can't touch keys.
    assert_eq!(
        c.get(u("/keys"))
            .bearer_auth(d.control())
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let r: Value = c
        .post(u("/keys"))
        .bearer_auth(d.owner())
        .json(&json!({"app": "corunis"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["profile"], "corunis");
    let key = r["key"].as_str().unwrap().to_string();
    let id = r["id"].as_str().unwrap().to_string();
    let list = c
        .get(u("/keys"))
        .bearer_auth(d.owner())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !list.contains(&key) && list.contains("corunis"),
        "listing never shows a secret"
    );
    let v1 = format!("{}/v1/models", d.base);
    assert_eq!(
        c.get(&v1).bearer_auth(&key).send().await.unwrap().status(),
        200
    );
    let r = c
        .post(u("/keys"))
        .bearer_auth(d.owner())
        .json(&json!({"app": "corunis", "profile": "personal"}))
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_client_error(),
        "the profile comes from the app, not the request"
    );
    assert_eq!(
        c.delete(u(&format!("/keys/{id}")))
            .bearer_auth(d.owner())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        c.get(&v1).bearer_auth(&key).send().await.unwrap().status(),
        401
    );

    // control mints a watch token; watch reads but can't pull or mint.
    let r: Value = c
        .post(u("/tokens"))
        .bearer_auth(d.control())
        .json(&json!({"scope": "watch", "label": "phone"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let watch = r["token"].as_str().unwrap().to_string();
    let wid = r["id"].as_str().unwrap().to_string();
    assert_eq!(
        c.get(u("/running"))
            .bearer_auth(&watch)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let pull = c
        .post(u("/models/pull"))
        .bearer_auth(&watch)
        .json(&json!({"id": "qwen3.5-0.8b"}))
        .send()
        .await
        .unwrap();
    assert_eq!(pull.status(), 403);
    let mint = c
        .post(u("/tokens"))
        .bearer_auth(&watch)
        .json(&json!({"scope": "watch"}))
        .send()
        .await
        .unwrap();
    assert_eq!(mint.status(), 403);
    let owner_scope = c
        .post(u("/tokens"))
        .bearer_auth(d.owner())
        .json(&json!({"scope": "owner"}))
        .send()
        .await
        .unwrap();
    assert_eq!(owner_scope.status(), 400);
    assert_eq!(
        c.delete(u(&format!("/tokens/{wid}")))
            .bearer_auth(d.control())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        c.get(u("/running"))
            .bearer_auth(&watch)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

#[tokio::test]
async fn wire_and_cloud_are_not_built_yet() {
    let d = start("127.0.0.1:9");
    for (m, p) in [
        ("GET", "/wire"),
        ("POST", "/wire/pi"),
        ("GET", "/cloud/runpod/pods"),
        ("POST", "/cloud/runpod/pods"),
    ] {
        let r = http()
            .request(m.parse().unwrap(), format!("{}/api/v1{p}", d.base))
            .bearer_auth(d.owner())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 501, "{m} {p}");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["error"]["code"], "not_built");
    }
}

#[tokio::test]
async fn one_daemon_per_data_dir_and_a_taken_port_is_named() {
    let d = start("127.0.0.1:9");
    let dir = tempfile::tempdir().unwrap();
    let out = command(dir.path(), &d.root, "127.0.0.1:9", "0")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("already owns"), "{err}");

    let port = d.base.rsplit(':').next().unwrap();
    let out = command(dir.path(), &dir.path().join("other"), "127.0.0.1:9", port)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(&format!("Port {port} is taken by")), "{err}");
    #[cfg(target_os = "linux")]
    assert!(
        err.contains(&format!("pid {}", d.child.id())),
        "names the holder: {err}"
    );
}

#[tokio::test]
async fn ollama_models_are_listed_in_place_and_removed_through_ollama() {
    let (ollama, calls) = mock_ollama().await;
    let d = start(&ollama);
    let c = http();
    let u = |p: &str| format!("{}/api/v1{p}", d.base);
    let v: Value = c
        .get(u("/models"))
        .bearer_auth(d.owner())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let m = v["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "ollama:tiny:latest")
        .cloned()
        .unwrap();
    assert_eq!(m["source"], "ollama");
    let path = PathBuf::from(m["path"].as_str().unwrap());
    assert!(
        path.starts_with(d.ollama_models.join("blobs")),
        "served from the blob: {}",
        path.display()
    );

    let r = c
        .delete(u("/models/ollama:tiny:latest"))
        .bearer_auth(d.owner())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "confirm");
    let r = c
        .delete(u("/models/ollama:tiny:latest"))
        .bearer_auth(d.owner())
        .json(&json!({"confirm": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let deletes: Vec<Value> = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p == "delete")
        .map(|(_, v)| v.clone())
        .collect();
    assert_eq!(deletes, vec![json!({"model": "tiny:latest"})]);
    assert!(
        path.exists(),
        "Fonsviva never deletes an Ollama blob itself"
    );
}

#[tokio::test]
async fn ollama_engine_gets_explicit_context_and_streams_through_v1() {
    let (ollama, calls) = mock_ollama().await;
    let d = start(&ollama);
    let c = http();
    let u = |p: &str| format!("{}/api/v1{p}", d.base);
    let r: Value = c
        .post(u("/run"))
        .bearer_auth(d.owner())
        .json(&json!({"model": "tiny", "engine": "ollama", "context": 4096}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["engine"], "ollama", "{r}");
    assert_eq!(r["context"], 4096);
    let load = calls
        .lock()
        .unwrap()
        .iter()
        .find(|(p, _)| p == "generate")
        .cloned()
        .unwrap()
        .1;
    assert_eq!(load["options"]["num_ctx"], 4096);

    let body = json!({"model": "tiny", "stream": true, "messages": [{"role": "user", "content": "Name three primary colors."}]});
    let text = c
        .post(format!("{}/v1/chat/completions", d.base))
        .bearer_auth(d.desktop())
        .json(&body)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        text.contains("\"Red\"") && text.contains("data: [DONE]"),
        "{text}"
    );
    let chat = calls
        .lock()
        .unwrap()
        .iter()
        .find(|(p, _)| p == "chat")
        .cloned()
        .unwrap()
        .1;
    assert_eq!(chat["options"]["num_ctx"], 4096, "num_ctx is always sent");

    let running: Value = c
        .get(u("/running"))
        .bearer_auth(d.owner())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(running["running"][0]["context"], 4096);
    let mut sample = Value::Null;
    for _ in 0..20 {
        let s: Value = c
            .get(u("/stats"))
            .bearer_auth(d.owner())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        sample = s["models"][0]["last"].clone();
        if !sample.is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(sample["gen_tokens"], 4);
    assert_eq!(sample["gen_tps"], 20.0);
    assert_eq!(sample["prompt_tps"], 200.0);

    let r: Value = c
        .post(u("/stop"))
        .bearer_auth(d.owner())
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["stopped"][0], "ollama:tiny:latest");
    let unload = calls
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(p, _)| p == "generate")
        .cloned()
        .unwrap()
        .1;
    assert_eq!(unload["keep_alive"], 0);
}

#[tokio::test]
async fn plugin_keys_are_control_scoped_local_only_and_replaceable() {
    let d = start("127.0.0.1:9");
    let c = http();
    let u = |p: &str| format!("{}/api/v1{p}", d.base);
    // control may create a plugin key but not a regular key.
    let r = c
        .post(u("/keys"))
        .bearer_auth(d.control())
        .json(&json!({"app": "pi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let first: Value = c
        .post(u("/plugin-keys"))
        .bearer_auth(d.control())
        .json(&json!({"plugin": "notes"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let s1 = first["secret"].as_str().unwrap().to_string();
    assert_eq!(first["profile"], "plugin");
    let v1 = format!("{}/v1/models", d.base);
    let models: Value = c
        .get(&v1)
        .bearer_auth(&s1)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let data = models["data"].as_array().unwrap();
    assert!(!data.is_empty());
    assert!(data.iter().all(|m| m["location"] == "local"), "{models}");
    // A cloud model is refused for a plugin key.
    let body =
        json!({"model": "cloud:qwen3.6-35b-a3b", "messages": [{"role": "user", "content": "hi"}]});
    let r = c
        .post(format!("{}/v1/chat/completions", d.base))
        .bearer_auth(&s1)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "cloud_blocked");
    // Re-creating revokes the first key.
    let second: Value = c
        .post(u("/plugin-keys"))
        .bearer_auth(d.control())
        .json(&json!({"plugin": "notes"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let s2 = second["secret"].as_str().unwrap().to_string();
    assert_eq!(
        c.get(&v1).bearer_auth(&s1).send().await.unwrap().status(),
        401
    );
    assert_eq!(
        c.get(&v1).bearer_auth(&s2).send().await.unwrap().status(),
        200
    );
    // The fixed app profiles hold for their plugin ids.
    let g: Value = c
        .post(u("/plugin-keys"))
        .bearer_auth(d.control())
        .json(&json!({"plugin": "gantrel"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(g["profile"], "gantrel");
    // watch can't create one; the plugin route never revokes a non-plugin key.
    let w: Value = c
        .post(u("/tokens"))
        .bearer_auth(d.control())
        .json(&json!({"scope": "watch"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let r = c
        .post(u("/plugin-keys"))
        .bearer_auth(w["token"].as_str().unwrap())
        .json(&json!({"plugin": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let keys: Value = c
        .get(u("/keys"))
        .bearer_auth(d.owner())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let desktop_id = keys["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["app"] == "desktop")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = c
        .delete(u(&format!("/plugin-keys/{desktop_id}")))
        .bearer_auth(d.control())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let id = second["id"].as_str().unwrap();
    assert_eq!(
        c.delete(u(&format!("/plugin-keys/{id}")))
            .bearer_auth(d.control())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        c.get(&v1).bearer_auth(&s2).send().await.unwrap().status(),
        401
    );
}

#[tokio::test]
async fn shutdown_finishes_while_an_events_stream_is_open() {
    let mut d = start("127.0.0.1:9");
    let c = http();
    let events = c
        .get(format!("{}/api/v1/events", d.base))
        .bearer_auth(d.owner())
        .send()
        .await
        .unwrap();
    assert_eq!(events.status(), 200);
    let r = c
        .post(format!("{}/api/v1/shutdown", d.base))
        .bearer_auth(d.owner())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let deadline = Instant::now() + Duration::from_secs(8);
    while d.child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "the daemon is still running 8s after shutdown with /events open"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !d.root.join("daemon.json").exists(),
        "daemon.json is removed on shutdown"
    );
    drop(events);
}

#[tokio::test]
async fn v1_answers_browser_preflight_and_api_v1_stays_strict() {
    let d = start("127.0.0.1:9");
    let c = http();
    let r = c
        .request(
            reqwest::Method::OPTIONS,
            format!("{}/v1/chat/completions", d.base),
        )
        .header("origin", "https://example.com")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "authorization,content-type")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "preflight needs no key: {}", r.status());
    let h = r.headers();
    let get = |k: &str| {
        h.get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_lowercase()
    };
    assert!(
        matches!(get("access-control-allow-origin").as_str(), "*" | "https://example.com"),
        "{h:?}"
    );
    assert!(get("access-control-allow-headers").contains("authorization"));
    assert!(get("access-control-allow-headers").contains("content-type"));
    assert!(get("access-control-allow-methods").contains("post"));

    let v1 = format!("{}/v1/models", d.base);
    let r = c
        .get(&v1)
        .header("origin", "https://example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "a real /v1 request still needs a key");
    let r = c
        .get(&v1)
        .bearer_auth(d.desktop())
        .header("origin", "https://example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().contains_key("access-control-allow-origin"));

    let api = format!("{}/api/v1/hardware", d.base);
    let r = c
        .request(reqwest::Method::OPTIONS, &api)
        .header("origin", "https://example.com")
        .header("access-control-request-method", "GET")
        .send()
        .await
        .unwrap();
    assert!(!r.headers().contains_key("access-control-allow-origin"));
    let r = c
        .get(&api)
        .bearer_auth(d.owner())
        .header("origin", "https://example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "/api/v1 keeps its Origin rule");
    assert!(!r.headers().contains_key("access-control-allow-origin"));
}
