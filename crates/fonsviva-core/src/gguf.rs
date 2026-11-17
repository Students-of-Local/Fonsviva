//! Reads the metadata section of a GGUF file: enough to know the architecture,
//! layer shapes and context length without loading any weights.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufReader, Read};
use std::path::Path;

const MAGIC: &[u8; 4] = b"GGUF";
/// Numeric arrays longer than this are counted, not kept (vocab scores and the like).
const KEEP_ARRAY_MAX: u64 = 4096;
/// More layers than this is a damaged or crafted file.
const MAX_LAYERS: u64 = 4096;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Ints(Vec<i64>),
    Bools(Vec<bool>),
    /// An array that was skipped: element type and length.
    Skipped(u32, u64),
}

impl Value {
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Int(v) if *v >= 0 => Some(*v as u64),
            Value::Float(f) if *f >= 0.0 => Some(*f as u64),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Raw metadata from the header, keyed by GGUF key.
#[derive(Debug, Clone, Default)]
pub struct Header {
    pub version: u32,
    pub tensor_count: u64,
    pub kv: BTreeMap<String, Value>,
}

pub fn read_header_file(path: &Path) -> Result<Header> {
    let f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    read_header(&mut BufReader::with_capacity(1 << 20, f))
        .with_context(|| format!("read GGUF header of {}", path.display()))
}

/// True when the file starts with the GGUF magic.
pub fn is_gguf(path: &Path) -> bool {
    let mut b = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut b))
        .map(|_| &b == MAGIC)
        .unwrap_or(false)
}

pub fn read_header<R: Read>(r: &mut R) -> Result<Header> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("not a GGUF file");
    }
    let version = read_u32(r)?;
    if !(2..=3).contains(&version) {
        bail!("unsupported GGUF version {version}");
    }
    let tensor_count = read_u64(r)?;
    let kv_count = read_u64(r)?;
    let mut kv = BTreeMap::new();
    for _ in 0..kv_count {
        let key = read_string(r)?;
        let ty = read_u32(r)?;
        let v = read_value(r, ty)?;
        kv.insert(key, v);
    }
    Ok(Header {
        version,
        tensor_count,
        kv,
    })
}

fn read_value<R: Read>(r: &mut R, ty: u32) -> Result<Value> {
    Ok(match ty {
        0 => Value::Int(read_n::<1, _>(r)?[0] as i64),
        1 => Value::Int(read_n::<1, _>(r)?[0] as i8 as i64),
        2 => Value::Int(u16::from_le_bytes(read_n(r)?) as i64),
        3 => Value::Int(i16::from_le_bytes(read_n(r)?) as i64),
        4 => Value::Int(read_u32(r)? as i64),
        5 => Value::Int(i32::from_le_bytes(read_n(r)?) as i64),
        6 => Value::Float(f32::from_le_bytes(read_n(r)?) as f64),
        7 => Value::Bool(read_n::<1, _>(r)?[0] != 0),
        8 => Value::Str(read_string(r)?),
        9 => {
            let ety = read_u32(r)?;
            let n = read_u64(r)?;
            if ety == 8 || ety == 9 || n > KEEP_ARRAY_MAX {
                for _ in 0..n {
                    skip_value(r, ety)?;
                }
                Value::Skipped(ety, n)
            } else if ety == 7 {
                let mut out = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    out.push(read_n::<1, _>(r)?[0] != 0);
                }
                Value::Bools(out)
            } else {
                let mut out = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    match read_value(r, ety)? {
                        Value::Int(i) => out.push(i),
                        Value::Float(f) => out.push(f as i64),
                        _ => bail!("unexpected array element"),
                    }
                }
                Value::Ints(out)
            }
        }
        10 => Value::Int(read_u64(r)? as i64),
        11 => Value::Int(i64::from_le_bytes(read_n(r)?)),
        12 => Value::Float(f64::from_le_bytes(read_n(r)?)),
        other => bail!("unknown GGUF value type {other}"),
    })
}

fn skip_value<R: Read>(r: &mut R, ty: u32) -> Result<()> {
    let n = match ty {
        0 | 1 | 7 => 1,
        2 | 3 => 2,
        4..=6 => 4,
        10..=12 => 8,
        8 => read_u64(r)?,
        9 => {
            let ety = read_u32(r)?;
            let n = read_u64(r)?;
            for _ in 0..n {
                skip_value(r, ety)?;
            }
            return Ok(());
        }
        other => bail!("unknown GGUF value type {other}"),
    };
    std::io::copy(&mut r.take(n), &mut std::io::sink())?;
    Ok(())
}

fn read_n<const N: usize, R: Read>(r: &mut R) -> Result<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b)?;
    Ok(b)
}
fn read_u32<R: Read>(r: &mut R) -> Result<u32> {
    Ok(u32::from_le_bytes(read_n(r)?))
}
fn read_u64<R: Read>(r: &mut R) -> Result<u64> {
    Ok(u64::from_le_bytes(read_n(r)?))
}
fn read_string<R: Read>(r: &mut R) -> Result<String> {
    let n = read_u64(r)?;
    if n > 1 << 24 {
        bail!("string too long ({n} bytes)");
    }
    let mut b = vec![0u8; n as usize];
    r.read_exact(&mut b)?;
    Ok(String::from_utf8_lossy(&b).into_owned())
}

/// The shape facts that decide memory use, read from a GGUF header.
/// The catalog stores these so fit can be computed before a download.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Shape {
    pub architecture: String,
    pub n_layer: u32,
    /// Layers that keep a full-length KV cache.
    pub n_layer_kv_full: u32,
    /// Layers that keep a sliding-window KV cache.
    #[serde(default)]
    pub n_layer_kv_swa: u32,
    #[serde(default)]
    pub sliding_window: u32,
    /// Sum over one full-attention layer of n_head_kv * (key_len + value_len).
    /// Hybrid models with mixed head counts store the average.
    pub kv_dims_per_layer: u32,
    /// Fixed recurrent state (SSM, conv) in bytes, independent of context.
    #[serde(default)]
    pub recurrent_bytes: u64,
    pub context_length: u64,
    #[serde(default)]
    pub n_vocab: u64,
    #[serde(default)]
    pub n_embd: u64,
    #[serde(default)]
    pub quant: Option<String>,
    #[serde(default)]
    pub size_label: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

impl Shape {
    /// KV cache bytes at `ctx` tokens with the default f16 cache (2 bytes per element).
    pub fn kv_bytes(&self, ctx: u64) -> u64 {
        let per_tok = self.kv_dims_per_layer as u64 * 2;
        let full = self.n_layer_kv_full as u64 * per_tok * ctx;
        // llama.cpp keeps window + one micro-batch for sliding layers.
        let swa_tokens = ctx.min(self.sliding_window as u64 + 512);
        let swa = self.n_layer_kv_swa as u64 * per_tok * swa_tokens;
        full + swa + self.recurrent_bytes
    }

    pub fn from_header(h: &Header) -> Result<Shape> {
        let arch =
            h.kv.get("general.architecture")
                .and_then(Value::as_str)
                .context("GGUF has no general.architecture")?
                .to_string();
        let k = |s: &str| h.kv.get(&format!("{arch}.{s}"));
        let num = |s: &str| k(s).and_then(Value::as_u64);
        let n_layer = num("block_count").context("GGUF has no block_count")?;
        // Per-layer tables are sized by this, so a crafted file could ask for
        // billions; real models have a few hundred layers at most.
        if n_layer > MAX_LAYERS {
            bail!("GGUF claims {n_layer} layers; no real model has more than {MAX_LAYERS}");
        }
        let n_layer = n_layer as u32;
        let n_embd = num("embedding_length").unwrap_or(0);
        let per_layer = |v: Option<&Value>, default: u64| -> Vec<u64> {
            match v {
                Some(Value::Ints(xs)) => xs.iter().map(|x| (*x).max(0) as u64).collect(),
                Some(v) => vec![v.as_u64().unwrap_or(default); n_layer as usize],
                None => vec![default; n_layer as usize],
            }
        };
        let heads = per_layer(k("attention.head_count"), 0);
        let max_heads = heads.iter().copied().max().unwrap_or(0);
        let heads_kv = per_layer(k("attention.head_count_kv"), max_heads);
        let head_dim = n_embd.checked_div(max_heads).unwrap_or(0);
        let key_len = num("attention.key_length").unwrap_or(head_dim);
        let value_len = num("attention.value_length").unwrap_or(head_dim);

        let interval = num("full_attention_interval").unwrap_or(0);
        let shared_kv = num("attention.shared_kv_layers").unwrap_or(0);
        let swa_window = num("attention.sliding_window").unwrap_or(0);
        let swa_pattern = k("attention.sliding_window_pattern");
        let is_swa = |l: usize| -> bool {
            if swa_window == 0 {
                return false;
            }
            match swa_pattern {
                Some(Value::Bools(b)) => b.get(l).copied().unwrap_or(false),
                Some(v) => match v.as_u64() {
                    Some(n) if n > 1 => !(l as u64 + 1).is_multiple_of(n),
                    _ => false,
                },
                // llama.cpp sets gpt-oss's pattern in code: every other layer is SWA.
                None if arch == "gpt-oss" => l.is_multiple_of(2),
                None => false,
            }
        };

        let mut full = 0u32;
        let mut swa = 0u32;
        let mut dims_sum = 0u64;
        let mut recurrent_layers = 0u64;
        let kv_layers = (n_layer as u64).saturating_sub(shared_kv) as usize;
        for (l, &kv_heads) in heads_kv.iter().enumerate() {
            let attn = if interval > 0 {
                (l as u64 + 1).is_multiple_of(interval)
            } else {
                kv_heads > 0
            };
            if !attn {
                recurrent_layers += 1;
                continue;
            }
            if l >= kv_layers {
                continue;
            }
            dims_sum += kv_heads * (key_len + value_len);
            if is_swa(l) {
                swa += 1;
            } else {
                full += 1;
            }
        }
        let kv_layers_total = (full + swa) as u64;
        let kv_dims_per_layer = dims_sum.checked_div(kv_layers_total).unwrap_or(0) as u32;

        // Recurrent state: SSM (Mamba, gated delta net) and short-conv layers, kept in f32.
        let mut recurrent_bytes = 0u64;
        if recurrent_layers > 0 {
            let d_inner = num("ssm.inner_size").unwrap_or(0);
            let d_state = num("ssm.state_size").unwrap_or(0);
            let d_conv = num("ssm.conv_kernel")
                .or_else(|| num("shortconv.l_cache"))
                .unwrap_or(0);
            let conv_width = if d_inner > 0 { d_inner } else { n_embd };
            let per_layer = d_inner * d_state + conv_width * d_conv.saturating_sub(1) * 3;
            recurrent_bytes = recurrent_layers * per_layer * 4;
        }

        let n_vocab = match h.kv.get("tokenizer.ggml.tokens") {
            Some(Value::Skipped(_, n)) => *n,
            _ => num("vocab_size").unwrap_or(0),
        };
        Ok(Shape {
            n_layer,
            n_layer_kv_full: full,
            n_layer_kv_swa: swa,
            sliding_window: if swa > 0 { swa_window as u32 } else { 0 },
            kv_dims_per_layer,
            recurrent_bytes,
            context_length: num("context_length").unwrap_or(0),
            n_vocab,
            n_embd,
            quant: h
                .kv
                .get("general.file_type")
                .and_then(Value::as_u64)
                .map(file_type_name),
            size_label: h
                .kv
                .get("general.size_label")
                .and_then(Value::as_str)
                .map(str::to_string),
            name: h
                .kv
                .get("general.name")
                .and_then(Value::as_str)
                .map(str::to_string),
            architecture: arch,
        })
    }
}

/// llama.cpp's `llama_ftype` names.
pub fn file_type_name(t: u64) -> String {
    match t {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        7 => "Q8_0",
        8 => "Q5_0",
        9 => "Q5_1",
        10 => "Q2_K",
        11 => "Q3_K_S",
        12 => "Q3_K_M",
        13 => "Q3_K_L",
        14 => "Q4_K_S",
        15 => "Q4_K_M",
        16 => "Q5_K_S",
        17 => "Q5_K_M",
        18 => "Q6_K",
        19 => "IQ2_XXS",
        20 => "IQ2_XS",
        21 => "Q2_K_S",
        22 => "IQ3_XS",
        23 => "IQ3_XXS",
        24 => "IQ1_S",
        25 => "IQ4_NL",
        26 => "IQ3_S",
        27 => "IQ3_M",
        28 => "IQ2_S",
        29 => "IQ2_M",
        30 => "IQ4_XS",
        31 => "IQ1_M",
        32 => "BF16",
        36 => "TQ1_0",
        37 => "TQ2_0",
        38 => "MXFP4",
        _ => return format!("type{t}"),
    }
    .to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a minimal GGUF header in memory.
    pub struct Builder {
        kv: Vec<u8>,
        n: u64,
    }
    impl Builder {
        pub fn new() -> Self {
            Builder { kv: vec![], n: 0 }
        }
        fn key(&mut self, k: &str, ty: u32) {
            self.kv.extend((k.len() as u64).to_le_bytes());
            self.kv.extend(k.as_bytes());
            self.kv.extend(ty.to_le_bytes());
            self.n += 1;
        }
        pub fn str(mut self, k: &str, v: &str) -> Self {
            self.key(k, 8);
            self.kv.extend((v.len() as u64).to_le_bytes());
            self.kv.extend(v.as_bytes());
            self
        }
        pub fn u32(mut self, k: &str, v: u32) -> Self {
            self.key(k, 4);
            self.kv.extend(v.to_le_bytes());
            self
        }
        pub fn i32s(mut self, k: &str, v: &[i32]) -> Self {
            self.key(k, 9);
            self.kv.extend(5u32.to_le_bytes());
            self.kv.extend((v.len() as u64).to_le_bytes());
            for x in v {
                self.kv.extend(x.to_le_bytes());
            }
            self
        }
        pub fn bools(mut self, k: &str, v: &[bool]) -> Self {
            self.key(k, 9);
            self.kv.extend(7u32.to_le_bytes());
            self.kv.extend((v.len() as u64).to_le_bytes());
            for x in v {
                self.kv.push(*x as u8);
            }
            self
        }
        pub fn strs(mut self, k: &str, v: &[&str]) -> Self {
            self.key(k, 9);
            self.kv.extend(8u32.to_le_bytes());
            self.kv.extend((v.len() as u64).to_le_bytes());
            for s in v {
                self.kv.extend((s.len() as u64).to_le_bytes());
                self.kv.extend(s.as_bytes());
            }
            self
        }
        pub fn build(self) -> Vec<u8> {
            let mut out = b"GGUF".to_vec();
            out.extend(3u32.to_le_bytes());
            out.extend(0u64.to_le_bytes());
            out.extend(self.n.to_le_bytes());
            out.extend(self.kv);
            out
        }
    }

    pub fn llama_like() -> Vec<u8> {
        Builder::new()
            .str("general.architecture", "llama")
            .str("general.name", "Tiny")
            .u32("general.file_type", 15)
            .u32("llama.block_count", 32)
            .u32("llama.embedding_length", 4096)
            .u32("llama.attention.head_count", 32)
            .u32("llama.attention.head_count_kv", 8)
            .u32("llama.context_length", 131072)
            .strs("tokenizer.ggml.tokens", &["a", "b", "c"])
            .build()
    }

    #[test]
    fn reads_dense_shape_and_kv_size() {
        let h = read_header(&mut llama_like().as_slice()).unwrap();
        let s = Shape::from_header(&h).unwrap();
        assert_eq!(s.architecture, "llama");
        assert_eq!(s.n_layer_kv_full, 32);
        // 8 kv heads * (128 + 128)
        assert_eq!(s.kv_dims_per_layer, 2048);
        assert_eq!(s.quant.as_deref(), Some("Q4_K_M"));
        assert_eq!(s.n_vocab, 3);
        // Llama 3 8B at 8192 context: 32 * 2048 * 2 * 8192 = 1 GiB.
        assert_eq!(s.kv_bytes(8192), 1 << 30);
    }

    #[test]
    fn hybrid_counts_only_attention_layers() {
        // LFM2 style: head_count_kv is 0 on conv layers.
        let b = Builder::new()
            .str("general.architecture", "lfm2")
            .u32("lfm2.block_count", 4)
            .u32("lfm2.embedding_length", 1024)
            .u32("lfm2.attention.head_count", 16)
            .i32s("lfm2.attention.head_count_kv", &[0, 8, 0, 8])
            .u32("lfm2.shortconv.l_cache", 3)
            .u32("lfm2.context_length", 32768)
            .build();
        let s = Shape::from_header(&read_header(&mut b.as_slice()).unwrap()).unwrap();
        assert_eq!(s.n_layer_kv_full, 2);
        assert_eq!(s.kv_dims_per_layer, 8 * 128);
        assert!(s.recurrent_bytes > 0);

        // Qwen3-Next style: every 4th layer is full attention.
        let b = Builder::new()
            .str("general.architecture", "qwen35")
            .u32("qwen35.block_count", 8)
            .u32("qwen35.embedding_length", 1024)
            .u32("qwen35.attention.head_count", 8)
            .u32("qwen35.attention.head_count_kv", 2)
            .u32("qwen35.attention.key_length", 256)
            .u32("qwen35.attention.value_length", 256)
            .u32("qwen35.full_attention_interval", 4)
            .build();
        let s = Shape::from_header(&read_header(&mut b.as_slice()).unwrap()).unwrap();
        assert_eq!(s.n_layer_kv_full, 2);
        assert_eq!(s.kv_dims_per_layer, 1024);
    }

    #[test]
    fn sliding_window_layers_cap_at_window() {
        let b = Builder::new()
            .str("general.architecture", "gemma3")
            .u32("gemma3.block_count", 6)
            .u32("gemma3.embedding_length", 1024)
            .u32("gemma3.attention.head_count", 4)
            .u32("gemma3.attention.head_count_kv", 1)
            .u32("gemma3.attention.key_length", 256)
            .u32("gemma3.attention.value_length", 256)
            .u32("gemma3.attention.sliding_window", 1024)
            .u32("gemma3.attention.sliding_window_pattern", 6)
            .build();
        let s = Shape::from_header(&read_header(&mut b.as_slice()).unwrap()).unwrap();
        assert_eq!((s.n_layer_kv_full, s.n_layer_kv_swa), (1, 5));
        let at_32k = s.kv_bytes(32768);
        assert_eq!(at_32k, 512 * 2 * 32768 + 5 * 512 * 2 * 1536);

        let b = Builder::new()
            .str("general.architecture", "gemma4")
            .u32("gemma4.block_count", 3)
            .u32("gemma4.embedding_length", 512)
            .u32("gemma4.attention.head_count", 2)
            .u32("gemma4.attention.sliding_window", 512)
            .bools(
                "gemma4.attention.sliding_window_pattern",
                &[true, true, false],
            )
            .build();
        let s = Shape::from_header(&read_header(&mut b.as_slice()).unwrap()).unwrap();
        assert_eq!((s.n_layer_kv_full, s.n_layer_kv_swa), (1, 2));
    }

    #[test]
    fn a_crafted_layer_count_is_refused_before_allocating() {
        let b = Builder::new()
            .str("general.architecture", "llama")
            .u32("llama.block_count", 1_000_000)
            .u32("llama.attention.head_count", 32)
            .build();
        let h = read_header(&mut b.as_slice()).unwrap();
        assert!(Shape::from_header(&h).is_err());
    }

    #[test]
    fn rejects_non_gguf() {
        assert!(read_header(&mut &b"PK\x03\x04rest"[..]).is_err());
    }
}
