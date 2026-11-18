//! CLI commands. Each prints plain words first and every setting it used.

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::io::Write;
use std::time::Instant;

use crate::client::Client;
use crate::discovery::{self, Layout};
use crate::server::RunReq;
use fonsviva_core::catalog::{self, Verdict};
use fonsviva_core::engine;
use fonsviva_core::hardware::{self, Hardware};
use fonsviva_core::paths;
use fonsviva_core::store::{self, Locations};
use fonsviva_core::util::human_bytes;

fn hw_line(hw: &Hardware) -> String {
    let gpu = match hw.best_gpu() {
        Some(g) => format!(
            "{} ({} VRAM)",
            g.name,
            g.vram_total
                .map(human_bytes)
                .unwrap_or_else(|| "unknown".into())
        ),
        None if hw.unified_memory => "unified memory".into(),
        None => "no usable GPU".into(),
    };
    format!(
        "{} {}, {} cores, {} RAM ({} free), {gpu}, backend {}",
        hw.os,
        hw.arch,
        hw.cpu.logical_cores,
        human_bytes(hw.ram_total),
        human_bytes(hw.ram_available),
        hw.backend
    )
}

pub fn hw(as_json: bool) -> Result<()> {
    let hw = hardware::detect();
    if as_json {
        println!("{}", serde_json::to_string_pretty(&hw)?);
        return Ok(());
    }
    println!(
        "OS        {} {} ({})",
        hw.os,
        hw.os_version.as_deref().unwrap_or(""),
        hw.arch
    );
    println!(
        "CPU       {} ({} cores, {} threads)",
        hw.cpu.brand, hw.cpu.physical_cores, hw.cpu.logical_cores
    );
    println!(
        "RAM       {} total, {} available",
        human_bytes(hw.ram_total),
        human_bytes(hw.ram_available)
    );
    if hw.gpus.is_empty() {
        println!("GPU       none found");
    }
    for g in &hw.gpus {
        println!(
            "GPU       {} {} ({}{}, via {})",
            g.vendor,
            g.name,
            g.vram_total
                .map(|v| format!("{} VRAM", human_bytes(v)))
                .unwrap_or_else(|| "VRAM unknown".into()),
            if g.integrated { ", integrated" } else { "" },
            g.source
        );
    }
    println!(
        "Backend   {}{}",
        hw.backend,
        if hw.unified_memory {
            " (unified memory)"
        } else {
            ""
        }
    );
    for n in &hw.notes {
        println!("Note      {n}");
    }
    Ok(())
}

pub fn models_list(as_json: bool) -> Result<()> {
    let models = store::scan(&Locations::detect());
    if as_json {
        println!("{}", serde_json::to_string_pretty(&models)?);
        return Ok(());
    }
    if models.is_empty() {
        println!("No models yet. Run `fonsviva setup` or `fonsviva models pull <id>`.");
        return Ok(());
    }
    println!(
        "{:<48} {:<9} {:>8} {:<8} {:<10} PATH",
        "ID", "SOURCE", "SIZE", "QUANT", "ARCH"
    );
    for m in &models {
        println!(
            "{:<48} {:<9} {:>8} {:<8} {:<10} {}{}",
            m.id,
            m.source,
            human_bytes(m.bytes),
            m.quant.as_deref().unwrap_or("?"),
            m.architecture.as_deref().unwrap_or("?"),
            m.path.display(),
            m.duplicate_group
                .as_ref()
                .map(|g| format!("  (same file as {g})"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

pub fn recommend(as_json: bool) -> Result<()> {
    let hw = hardware::detect();
    let recs = catalog::recommend(&catalog::catalog(), &hw);
    if as_json {
        println!("{}", serde_json::to_string_pretty(&recs)?);
        return Ok(());
    }
    println!("{}\n", hw_line(&hw));
    println!(
        "  {:<22} {:>8} {:<6} {:>7}  WHY",
        "MODEL", "SIZE", "FIT", "CONTEXT"
    );
    for r in &recs {
        let fit = match r.fit.fit {
            Verdict::Fits => "fits",
            Verdict::Tight => "tight",
            Verdict::No => "no",
        };
        println!(
            "{} {:<22} {:>8} {:<6} {:>7}  {}",
            if r.setup_default { "*" } else { " " },
            r.entry.id,
            human_bytes(r.entry.bytes),
            fit,
            if r.fit.fit == Verdict::No {
                "-".to_string()
            } else {
                r.fit.context.to_string()
            },
            r.fit.reason
        );
    }
    println!("\n* setup's pick. Only models marked fits are recommended.");
    Ok(())
}

pub async fn pull(id: &str) -> Result<()> {
    let c = Client::attach(true).await?;
    let r = c.post("/models/pull", json!({"id": id})).await?;
    let job = r["job"].as_str().context("no job id")?;
    let j = c.wait_job(job, id).await?;
    println!(
        "Downloaded {id} ({} fetched, sha256 verified).",
        human_bytes(j["fetched"].as_u64().unwrap_or(0))
    );
    Ok(())
}

pub async fn rm(id: &str, yes: bool) -> Result<()> {
    let c = Client::attach(true).await?;
    let path = format!("/models/{id}");
    match c.delete(&path, json!({"confirm": yes})).await {
        Ok(r) => {
            println!("Removed {}.", r["removed"].as_str().unwrap_or(id));
            Ok(())
        }
        Err(e) if !yes && e.to_string().contains("belongs to Ollama") => {
            eprint!("{e}\nRemove it through Ollama? [y/N] ");
            if confirm()? {
                let r = c.delete(&path, json!({"confirm": true})).await?;
                println!(
                    "Removed {} through Ollama.",
                    r["removed"].as_str().unwrap_or(id)
                );
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn confirm() -> Result<bool> {
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(matches!(s.trim().to_lowercase().as_str(), "y" | "yes"))
}

fn print_loaded(r: &Value) {
    println!(
        "Loaded {} with {}",
        r["model"].as_str().unwrap_or("?"),
        r["engine"].as_str().unwrap_or("?")
    );
    if let Some(n) = r["engine_note"].as_str() {
        println!("  Note       {n}");
    }
    println!("  Context    {} tokens", r["context"]);
    println!("  GPU layers {}", r["gpu_layers"]);
    println!("  File       {}", r["path"].as_str().unwrap_or("?"));
    if let Some(f) = r.get("fit").filter(|f| f.is_object()) {
        println!(
            "  Fit        {} ({})",
            f["fit"].as_str().unwrap_or("?"),
            f["reason"].as_str().unwrap_or("")
        );
    }
    if let Some(m) = r["resident_bytes"].as_u64() {
        println!("  Memory     {}", human_bytes(m));
    }
}

pub async fn run(req: RunReq) -> Result<()> {
    let c = Client::attach(true).await?;
    let r = c.post("/run", serde_json::to_value(&req)?).await?;
    print_loaded(&r);
    println!(
        "  URL        {}/v1 (key in {})",
        c.base,
        Layout::current().cli_key().display()
    );
    Ok(())
}

pub async fn stop(model: Option<String>, daemon: bool) -> Result<()> {
    let c = match Client::attach(false).await {
        Ok(c) => c,
        Err(_) => {
            println!("The daemon is not running.");
            return Ok(());
        }
    };
    if daemon {
        c.post("/shutdown", json!({})).await?;
        println!(
            "Stopping the daemon (pid {}) and every engine it started.",
            c.info.pid
        );
        return Ok(());
    }
    let r = c.post("/stop", json!({"model": model})).await?;
    let stopped: Vec<&str> = r["stopped"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if stopped.is_empty() {
        println!("Nothing was loaded.");
    } else {
        println!("Stopped {}.", stopped.join(", "));
    }
    Ok(())
}

pub async fn status() -> Result<()> {
    let l = Layout::current();
    let c = match Client::attach(false).await {
        Ok(c) => c,
        Err(e) => {
            println!("Daemon     not running ({e})");
            println!("Data dir   {}", l.root.display());
            return Ok(());
        }
    };
    println!(
        "Daemon     {} ({} channel), pid {}, {}",
        c.base, c.info.channel, c.info.pid, c.info.version
    );
    println!("Data dir   {}", l.root.display());
    let running = c.get("/running").await?;
    let stats = c.get("/stats").await?;
    let list = running["running"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("Loaded     nothing");
    }
    for r in &list {
        println!(
            "Loaded     {} via {}, context {}, GPU layers {}, memory {}",
            r["model"].as_str().unwrap_or("?"),
            r["engine"].as_str().unwrap_or("?"),
            r["context"],
            r["gpu_layers"],
            r["resident_bytes"]
                .as_u64()
                .map(human_bytes)
                .unwrap_or_else(|| "?".into())
        );
        if let Some(s) = stats["models"]
            .as_array()
            .and_then(|a| a.iter().find(|s| s["model"] == r["model"]))
            && s["requests"].as_u64().unwrap_or(0) > 0
        {
            let a = &s["rolling"];
            println!(
                "           last {} requests: TTFT {:.0} ms, prompt {:.1} tok/s, generation {:.1} tok/s",
                a["samples"],
                a["ttft_ms"].as_f64().unwrap_or(0.0),
                a["prompt_tps"].as_f64().unwrap_or(0.0),
                a["gen_tps"].as_f64().unwrap_or(0.0)
            );
        }
    }
    Ok(())
}

/// Which model `ask` should use when none is named.
async fn pick_model(c: &Client) -> Result<String> {
    let running = c.get("/running").await?;
    if let Some(m) = running["running"].as_array().and_then(|a| a.first()) {
        return Ok(m["model"].as_str().unwrap_or_default().to_string());
    }
    let models = store::scan(&Locations::detect());
    let def = catalog::catalog()
        .models
        .into_iter()
        .find(|m| m.default)
        .map(|m| m.id);
    if let Some(m) = models
        .iter()
        .find(|m| m.catalog_id.is_some() && m.catalog_id == def)
    {
        return Ok(m.id.clone());
    }
    if let Some(m) = models.iter().find(|m| m.source == "fonsviva") {
        return Ok(m.id.clone());
    }
    bail!("No model is loaded or downloaded. Run `fonsviva setup` first, or name one with --model.")
}

pub async fn ask(prompt: &str, model: Option<String>) -> Result<()> {
    let c = Client::attach(true).await?;
    let model = match model {
        Some(m) => m,
        None => pick_model(&c).await?,
    };
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": true,
        // Answer directly; thinking models otherwise spend the budget reasoning.
        "chat_template_kwargs": {"enable_thinking": false},
        "think": false,
    });
    let sent = Instant::now();
    let resp = c
        .v1("/chat/completions", &body)
        .send()
        .await
        .context("the daemon didn't answer")?;
    if !resp.status().is_success() {
        let v: Value = resp.json().await.unwrap_or_default();
        let e = &v["error"];
        bail!(
            "{}{}",
            e["message"].as_str().unwrap_or("request failed"),
            e["fix"]
                .as_str()
                .map(|f| format!("\n  {f}"))
                .unwrap_or_default()
        );
    }
    let mut first = None;
    let mut buf = String::new();
    let mut out = std::io::stdout();
    let mut s = resp.bytes_stream();
    while let Some(chunk) = s.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk?));
        while let Some(i) = buf.find('\n') {
            let line: String = buf.drain(..=i).collect();
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(data.trim()) else {
                continue;
            };
            let d = &v["choices"][0]["delta"];
            for (k, dim) in [("reasoning_content", true), ("content", false)] {
                if let Some(t) = d[k].as_str().filter(|t| !t.is_empty()) {
                    first.get_or_insert_with(Instant::now);
                    if dim {
                        write!(out, "\x1b[2m{t}\x1b[0m")?;
                    } else {
                        write!(out, "{t}")?;
                    }
                    out.flush()?;
                }
            }
        }
    }
    println!();
    let client_ttft = first.map(|f| (f - sent).as_secs_f64() * 1000.0);
    // The daemon's tap records the sample once the stream ends.
    let mut sample = Value::Null;
    let mut running = Value::Null;
    for _ in 0..10 {
        let stats = c.get("/stats").await?;
        let r = c.get("/running").await?;
        let hit = |list: &Value| -> Value {
            list.as_array()
                .and_then(|a| {
                    a.iter().find(|s| {
                        s["model"]
                            .as_str()
                            .is_some_and(|m| m == model || m.ends_with(&format!(":{model}")))
                    })
                })
                .cloned()
                .unwrap_or(Value::Null)
        };
        let s = hit(&stats["models"]);
        running = hit(&r["running"]);
        if !s["last"].is_null() {
            sample = s;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let last = &sample["last"];
    let mem = running["resident_bytes"]
        .as_u64()
        .map(human_bytes)
        .unwrap_or_else(|| "?".into());
    let gpu = running["gpu_bytes"]
        .as_u64()
        .map(|g| format!(" + {} GPU", human_bytes(g)))
        .unwrap_or_default();
    eprintln!(
        "\nTTFT {:.0} ms | prompt {:.1} tok/s | generation {:.1} tok/s ({:.1} by wall clock) | memory {mem}{gpu} | context {} | {} {}",
        last["ttft_ms"].as_f64().or(client_ttft).unwrap_or(0.0),
        last["prompt_tps"].as_f64().unwrap_or(0.0),
        last["gen_tps"].as_f64().unwrap_or(0.0),
        last["wall_gen_tps"].as_f64().unwrap_or(0.0),
        running["context"],
        running["model"].as_str().unwrap_or(&model),
        running["engine"].as_str().unwrap_or("")
    );
    Ok(())
}

pub async fn setup(yes: bool) -> Result<()> {
    let t0 = Instant::now();
    let hw = hardware::detect();
    let recs = catalog::recommend(&catalog::catalog(), &hw);
    let Some(pick) = recs.iter().find(|r| r.setup_default) else {
        bail!(
            "No catalog model fits this machine ({}). Free some memory and run setup again.",
            hw_line(&hw)
        );
    };
    let asset = engine::choose_asset(&hw, &engine::llamacpp_tag())?;
    println!("This machine  {}", hw_line(&hw));
    println!(
        "Model         {} ({}, {}, {} license)",
        pick.entry.name,
        human_bytes(pick.entry.bytes),
        pick.entry.repo,
        pick.entry.license
    );
    println!(
        "Context       {} tokens ({})",
        pick.fit.context, pick.fit.reason
    );
    println!(
        "Engine        llama.cpp {} ({})",
        engine::llamacpp_tag(),
        asset.asset
    );
    println!("Data dir      {}", paths::data_dir().display());
    if !yes {
        eprint!("Download and load it? [y/N] ");
        if !confirm()? {
            println!("Nothing changed.");
            return Ok(());
        }
    }
    let c = Client::attach(true).await?;
    let model_id = pick.entry.id.clone();
    // Engine and model download in parallel.
    let eng = c.post("/engines/llama.cpp/install", json!({})).await?;
    let pull = c.post("/models/pull", json!({"id": model_id})).await?;
    let mut fetched = 0;
    let (e, m) = tokio::join!(
        async {
            match eng["job"].as_str() {
                Some(j) => c
                    .wait_job(j, "llama.cpp")
                    .await
                    .map(|j| j["fetched"].as_u64().unwrap_or(0)),
                None => Ok(0),
            }
        },
        async {
            let j = pull["job"].as_str().context("no job id")?;
            c.wait_job(j, &model_id)
                .await
                .map(|j| j["fetched"].as_u64().unwrap_or(0))
        }
    );
    fetched += e? + m?;
    let r = c
        .post("/run", json!({"model": format!("fonsviva:{model_id}")}))
        .await?;
    print_loaded(&r);
    println!(
        "\nReady in {:.1} s, {} downloaded.",
        t0.elapsed().as_secs_f64(),
        human_bytes(fetched)
    );
    println!("OpenAI-compatible URL  {}/v1", c.base);
    println!(
        "Key for this CLI       {}",
        Layout::current().cli_key().display()
    );
    println!("Try                    fonsviva ask \"Name three primary colors.\"");
    Ok(())
}

pub async fn doctor() -> Result<()> {
    let mut problems = 0;
    let mut line = |ok: bool, what: &str, detail: String| {
        if !ok {
            problems += 1;
        }
        println!("{} {what:<12} {detail}", if ok { "ok  " } else { "FAIL" });
    };
    let l = Layout::current();
    let writable = std::fs::create_dir_all(&l.root).is_ok()
        && std::fs::write(l.root.join(".doctor"), b"ok").is_ok()
        && std::fs::remove_file(l.root.join(".doctor")).is_ok();
    line(writable, "data dir", l.root.display().to_string());
    if let Some(free) = store::free_space(&l.root) {
        line(
            free > 2 << 30,
            "disk",
            format!("{} free", human_bytes(free)),
        );
    }
    let hw = hardware::detect();
    line(true, "hardware", hw_line(&hw));
    match Client::attach(false).await {
        Ok(c) => line(
            true,
            "daemon",
            format!("{} pid {}, proof and data dir id match", c.base, c.info.pid),
        ),
        Err(e) => {
            let port = discovery::read_info(&l)
                .map(|i| i.port)
                .unwrap_or_else(paths::port);
            let holder = discovery::port_holder(port);
            let ok = holder.is_none();
            line(
                ok,
                "daemon",
                match holder {
                    Some(h) => format!("not running, and port {port} is held by {h}"),
                    None => format!("not running ({e}); port {port} is free"),
                },
            );
        }
    }
    match engine::installed(&paths::engines_dir(), &hw) {
        Some(i) => line(
            true,
            "llama.cpp",
            format!("{} at {}", i.tag, i.server.display()),
        ),
        None => line(
            true,
            "llama.cpp",
            format!(
                "not installed yet ({} downloads on first run)",
                engine::llamacpp_tag()
            ),
        ),
    }
    let base = engine::ollama_base();
    match engine::ollama_version(&base).await {
        Some(v) => line(true, "ollama", format!("{v} at {base}")),
        None => line(true, "ollama", format!("not running at {base} (optional)")),
    }
    let models = store::scan(&Locations::detect());
    line(true, "models", format!("{} found", models.len()));
    let cat = catalog::catalog();
    let sane = cat.models.iter().all(|m| m.sha256.len() == 64);
    line(
        sane,
        "catalog",
        format!("{} models, schema {}", cat.models.len(), cat.schema),
    );
    if problems > 0 {
        bail!("{problems} check(s) failed");
    }
    Ok(())
}

pub async fn topology(
    peers: &[String],
    count: u32,
    as_json: bool,
    out: Option<&std::path::Path>,
) -> Result<()> {
    let mut parsed = Vec::new();
    for peer in peers {
        let (name, addrs) = peer
            .split_once('=')
            .with_context(|| format!("--peer {peer}: expected name=addr[,addr...]"))?;
        let addrs = addrs
            .split(',')
            .map(|a| {
                a.trim()
                    .parse()
                    .with_context(|| format!("--peer {peer}: {a} is not an IPv4 address"))
            })
            .collect::<Result<Vec<std::net::Ipv4Addr>>>()?;
        parsed.push((name.to_string(), addrs));
    }
    let topology =
        tokio::task::spawn_blocking(move || fonsviva_core::topology::probe(&parsed, count)).await?;
    let text = serde_json::to_string_pretty(&topology)?;
    if let Some(out) = out {
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(out, format!("{text}\n"))?;
    }
    if as_json {
        println!("{text}");
        return Ok(());
    }
    let ms = |rtt: Option<fonsviva_core::topology::Rtt>| {
        rtt.map_or("no answer".to_string(), |r| {
            format!("{:.2} ms avg, {:.2} max", r.avg_ms, r.max_ms)
        })
    };
    for peer in &topology.peers {
        println!("{}", peer.name);
        for link in &peer.links {
            println!(
                "  {:?} {} {} -> {}: bound {}; unbound {}",
                link.kind,
                link.interface,
                link.local,
                link.peer,
                ms(link.bound),
                ms(link.unbound)
            );
            if let (false, Some(net), true) = (
                link.routed_on_link,
                &link.network_route_interface,
                link.route_interface.as_deref() == Some(link.interface.as_str()),
            ) {
                println!(
                    "    {} reaches {} only through a host route cloned from earlier traffic; when it expires, the network route sends it through {}: only a bound socket can count on this link",
                    link.peer, link.interface, net
                );
            } else if !link.routed_on_link
                && link.route_interface.as_deref() == Some(link.interface.as_str())
            {
                println!(
                    "    {} reaches {} through a host route cloned from earlier traffic, and the network route under it could not be read: only a bound socket can count on this link",
                    link.peer, link.interface
                );
            } else if !link.routed_on_link {
                println!(
                    "    the kernel routes {} through {}, not {}: only a bound socket uses this link",
                    link.peer,
                    link.route_interface.as_deref().unwrap_or("nothing"),
                    link.interface
                );
            }
        }
        for addr in &peer.no_link {
            println!("  {addr}: no local interface shares its subnet");
        }
    }
    Ok(())
}
