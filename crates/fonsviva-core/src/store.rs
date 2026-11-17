//! One list of installed models across Fonsviva's own store, Ollama,
//! LM Studio and the Hugging Face cache. Other tools' files are read in
//! place and never copied.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::catalog::{self, Entry};
use crate::download::{self, Cancel, Progress};
use crate::gguf::{self, Shape};
use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Installed {
    pub id: String,
    pub name: String,
    /// fonsviva, ollama, lmstudio, hf
    pub source: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub quant: Option<String>,
    pub params: Option<String>,
    pub architecture: Option<String>,
    pub context_max: Option<u64>,
    pub sha256: Option<String>,
    pub duplicate_group: Option<String>,
    /// Catalog id when this is a catalog model.
    pub catalog_id: Option<String>,
    /// Ollama's own name for the model, for its API.
    pub ollama_name: Option<String>,
    /// A vision projector blob ships with it (llama.cpp would need `--mmproj`).
    #[serde(default)]
    pub has_projector: bool,
    #[serde(skip)]
    pub shape: Option<Shape>,
}

/// Where to look. `detect()` reads the usual places and env overrides; tests
/// build one by hand.
#[derive(Debug, Clone, Default)]
pub struct Locations {
    pub fonsviva: PathBuf,
    pub ollama: Vec<PathBuf>,
    pub lmstudio: Vec<PathBuf>,
    pub hf: Vec<PathBuf>,
}

impl Locations {
    pub fn detect() -> Locations {
        let home = paths::home_dir();
        let env = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let mut ollama = vec![];
        if let Some(p) = env("OLLAMA_MODELS") {
            ollama.push(p);
        }
        ollama.push(home.join(".ollama/models"));
        if cfg!(target_os = "linux") {
            ollama.push("/usr/share/ollama/.ollama/models".into());
            ollama.push("/var/lib/ollama/.ollama/models".into());
        }
        let mut lmstudio = vec![];
        if let Ok(p) = std::fs::read_to_string(home.join(".lmstudio-home-pointer")) {
            lmstudio.push(PathBuf::from(p.trim()).join("models"));
        }
        lmstudio.push(home.join(".lmstudio/models"));
        lmstudio.push(home.join(".cache/lm-studio/models"));
        let mut hf = vec![];
        if let Some(p) = env("HF_HUB_CACHE") {
            hf.push(p);
        }
        if let Some(p) = env("HF_HOME") {
            hf.push(p.join("hub"));
        }
        hf.push(
            env("XDG_CACHE_HOME")
                .unwrap_or_else(|| home.join(".cache"))
                .join("huggingface/hub"),
        );
        Locations {
            fonsviva: paths::models_dir(),
            ollama: dedup(ollama),
            lmstudio: dedup(lmstudio),
            hf: dedup(hf),
        }
    }
}

fn dedup(v: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = vec![];
    for p in v {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// The record Fonsviva writes next to each model it downloaded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub repo: String,
    pub revision: String,
    pub file: String,
    pub sha256: String,
    pub bytes: u64,
    pub downloaded_at: String,
}

const RECORD: &str = "fonsviva.json";

type ShapeKey = (PathBuf, u64, Option<std::time::SystemTime>);
static SHAPES: Mutex<Option<HashMap<ShapeKey, Option<Shape>>>> = Mutex::new(None);

/// GGUF shape with a cache keyed by path, size and mtime.
pub fn shape_of(path: &Path) -> Option<Shape> {
    let meta = std::fs::metadata(path).ok()?;
    let key = (path.to_path_buf(), meta.len(), meta.modified().ok());
    if let Some(s) = SHAPES
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .get(&key)
    {
        return s.clone();
    }
    let s = gguf::read_header_file(path)
        .ok()
        .and_then(|h| Shape::from_header(&h).ok());
    SHAPES
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(key, s.clone());
    s
}

fn with_shape(mut m: Installed) -> Installed {
    if let Some(s) = shape_of(&m.path) {
        m.quant = m.quant.or(s.quant.clone());
        m.params = m.params.or(s.size_label.clone());
        m.architecture = Some(s.architecture.clone());
        m.context_max = (s.context_length > 0).then_some(s.context_length);
        if m.name.is_empty() {
            m.name = s.name.clone().unwrap_or_default();
        }
        m.shape = Some(s);
    }
    m
}

/// Scans every location. Blocking (reads GGUF headers); call from a blocking task.
pub fn scan(loc: &Locations) -> Vec<Installed> {
    let mut out = vec![];
    out.extend(scan_fonsviva(&loc.fonsviva));
    for d in &loc.ollama {
        out.extend(scan_ollama(d));
    }
    for d in &loc.lmstudio {
        out.extend(scan_lmstudio(d));
    }
    for d in &loc.hf {
        out.extend(scan_hf(d));
    }
    let mut out: Vec<Installed> = out.into_iter().map(with_shape).collect();
    mark_duplicates(&mut out);
    out
}

pub fn scan_fonsviva(dir: &Path) -> Vec<Installed> {
    let cat = catalog::catalog();
    let mut out = vec![];
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(rec) = std::fs::read_to_string(e.path().join(RECORD))
            .map_err(anyhow::Error::from)
            .and_then(|s| serde_json::from_str::<Record>(&s).map_err(Into::into))
        else {
            continue;
        };
        let path = e.path().join(&rec.file);
        if !path.exists() {
            continue;
        }
        let entry = cat.models.iter().find(|m| m.id == rec.id);
        out.push(Installed {
            id: format!("fonsviva:{}", rec.id),
            name: entry.map(|m| m.name.clone()).unwrap_or_default(),
            source: "fonsviva".into(),
            bytes: rec.bytes,
            quant: entry.map(|m| m.quant.clone()),
            params: entry.map(|m| m.params.clone()),
            architecture: None,
            context_max: None,
            sha256: Some(rec.sha256.clone()),
            duplicate_group: None,
            catalog_id: Some(rec.id.clone()),
            ollama_name: None,
            has_projector: false,
            shape: None,
            path,
        });
    }
    out
}

/// Ollama's display name for a manifest path `<host>/<namespace>/<model>/<tag>`.
pub fn ollama_name(host: &str, ns: &str, model: &str, tag: &str) -> String {
    match (host, ns) {
        ("registry.ollama.ai", "library") => format!("{model}:{tag}"),
        ("registry.ollama.ai", _) => format!("{ns}/{model}:{tag}"),
        _ => format!("{host}/{ns}/{model}:{tag}"),
    }
}

pub fn scan_ollama(root: &Path) -> Vec<Installed> {
    let mut seen = std::collections::HashSet::new();
    let mut out = vec![];
    for mdir in ["manifests", "manifests-v2"] {
        let base = root.join(mdir);
        for host in read_dirs(&base) {
            for ns in read_dirs(&host) {
                for model in read_dirs(&ns) {
                    for tag in std::fs::read_dir(&model).into_iter().flatten().flatten() {
                        let p = tag.path();
                        if !p.is_file() {
                            continue;
                        }
                        let name = ollama_name(
                            &file_name(&host),
                            &file_name(&ns),
                            &file_name(&model),
                            &file_name(&p),
                        );
                        if !seen.insert(name.clone()) {
                            continue;
                        }
                        if let Some(m) = ollama_entry(root, &p, &name) {
                            out.push(m);
                        }
                    }
                }
            }
        }
    }
    out
}

fn ollama_entry(root: &Path, manifest: &Path, name: &str) -> Option<Installed> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(manifest).ok()?).ok()?;
    let layers = v.get("layers")?.as_array()?;
    let mt = |l: &serde_json::Value| {
        l.get("mediaType")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string()
    };
    let model = layers
        .iter()
        .find(|l| mt(l) == "application/vnd.ollama.image.model")?;
    let digest = model.get("digest")?.as_str()?;
    let sha = digest.strip_prefix("sha256:")?;
    let path = root.join("blobs").join(format!("sha256-{sha}"));
    if !gguf::is_gguf(&path) {
        return None;
    }
    let bytes = std::fs::metadata(&path).ok()?.len();
    Some(Installed {
        id: format!("ollama:{name}"),
        name: name.to_string(),
        source: "ollama".into(),
        path,
        bytes,
        quant: None,
        params: None,
        architecture: None,
        context_max: None,
        sha256: Some(sha.to_string()),
        duplicate_group: None,
        catalog_id: None,
        ollama_name: Some(name.to_string()),
        has_projector: layers
            .iter()
            .any(|l| mt(l) == "application/vnd.ollama.image.projector"),
        shape: None,
    })
}

pub fn scan_lmstudio(root: &Path) -> Vec<Installed> {
    let mut files = vec![];
    walk_gguf(root, 0, &mut files);
    files
        .into_iter()
        .filter_map(|p| {
            let rel = p
                .strip_prefix(root)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            Some(Installed {
                id: format!("lmstudio:{rel}"),
                name: rel.clone(),
                source: "lmstudio".into(),
                bytes: split_bytes(&p),
                path: p,
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
            })
        })
        .collect()
}

pub fn scan_hf(root: &Path) -> Vec<Installed> {
    let mut out = vec![];
    for repo in read_dirs(root) {
        let n = file_name(&repo);
        let Some(rest) = n.strip_prefix("models--") else {
            continue;
        };
        let repo_id = rest.replacen("--", "/", 1);
        let mut files = vec![];
        walk_gguf(&repo.join("snapshots"), 0, &mut files);
        for f in files {
            let rel = f
                .strip_prefix(repo.join("snapshots"))
                .ok()
                .and_then(|r| {
                    r.iter()
                        .skip(1)
                        .collect::<PathBuf>()
                        .to_str()
                        .map(str::to_string)
                })
                .unwrap_or_default()
                .replace('\\', "/");
            let id = format!("hf:{repo_id}/{rel}");
            if out.iter().any(|m: &Installed| m.id == id) {
                continue;
            }
            // Snapshot files link to blobs named by their sha256. The blob
            // gives the hash only: llama.cpp finds split parts by their
            // snapshot names, so the model is served and sized from there.
            let blob = std::fs::canonicalize(&f)
                .map(|b| file_name(&b))
                .unwrap_or_default();
            let sha =
                (blob.len() == 64 && blob.chars().all(|c| c.is_ascii_hexdigit())).then_some(blob);
            out.push(Installed {
                id,
                name: format!("{repo_id} {rel}"),
                source: "hf".into(),
                bytes: split_bytes(&f),
                path: f,
                quant: None,
                params: None,
                architecture: None,
                context_max: None,
                sha256: sha,
                duplicate_group: None,
                catalog_id: None,
                ollama_name: None,
                has_projector: false,
                shape: None,
            });
        }
    }
    out
}

fn read_dirs(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(p)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    v.sort();
    v
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// GGUF model files under `dir`, skipping projectors and later split parts.
fn walk_gguf(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 5 {
        return;
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.path());
    for e in entries {
        let p = e.path();
        let n = file_name(&p).to_lowercase();
        if p.is_dir() {
            walk_gguf(&p, depth + 1, out);
        } else if n.ends_with(".gguf") && !n.contains("mmproj") && !is_later_split(&n) {
            out.push(p);
        }
    }
}

/// `-00002-of-00003.gguf` and later parts are loaded through the first one.
fn is_later_split(name: &str) -> bool {
    split_index(name).is_some_and(|(i, _)| i > 1)
}

fn split_index(name: &str) -> Option<(u32, u32)> {
    let stem = name.strip_suffix(".gguf")?;
    let (head, total) = stem.rsplit_once("-of-")?;
    let idx = head.rsplit_once('-')?.1;
    Some((idx.parse().ok()?, total.parse().ok()?))
}

/// Size of a model, summing split parts when `path` is the first part.
fn split_bytes(path: &Path) -> u64 {
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let name = file_name(path);
    let Some((1, total)) = split_index(&name.to_lowercase()) else {
        return size(path);
    };
    let width = name.len() - name.rfind("-of-").unwrap_or(0) - ".gguf".len() - "-of-".len();
    (1..=total)
        .map(|i| {
            let part = name.replacen(
                &format!("{:0width$}-of-", 1, width = width),
                &format!("{:0width$}-of-", i, width = width),
                1,
            );
            size(&path.with_file_name(part))
        })
        .sum()
}

/// Groups copies of the same weights: by sha256 when known, else by exact size
/// and architecture.
pub fn mark_duplicates(models: &mut [Installed]) {
    let sha_key = |s: &str| format!("sha256:{}", &s[..s.len().min(12)]);
    // A file without a hash still matches a hashed one of the same size and architecture.
    let hashed: HashMap<(u64, Option<String>), String> = models
        .iter()
        .filter_map(|m| {
            Some((
                (m.bytes, m.architecture.clone()),
                sha_key(m.sha256.as_deref()?),
            ))
        })
        .collect();
    let keys: Vec<String> = models
        .iter()
        .map(|m| match &m.sha256 {
            Some(s) => sha_key(s),
            None => hashed
                .get(&(m.bytes, m.architecture.clone()))
                .cloned()
                .unwrap_or_else(|| {
                    format!(
                        "size:{}:{}",
                        m.bytes,
                        m.architecture.clone().unwrap_or_default()
                    )
                }),
        })
        .collect();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for k in &keys {
        *counts.entry(k).or_default() += 1;
    }
    for (m, k) in models.iter_mut().zip(&keys) {
        m.duplicate_group = (counts[k.as_str()] > 1).then(|| k.clone());
    }
}

/// Finds a model by id, Ollama name, catalog id or file name.
pub fn resolve<'a>(models: &'a [Installed], want: &str) -> Option<&'a Installed> {
    models
        .iter()
        .find(|m| m.id == want)
        .or_else(|| {
            models
                .iter()
                .find(|m| m.catalog_id.as_deref() == Some(want))
        })
        .or_else(|| {
            models
                .iter()
                .find(|m| m.ollama_name.as_deref() == Some(want))
        })
        .or_else(|| {
            models.iter().find(|m| {
                m.ollama_name
                    .as_deref()
                    .map(|n| n.trim_end_matches(":latest"))
                    == Some(want)
            })
        })
}

/// Downloads a catalog model into Fonsviva's store. Returns bytes fetched now.
pub async fn pull(
    entry: &Entry,
    models_dir: &Path,
    cancel: &Cancel,
    on_progress: impl FnMut(&Progress),
) -> Result<(PathBuf, u64)> {
    let dir = models_dir.join(&entry.id);
    let dest = dir.join(&entry.file);
    let rec_path = dir.join(RECORD);
    if dest.exists() && rec_path.exists() {
        return Ok((dest, 0));
    }
    let free = free_space(models_dir);
    let have = std::fs::metadata(download::part_path(&dest))
        .map(|m| m.len())
        .unwrap_or(0);
    if let Some(free) = free
        && free < entry.bytes.saturating_sub(have)
    {
        bail!(
            "not enough disk space: {} needs {}, {} free",
            entry.name,
            crate::util::human_bytes(entry.bytes - have),
            crate::util::human_bytes(free)
        );
    }
    let fetched = download::fetch(
        &download::http_client(),
        &entry.url(),
        &dest,
        &entry.sha256,
        entry.bytes,
        cancel,
        on_progress,
    )
    .await?;
    let rec = Record {
        id: entry.id.clone(),
        repo: entry.repo.clone(),
        revision: entry.revision.clone(),
        file: entry.file.clone(),
        sha256: entry.sha256.clone(),
        bytes: entry.bytes,
        downloaded_at: crate::util::rfc3339(crate::util::unix_now()),
    };
    tokio::fs::write(&rec_path, serde_json::to_vec_pretty(&rec)?).await?;
    Ok((dest, fetched))
}

/// Deletes a model Fonsviva downloaded (its folder under models/).
pub fn remove_fonsviva(models_dir: &Path, catalog_id: &str) -> Result<()> {
    if catalog_id.is_empty() || catalog_id.contains(['/', '\\']) || catalog_id.contains("..") {
        bail!("bad model id");
    }
    let dir = models_dir.join(catalog_id);
    if !dir.join(RECORD).exists() {
        bail!("{catalog_id} was not downloaded by Fonsviva");
    }
    std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))
}

/// Free bytes on the disk holding `p` (or its nearest existing parent).
pub fn free_space(p: &Path) -> Option<u64> {
    let mut p = p.to_path_buf();
    while !p.exists() {
        p = p.parent()?.to_path_buf();
    }
    let disks = sysinfo::Disks::new_with_refreshed_list().ok()?;
    let canon = std::fs::canonicalize(&p).ok()?;
    (&disks)
        .into_iter()
        .filter(|d| canon.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gguf::tests::llama_like;

    #[test]
    fn scans_ollama_manifests_in_place() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let blob_sha = "ab".repeat(32);
        let proj_sha = "cd".repeat(32);
        std::fs::create_dir_all(root.join("blobs")).unwrap();
        std::fs::write(root.join(format!("blobs/sha256-{blob_sha}")), llama_like()).unwrap();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "layers": [
                {"mediaType": "application/vnd.ollama.image.model", "digest": format!("sha256:{blob_sha}")},
                {"mediaType": "application/vnd.ollama.image.template", "digest": "sha256:00"}
            ]
        });
        for (mdir, host, ns, model, tag) in [
            (
                "manifests",
                "registry.ollama.ai",
                "library",
                "tiny",
                "latest",
            ),
            (
                "manifests-v2",
                "registry.ollama.ai",
                "library",
                "tiny",
                "latest",
            ),
            ("manifests", "hf.co", "Org", "Tiny-GGUF", "Q4_K_M"),
        ] {
            let p = root.join(mdir).join(host).join(ns).join(model);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join(tag), manifest.to_string()).unwrap();
        }
        // A vision model: model layer plus projector.
        let vis = serde_json::json!({"layers": [
            {"mediaType": "application/vnd.ollama.image.model", "digest": format!("sha256:{blob_sha}")},
            {"mediaType": "application/vnd.ollama.image.projector", "digest": format!("sha256:{proj_sha}")}
        ]});
        let p = root.join("manifests/registry.ollama.ai/library/vis");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("latest"), vis.to_string()).unwrap();

        let loc = Locations {
            fonsviva: root.join("none"),
            ollama: vec![root.to_path_buf()],
            ..Default::default()
        };
        let m = scan(&loc);
        let names: Vec<_> = m.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            names,
            [
                "ollama:hf.co/Org/Tiny-GGUF:Q4_K_M",
                "ollama:tiny:latest",
                "ollama:vis:latest"
            ]
        );
        let tiny = &m[1];
        assert_eq!(tiny.source, "ollama");
        assert_eq!(tiny.path, root.join(format!("blobs/sha256-{blob_sha}")));
        assert_eq!(tiny.sha256.as_deref(), Some(blob_sha.as_str()));
        assert_eq!(tiny.quant.as_deref(), Some("Q4_K_M"));
        assert_eq!(tiny.context_max, Some(131072));
        assert!(
            tiny.duplicate_group.is_some(),
            "same blob listed three times"
        );
        assert!(m[2].has_projector && !tiny.has_projector);
        assert_eq!(resolve(&m, "tiny").unwrap().id, "ollama:tiny:latest");
    }

    #[test]
    fn scans_lmstudio_and_hf_cache() {
        let d = tempfile::tempdir().unwrap();
        let lms = d.path().join("lms");
        let pub_dir = lms.join("pub/Tiny-GGUF");
        std::fs::create_dir_all(&pub_dir).unwrap();
        std::fs::write(pub_dir.join("tiny-Q4_K_M.gguf"), llama_like()).unwrap();
        std::fs::write(pub_dir.join("mmproj-tiny-f16.gguf"), llama_like()).unwrap();
        std::fs::write(pub_dir.join("big-00001-of-00002.gguf"), llama_like()).unwrap();
        std::fs::write(pub_dir.join("big-00002-of-00002.gguf"), [0u8; 10]).unwrap();

        let hf = d.path().join("hub");
        let sha = "ef".repeat(32);
        let repo = hf.join("models--Org--Tiny-GGUF");
        std::fs::create_dir_all(repo.join("blobs")).unwrap();
        std::fs::create_dir_all(repo.join("snapshots/abc123")).unwrap();
        std::fs::write(repo.join(format!("blobs/{sha}")), llama_like()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            format!("../../blobs/{sha}"),
            repo.join("snapshots/abc123/tiny-Q4_K_M.gguf"),
        )
        .unwrap();
        #[cfg(not(unix))]
        std::fs::write(repo.join("snapshots/abc123/tiny-Q4_K_M.gguf"), llama_like()).unwrap();

        let loc = Locations {
            fonsviva: d.path().join("none"),
            lmstudio: vec![lms],
            hf: vec![hf],
            ..Default::default()
        };
        let m = scan(&loc);
        let ids: Vec<_> = m.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "lmstudio:pub/Tiny-GGUF/big-00001-of-00002.gguf",
                "lmstudio:pub/Tiny-GGUF/tiny-Q4_K_M.gguf",
                "hf:Org/Tiny-GGUF/tiny-Q4_K_M.gguf"
            ]
        );
        assert_eq!(
            m[0].bytes,
            llama_like().len() as u64 + 10,
            "split parts summed"
        );
        #[cfg(unix)]
        {
            assert_eq!(m[2].sha256.as_deref(), Some(sha.as_str()));
            assert!(m[2].path.ends_with("snapshots/abc123/tiny-Q4_K_M.gguf"));
            // LM Studio's copy has no hash but matches the HF blob by size.
            assert_eq!(m[1].duplicate_group, m[2].duplicate_group);
            assert!(m[1].duplicate_group.is_some());
        }
    }

    #[cfg(unix)]
    #[test]
    fn hf_split_models_are_served_from_the_snapshot() {
        let d = tempfile::tempdir().unwrap();
        let hf = d.path().join("hub");
        let repo = hf.join("models--Org--Big-GGUF");
        let snap = repo.join("snapshots/rev1");
        std::fs::create_dir_all(repo.join("blobs")).unwrap();
        std::fs::create_dir_all(&snap).unwrap();
        let (sha1, sha2) = ("a1".repeat(32), "b2".repeat(32));
        std::fs::write(repo.join(format!("blobs/{sha1}")), llama_like()).unwrap();
        std::fs::write(repo.join(format!("blobs/{sha2}")), [0u8; 10]).unwrap();
        for (name, sha) in [
            ("big-00001-of-00002.gguf", &sha1),
            ("big-00002-of-00002.gguf", &sha2),
        ] {
            std::os::unix::fs::symlink(format!("../../blobs/{sha}"), snap.join(name)).unwrap();
        }
        let m = scan_hf(&hf);
        assert_eq!(m.len(), 1);
        assert_eq!(
            m[0].path,
            snap.join("big-00001-of-00002.gguf"),
            "llama.cpp finds the other parts by name, so serve the snapshot path"
        );
        assert_eq!(m[0].bytes, llama_like().len() as u64 + 10, "both parts");
        assert_eq!(m[0].sha256.as_deref(), Some(sha1.as_str()));
    }

    #[test]
    fn remove_refuses_files_it_did_not_download() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("x")).unwrap();
        assert!(remove_fonsviva(d.path(), "x").is_err());
        assert!(remove_fonsviva(d.path(), "../x").is_err());
        assert!(d.path().join("x").exists());
    }
}
