//! The curated catalog (`catalog/models.json`) and the fit check: will this
//! model, at this context, fit in this machine's memory?

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::gguf::Shape;
use crate::hardware::Hardware;
use crate::util::{GIB, MIB, human_bytes};

const CATALOG_JSON: &str = include_str!("../../../catalog/models.json");

/// Contexts tried, largest first. 32768 is the default when it fits.
pub const CONTEXT_STEPS: [u64; 5] = [32768, 16384, 8192, 4096, 2048];
pub const DEFAULT_CONTEXT: u64 = 32768;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub schema: u32,
    pub updated: String,
    pub models: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub name: String,
    /// chat or code
    pub role: String,
    pub params: String,
    #[serde(default)]
    pub default: bool,
    pub repo: String,
    pub revision: String,
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
    pub quant: String,
    pub license: String,
    pub summary: String,
    pub shape: Shape,
}

impl Entry {
    pub fn url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repo, self.revision, self.file
        )
    }
}

pub fn catalog() -> Catalog {
    parse(CATALOG_JSON).expect("catalog/models.json is checked by tests")
}

pub fn parse(json: &str) -> Result<Catalog> {
    serde_json::from_str(json).context("parse catalog")
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Fits,
    Tight,
    No,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Fit {
    pub fit: Verdict,
    pub reason: String,
    pub context: u64,
    /// Total memory needed: weights + KV cache + overhead.
    pub need_bytes: u64,
    pub weights_bytes: u64,
    pub kv_bytes: u64,
    pub overhead_bytes: u64,
    /// Memory the model may use on this machine.
    pub budget_bytes: u64,
    /// gpu, unified, split or cpu
    pub placement: String,
    /// Layers to offload: all of them on GPU and unified memory, a share when split, 0 on CPU.
    pub gpu_layers: u32,
}

/// Runtime and compute buffers: a base, the logits buffer for a 512-token
/// micro-batch, and a share of the weights for scratch.
pub fn overhead_bytes(shape: &Shape, weights: u64) -> u64 {
    192 * MIB + shape.n_vocab * 512 * 4 + weights / 50
}

/// Fit at one context.
pub fn fit_at(shape: &Shape, weights: u64, hw: &Hardware, ctx: u64) -> Fit {
    let kv = shape.kv_bytes(ctx);
    let overhead = overhead_bytes(shape, weights);
    let need = weights + kv + overhead;
    let n_layer = shape.n_layer + 1;
    let gb = human_bytes;

    let (fit, budget, placement, gpu_layers, reason);
    if hw.unified_memory {
        // macOS lets Metal wire about 70% of RAM by default.
        let metal = hw.ram_total / 10 * 7;
        budget = metal.min(hw.ram_available + need.min(hw.ram_total / 4));
        placement = "unified";
        gpu_layers = n_layer;
        (fit, reason) = verdict(need, budget, "unified memory");
    } else if let Some(g) = hw.best_gpu() {
        let vram = g.vram_free.unwrap_or(g.vram_total.unwrap_or(0) / 100 * 95);
        if need <= vram / 10 * 9 {
            fit = Verdict::Fits;
            budget = vram;
            placement = "gpu";
            gpu_layers = n_layer;
            reason = format!(
                "Fits on the {} with room to spare: needs {} of {} free.",
                g.name,
                gb(need),
                gb(vram)
            );
        } else if need <= vram + hw.ram_available / 10 * 8 {
            fit = Verdict::Tight;
            budget = vram + hw.ram_available;
            placement = "split";
            let on_gpu = (vram / 10 * 9).saturating_sub(kv + overhead);
            gpu_layers = ((on_gpu as f64 / weights.max(1) as f64) * shape.n_layer as f64) as u32;
            reason = format!(
                "Needs {}, more than the {} free on the {}, so part runs on the CPU and it will be slower.",
                gb(need),
                gb(vram),
                g.name
            );
        } else {
            fit = Verdict::No;
            budget = vram + hw.ram_available;
            placement = "split";
            gpu_layers = 0;
            reason = format!(
                "Needs {}; the GPU has {} free and RAM has {} free.",
                gb(need),
                gb(vram),
                gb(hw.ram_available)
            );
        }
    } else {
        budget = hw.ram_available;
        placement = "cpu";
        gpu_layers = 0;
        (fit, reason) = verdict(need, budget, "free RAM");
    }
    Fit {
        fit,
        reason,
        context: ctx,
        need_bytes: need,
        weights_bytes: weights,
        kv_bytes: kv,
        overhead_bytes: overhead,
        budget_bytes: budget,
        placement: placement.into(),
        gpu_layers,
    }
}

fn verdict(need: u64, budget: u64, what: &str) -> (Verdict, String) {
    let (n, b) = (human_bytes(need), human_bytes(budget));
    if need <= budget / 10 * 8 {
        (Verdict::Fits, format!("Fits: needs {n} of {b} {what}."))
    } else if need <= budget {
        (
            Verdict::Tight,
            format!("Tight: needs {n} of {b} {what}; close other apps first."),
        )
    } else {
        (
            Verdict::No,
            format!("Doesn't fit: needs {n}, only {b} {what}."),
        )
    }
}

/// Picks the context: the requested one, else 32768 when it fits, else the
/// largest step that fits (then the largest that is tight).
pub fn fit_best(shape: &Shape, weights: u64, hw: &Hardware, requested: Option<u64>) -> Fit {
    let max = if shape.context_length > 0 {
        shape.context_length
    } else {
        DEFAULT_CONTEXT
    };
    if let Some(c) = requested {
        return fit_at(shape, weights, hw, c.min(max));
    }
    let steps: Vec<u64> = CONTEXT_STEPS.iter().map(|c| (*c).min(max)).collect();
    let fits: Vec<Fit> = steps
        .iter()
        .map(|c| fit_at(shape, weights, hw, *c))
        .collect();
    fits.iter()
        .find(|f| f.fit == Verdict::Fits)
        .or_else(|| fits.iter().find(|f| f.fit == Verdict::Tight))
        .cloned()
        .unwrap_or_else(|| fits[0].clone())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recommendation {
    #[serde(flatten)]
    pub entry: Entry,
    #[serde(flatten)]
    pub fit: Fit,
    /// Shown as a choice. Only models that fit are recommended.
    pub recommended: bool,
    /// The first model setup downloads on this machine.
    pub setup_default: bool,
    /// Measured on this machine by `POST /api/v1/bench`, when available.
    pub measured: Option<serde_json::Value>,
}

/// Every catalog model with its fit. Recommended ones come first, largest first.
pub fn recommend(cat: &Catalog, hw: &Hardware) -> Vec<Recommendation> {
    let mut out: Vec<Recommendation> = cat
        .models
        .iter()
        .map(|e| {
            let fit = fit_best(&e.shape, e.bytes, hw, None);
            Recommendation {
                recommended: fit.fit == Verdict::Fits,
                setup_default: false,
                measured: None,
                entry: e.clone(),
                fit,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.recommended
            .cmp(&a.recommended)
            .then(b.entry.bytes.cmp(&a.entry.bytes))
    });
    // Setup's first model: the catalog default when it fits, else the
    // smallest model that fits. Never a model that doesn't fit.
    let pick = out
        .iter()
        .position(|r| r.entry.default && r.recommended)
        .or_else(|| {
            out.iter()
                .enumerate()
                .filter(|(_, r)| r.recommended)
                .min_by_key(|(_, r)| r.entry.bytes)
                .map(|(i, _)| i)
        });
    if let Some(i) = pick {
        out[i].setup_default = true;
    }
    out
}

/// Plain-words size of a context for people.
pub fn context_words(ctx: u64) -> String {
    format!("{} tokens (about {} pages)", ctx, (ctx / 600).max(1))
}

/// Total RAM shown in GiB-rounded words, used in notes.
pub fn ram_words(hw: &Hardware) -> String {
    format!(
        "{} GB RAM, {} free",
        hw.ram_total.div_ceil(GIB),
        human_bytes(hw.ram_available)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{Cpu, Gpu};

    fn cpu_box(avail_gib: u64) -> Hardware {
        Hardware {
            os: "linux".into(),
            os_version: None,
            arch: "x86_64".into(),
            cpu: Cpu {
                brand: "x".into(),
                physical_cores: 8,
                logical_cores: 8,
            },
            ram_total: 24 * GIB,
            ram_available: avail_gib * GIB,
            gpus: vec![],
            unified_memory: false,
            backend: "cpu".into(),
            notes: vec![],
        }
    }

    fn gpu_box(vram_gib: u64) -> Hardware {
        let mut h = cpu_box(16);
        h.gpus.push(Gpu {
            vendor: "nvidia".into(),
            name: "RTX".into(),
            vram_total: Some(vram_gib * GIB),
            vram_free: Some(vram_gib * GIB),
            backend: "cuda".into(),
            integrated: false,
            driver: None,
            source: "test".into(),
        });
        h.backend = "cuda".into();
        h
    }

    #[test]
    fn catalog_is_valid() {
        let c = catalog();
        assert!((6..=10).contains(&c.models.len()));
        let defaults: Vec<_> = c.models.iter().filter(|m| m.default).collect();
        assert_eq!(defaults.len(), 1, "exactly one default");
        assert!(defaults[0].bytes < 1_000_000_000, "default is under 1 GB");
        assert!(c.models.iter().any(|m| m.role == "code"));
        let mut ids = std::collections::HashSet::new();
        for m in &c.models {
            assert!(ids.insert(&m.id), "duplicate id {}", m.id);
            assert_eq!(m.sha256.len(), 64, "{}", m.id);
            assert_eq!(m.revision.len(), 40, "{}", m.id);
            assert!(m.file.ends_with(".gguf"));
            assert!(
                m.shape.n_layer > 0 && m.shape.context_length > 0,
                "{}",
                m.id
            );
            assert!(m.shape.kv_dims_per_layer > 0, "{}", m.id);
            assert!(
                matches!(m.license.as_str(), "apache-2.0" | "mit"),
                "{} has a non-permissive license",
                m.id
            );
            assert!(!m.summary.contains('\u{2014}') && !m.summary.contains('\u{2013}'));
        }
    }

    #[test]
    fn never_recommends_a_model_that_does_not_fit() {
        let c = catalog();
        for hw in [
            cpu_box(1),
            cpu_box(3),
            cpu_box(6),
            cpu_box(40),
            gpu_box(8),
            gpu_box(24),
        ] {
            let recs = recommend(&c, &hw);
            for r in &recs {
                if r.recommended || r.setup_default {
                    assert_eq!(r.fit.fit, Verdict::Fits, "{} on {:?}", r.entry.id, hw.gpus);
                    assert!(r.fit.need_bytes <= r.fit.budget_bytes);
                }
            }
            assert!(recs.iter().filter(|r| r.setup_default).count() <= 1);
        }
        // 1 GiB free: nothing fits, nothing is recommended, setup has no pick.
        let recs = recommend(&c, &cpu_box(1));
        assert!(recs.iter().all(|r| !r.recommended && !r.setup_default));
        // A 30B coder never fits in 6 GiB of RAM.
        let recs = recommend(&c, &cpu_box(6));
        let big = recs.iter().find(|r| r.entry.id.contains("30b")).unwrap();
        assert_eq!(big.fit.fit, Verdict::No);
        assert!(!big.recommended);
        assert!(big.fit.reason.starts_with("Doesn't fit"));
    }

    #[test]
    fn default_context_is_32k_when_it_fits_else_largest() {
        let c = catalog();
        let tiny = c.models.iter().find(|m| m.default).unwrap();
        let f = fit_best(&tiny.shape, tiny.bytes, &cpu_box(16), None);
        assert_eq!(f.context, 32768);
        assert_eq!(f.fit, Verdict::Fits);

        // Tight memory: drops to a smaller context instead of failing.
        let s = Shape {
            architecture: "llama".into(),
            n_layer: 32,
            n_layer_kv_full: 32,
            kv_dims_per_layer: 2048,
            context_length: 131072,
            ..Default::default()
        };
        let f = fit_best(&s, 4 * GIB, &cpu_box(7), None);
        assert!(f.context < 32768, "got {}", f.context);
        assert_eq!(f.fit, Verdict::Fits);
        // Never above what the model supports.
        let small = Shape {
            context_length: 8192,
            ..s
        };
        assert_eq!(fit_best(&small, GIB, &cpu_box(40), None).context, 8192);
    }

    #[test]
    fn gpu_split_is_tight_with_partial_layers() {
        let s = Shape {
            architecture: "llama".into(),
            n_layer: 32,
            n_layer_kv_full: 32,
            kv_dims_per_layer: 2048,
            context_length: 131072,
            ..Default::default()
        };
        let f = fit_at(&s, 10 * GIB, &gpu_box(8), 8192);
        assert_eq!(f.fit, Verdict::Tight);
        assert_eq!(f.placement, "split");
        assert!(f.gpu_layers > 0 && f.gpu_layers < 32);
        let f = fit_at(&s, 4 * GIB, &gpu_box(24), 8192);
        assert_eq!((f.fit, f.gpu_layers), (Verdict::Fits, 33));
    }

    #[test]
    fn a_cpu_backend_fits_against_ram_only() {
        // An AMD card with no Vulkan loader: the GPU is listed, but llama.cpp
        // runs on the CPU, so its VRAM must not count.
        let mut h = cpu_box(8);
        h.gpus.push(Gpu {
            vendor: "amd".into(),
            name: "Radeon".into(),
            vram_total: Some(24 * GIB),
            vram_free: Some(24 * GIB),
            backend: "cpu".into(),
            integrated: false,
            driver: None,
            source: "test".into(),
        });
        assert_eq!(h.backend, "cpu");
        let s = Shape {
            architecture: "llama".into(),
            n_layer: 32,
            n_layer_kv_full: 32,
            kv_dims_per_layer: 2048,
            context_length: 131072,
            ..Default::default()
        };
        let f = fit_at(&s, 12 * GIB, &h, 8192);
        assert_eq!(f.fit, Verdict::No, "{}", f.reason);
        assert_eq!(f.gpu_layers, 0);
        assert_eq!(f.placement, "cpu");
        assert!(h.best_gpu().is_none());
    }
}
