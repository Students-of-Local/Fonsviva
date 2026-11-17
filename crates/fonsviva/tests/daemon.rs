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

/// An Ollama that loads models but never answers an unload.
async fn stuck_ollama() -> String {
    let generate = |Json(v): Json<Value>| async move {
        if v["keep_alive"] == 0 {
            std::future::pending::<()>().await
        }
        Json(json!({"done": true}))
    };
    let tiny = json!({"name": "tiny:latest", "model": "tiny:latest", "context_length": 4096});
    let ps = move || std::future::ready(Json(json!({ "models": [tiny.clone()] })));
    let app = Router::new()
        .route(
            "/api/version",
            get(|| async { Json(json!({"version": "0.99.0-mock"})) }),
        )
        .route("/api/generate", post(generate))
        .route("/api/ps", get(ps));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
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
    assert_eq!(
        c.get(u("/tokens"))
            .bearer_auth(&watch)
            .send()
            .await
            .unwrap()
            .status(),
        403,
        "a phone can't list the other paired phones"
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
async fn a_stuck_ollama_holds_up_neither_stop_nor_shutdown() {
    let mut d = start(&stuck_ollama().await);
    let (c, owner, base) = (http(), d.owner(), d.base.clone());
    let post = |p: &str| c.post(format!("{base}/api/v1{p}")).bearer_auth(&owner);
    let run = json!({"model": "tiny", "engine": "ollama", "context": 4096});
    let secs = Duration::from_secs;
    assert_eq!(post("/run").json(&run).send().await.unwrap().status(), 200);
    let stop = tokio::spawn(post("/stop").json(&json!({})).send());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let running = c.get(format!("{base}/api/v1/running")).bearer_auth(&owner);
    let running = tokio::time::timeout(secs(2), running.send()).await;
    assert!(running.is_ok(), "/running waits on a stop");
    assert!(
        tokio::time::timeout(secs(10), stop).await.is_ok(),
        "stop waits on Ollama"
    );
    assert_eq!(post("/run").json(&run).send().await.unwrap().status(), 200);
    post("/shutdown").send().await.unwrap();
    let exited = async {
        while d.child.try_wait().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(100)).await
        }
    };
    assert!(
        tokio::time::timeout(secs(8), exited).await.is_ok(),
        "still running 8s after shutdown"
    );
}

#[tokio::test]
async fn the_desktop_app_origin_passes_api_v1_preflight_and_others_do_not() {
    let d = start("127.0.0.1:9");
    let (c, url) = (http(), format!("{}/api/v1/events", d.base));
    let ask = |rb: reqwest::RequestBuilder, origin: &str| {
        let rb = rb
            .header("origin", origin)
            .header("access-control-request-method", "GET");
        async move {
            let r = rb.send().await.unwrap();
            (
                r.status().as_u16(),
                r.headers().get("access-control-allow-origin").cloned(),
            )
        }
    };
    let preflight = || c.request(reqwest::Method::OPTIONS, &url);
    let (s, allow) = ask(preflight(), "tauri://localhost").await;
    assert!(
        s < 300 && allow.unwrap() == "tauri://localhost",
        "the exact origin, never *"
    );
    let (s, allow) = ask(preflight(), "https://evil.example").await;
    assert!(s >= 400 && allow.is_none(), "{s}");
    let (s, _) = ask(c.get(&url), "http://tauri.localhost").await;
    assert_eq!(s, 401, "a real request still needs the bearer");
    let (s, allow) = ask(c.get(&url).bearer_auth(d.owner()), "http://tauri.localhost").await;
    assert!(
        s == 200 && allow.unwrap() == "http://tauri.localhost",
        "{s}"
    );
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
        .header(
            "access-control-request-headers",
            "authorization,content-type",
        )
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success(),
        "preflight needs no key: {}",
        r.status()
    );
    let h = r.headers();
    let get = |k: &str| {
        h.get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_lowercase()
    };
    assert!(
        matches!(
            get("access-control-allow-origin").as_str(),
            "*" | "https://example.com"
        ),
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

// ------------------------------------------------- a stand-in llama-server

/// Stands in for llama-server when a test daemon starts its engine: the
/// daemon runs a script that re-runs this test binary with only this test.
/// It checks the key the way llama-server does: everything but /health
/// needs `Authorization: Bearer $LLAMA_API_KEY` when that is set. Run as
/// part of the suite, it returns at once.
#[tokio::test]
async fn fake_llama_server() {
    use axum::http::StatusCode;
    let Ok(args) = std::env::var("FAKE_LLAMA_ARGS") else {
        return;
    };
    let args: Vec<String> = args.split(' ').map(str::to_string).collect();
    let arg = |k: &str| {
        let i = args.iter().position(|a| a == k).unwrap();
        args[i + 1].clone()
    };
    let port: u16 = arg("--port").parse().unwrap();
    let ctx: u64 = arg("--ctx-size").parse().unwrap();
    let key = std::env::var("LLAMA_API_KEY").ok();
    let app = Router::new().fallback(move |req: axum::extract::Request| {
        let key = key.clone();
        async move {
            let path = req.uri().path().to_string();
            let sent = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            if path != "/health" && key.is_some_and(|k| sent != Some(format!("Bearer {k}"))) {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": {"message": "Invalid API Key"}})),
                );
            }
            let body = match path.as_str() {
                "/props" => json!({"default_generation_settings": {"n_ctx": ctx}}),
                "/v1/chat/completions" => json!({"object": "chat.completion",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "Red"}}]}),
                _ => json!({"status": "ok"}),
            };
            (StatusCode::OK, Json(body))
        }
    });
    let l = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    axum::serve(l, app).await.unwrap();
}

/// Installs the stand-in as this daemon's llama.cpp build.
#[cfg(unix)]
fn install_fake_llama(d: &Daemon) {
    use fonsviva_core::engine;
    use std::os::unix::fs::PermissionsExt;
    let hw = fonsviva_core::hardware::detect();
    let tag = engine::llamacpp_tag();
    let choice = engine::choose_asset(&hw, &tag).unwrap();
    let dir = engine::install_dir(&d.root.join("engines"), &tag, &choice.backend);
    std::fs::create_dir_all(&dir).unwrap();
    let server = dir.join("llama-server");
    let exe = std::env::current_exe().unwrap();
    std::fs::write(
        &server,
        format!(
            "#!/bin/sh\nFAKE_LLAMA_ARGS=\"$*\" exec '{}' fake_llama_server --exact --nocapture --test-threads=1\n",
            exe.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
    let install = engine::Install {
        tag: tag.clone(),
        backend: choice.backend,
        asset: choice.asset,
        sha256: "0".repeat(64),
        server,
        installed_at: "2026-11-17T00:00:00Z".into(),
    };
    std::fs::write(
        dir.join("install.json"),
        serde_json::to_vec(&install).unwrap(),
    )
    .unwrap();
}

#[cfg(unix)]
async fn load_on_fake_llama(d: &Daemon) -> Value {
    install_fake_llama(d);
    let r = http()
        .post(format!("{}/api/v1/run", d.base))
        .bearer_auth(d.owner())
        .json(&json!({"model": "tiny", "engine": "llama.cpp", "context": 2048, "gpu_layers": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["engine"], "llama.cpp", "{v}");
    v
}

#[cfg(unix)]
async fn chat(d: &Daemon) -> reqwest::Response {
    http()
        .post(format!("{}/v1/chat/completions", d.base))
        .bearer_auth(d.desktop())
        .json(&json!({"model": "tiny", "messages": [{"role": "user", "content": "Name three primary colors."}]}))
        .send()
        .await
        .unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn llama_engine_only_answers_the_daemon() {
    let d = start("127.0.0.1:9");
    let v = load_on_fake_llama(&d).await;
    let port = v["port"].as_u64().unwrap();
    assert!(
        !v.to_string().contains("api_key"),
        "the engine secret never leaves the daemon: {v}"
    );
    let r = http()
        .get(format!("http://127.0.0.1:{port}/props"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        401,
        "the engine port refuses a caller without the secret"
    );
    let r = chat(&d).await;
    assert_eq!(r.status(), 200, "the daemon sends the secret");
    assert!(r.text().await.unwrap().contains("Red"));
    let running = http()
        .get(format!("{}/api/v1/running", d.base))
        .bearer_auth(d.owner())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!running.contains("api_key"), "{running}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_crashed_engine_is_reloaded_instead_of_forwarded_to() {
    let d = start("127.0.0.1:9");
    let v = load_on_fake_llama(&d).await;
    let pid = v["pid"].as_u64().unwrap();
    let st = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(st.success());
    // Let the kill land before asking.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = chat(&d).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let running: Value = http()
        .get(format!("{}/api/v1/running", d.base))
        .bearer_auth(d.owner())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let new_pid = running["running"][0]["pid"].as_u64().unwrap();
    assert_ne!(new_pid, pid, "a fresh engine serves it");
}

#[tokio::test]
async fn run_with_other_settings_is_refused_not_faked() {
    let (ollama, _calls) = mock_ollama().await;
    let d = start(&ollama);
    let c = http();
    let run = |body: Value| {
        c.post(format!("{}/api/v1/run", d.base))
            .bearer_auth(d.owner())
            .json(&body)
            .send()
    };
    let r = run(json!({"model": "tiny", "engine": "ollama", "context": 4096}))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = run(json!({"model": "tiny", "engine": "ollama", "context": 4096}))
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "the same settings return the load");
    let r = run(json!({"model": "tiny"})).await.unwrap();
    assert_eq!(r.status(), 200, "no settings asked, so any load will do");
    let r = run(json!({"model": "tiny", "engine": "ollama", "context": 8192}))
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "loaded_with_other_settings", "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("4096") && msg.contains("8192"), "{msg}");
}
