use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail, ensure};
use fonsviva_core::download::sha256_file;
use fonsviva_core::gguf::{Shape, read_header_file};
use fonsviva_core::plan::{
    self, BenchResult, Engine, FleetLink, Machine, Measured, Model, Placement, RESULT_SCHEMA,
};
use fonsviva_core::topology::LinkKind;
use fonsviva_core::util::{hex, rfc3339, slug, unix_now};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FleetFile {
    fleet: String,
    machines: Vec<Machine>,
    links: Vec<FleetLink>,
}

pub struct Topology {
    pub fleet: PathBuf,
    pub model: PathBuf,
    pub on: Vec<String>,
    pub link: Option<LinkKind>,
    pub rpc: Option<String>,
    pub device: Option<String>,
    pub llama_bench: PathBuf,
    pub prompt: u32,
    pub generate: u32,
    pub repetitions: u32,
    pub note: Option<String>,
    pub out: PathBuf,
}

fn placement(on: &[String], link: Option<LinkKind>) -> Result<Placement> {
    let names: Vec<&str> = on.iter().map(String::as_str).collect();
    match (names.as_slice(), link) {
        ([one], None) => Ok(Placement::single(one)),
        ([_], Some(_)) => bail!("one machine runs without --link"),
        ([], _) => bail!("name the machines with --on"),
        (_, None) => bail!("a split needs --link"),
        (many, Some(kind)) => Ok(Placement::split(many, kind)),
    }
}

#[derive(Deserialize)]
struct Row {
    build_number: u64,
    n_prompt: u64,
    n_gen: u64,
    avg_ts: f64,
}

fn rates(rows: &[Row]) -> Result<(f64, f64, u64)> {
    let prompt = rows.iter().find(|r| r.n_prompt > 0 && r.n_gen == 0);
    let decode = rows.iter().find(|r| r.n_gen > 0 && r.n_prompt == 0);
    match (prompt, decode) {
        (Some(p), Some(d)) => Ok((p.avg_ts, d.avg_ts, d.build_number)),
        _ => bail!("llama-bench gave no prompt and decode rows"),
    }
}

pub async fn topology(t: Topology) -> Result<()> {
    let fleet: FleetFile = serde_json::from_str(
        &std::fs::read_to_string(&t.fleet).with_context(|| format!("{}", t.fleet.display()))?,
    )
    .with_context(|| format!("{}", t.fleet.display()))?;
    let placement = placement(&t.on, t.link)?;
    for name in &placement.machines {
        ensure!(
            fleet.machines.iter().any(|m| &m.name == name),
            "the fleet file does not describe {name}"
        );
    }
    ensure!(
        placement.is_split() == t.rpc.is_some(),
        "a split needs --rpc (the local end of `fonsviva node forward`) and one machine takes none"
    );
    let shape = Shape::from_header(&read_header_file(&t.model)?)?;
    let quant = shape.quant.clone().context("the model names no quant")?;
    let id = shape
        .name
        .clone()
        .or_else(|| {
            t.model
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .map(|s| slug(&s))
        .context("the model has no name")?;
    let bytes = std::fs::metadata(&t.model)?.len();
    eprintln!("hashing {}", t.model.display());
    let commit = sha256_file(&t.model).await?;

    let mut args: Vec<String> = vec![
        "-m".into(),
        t.model.display().to_string(),
        "-p".into(),
        t.prompt.to_string(),
        "-n".into(),
        t.generate.to_string(),
        "-r".into(),
        t.repetitions.to_string(),
        "-ngl".into(),
        "99".into(),
        "-o".into(),
        "json".into(),
    ];
    if let Some(device) = &t.device {
        args.extend(["-dev".into(), device.clone()]);
    }
    if let Some(rpc) = &t.rpc {
        args.extend(["--rpc".into(), rpc.clone()]);
    }
    eprintln!("running {} {}", t.llama_bench.display(), args.join(" "));
    let output = tokio::process::Command::new(&t.llama_bench)
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("starting {}", t.llama_bench.display()))?;
    let mut log = format!("$ {} {}\n", t.llama_bench.display(), args.join(" ")).into_bytes();
    log.extend_from_slice(b"--- stdout\n");
    log.extend_from_slice(&output.stdout);
    log.extend_from_slice(b"--- stderr\n");
    log.extend_from_slice(&output.stderr);
    let home = fonsviva_core::paths::home_dir().display().to_string();
    let log = String::from_utf8_lossy(&log)
        .replace(&home, "~")
        .into_bytes();
    let measured_at = rfc3339(unix_now());
    std::fs::create_dir_all(t.out.join("logs"))?;
    let stem = format!(
        "{}-{}-{}-{}",
        measured_at.replace(':', ""),
        fleet.fleet,
        id,
        placement.machines.join("+")
    );
    let log_path = t.out.join("logs").join(format!("{stem}.log"));
    std::fs::write(&log_path, &log)?;
    ensure!(
        output.status.success(),
        "llama-bench failed ({}); log at {}",
        output.status,
        log_path.display()
    );
    let rows: Vec<Row> = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("llama-bench output; log at {}", log_path.display()))?;
    let (prompt_tps, decode_tps, build) = rates(&rows)?;

    let earlier = plan::load_results(&t.out)?;
    let measured: Vec<Measured> = earlier
        .iter()
        .map(|(_, r)| r)
        .filter(|r| r.fleet == fleet.fleet && r.model.id == id && r.model.quant == quant)
        .map(|r| Measured {
            placement: r.placement.clone(),
            decode_tps: r.decode_tps,
        })
        .collect();
    let plan_fleet = plan::Fleet {
        machines: fleet.machines.clone(),
        links: fleet.links.clone(),
    };
    let planned = plan::plan(&plan_fleet, bytes, &measured)
        .choice()
        .is_some_and(|c| c.placement == placement);

    let result = BenchResult {
        schema: RESULT_SCHEMA.into(),
        fleet: fleet.fleet,
        measured_at,
        machines: fleet.machines,
        links: fleet.links,
        engine: Engine {
            name: "llama.cpp".into(),
            version: format!("b{build}"),
        },
        model: Model {
            id,
            commit,
            quant,
            bytes,
        },
        placement,
        planned,
        prompt_tps,
        decode_tps,
        log_sha256: hex(&Sha256::digest(&log)),
        regression_note: t.note,
    };
    let path = t.out.join(format!("{stem}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&result)? + "\n")?;
    println!(
        "{} on {}: {decode_tps:.1} tok/s decode, {prompt_tps:.1} tok/s prompt{}\nwrote {}",
        result.model.id,
        result.placement,
        if planned { " (the plan)" } else { "" },
        path.display()
    );
    let all: Vec<BenchResult> = plan::load_results(&t.out)?
        .into_iter()
        .map(|(_, r)| r)
        .collect();
    let problems = plan::ratchet(&all);
    if !problems.is_empty() {
        bail!("the ratchet fails:\n{}", problems.join("\n"));
    }
    Ok(())
}

pub fn default_out() -> PathBuf {
    Path::new("benchmarks/results/topology").to_path_buf()
}
