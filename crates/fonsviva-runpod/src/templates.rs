//! The pod templates Fonsviva supports: two OpenAI-compatible servers, each
//! serving one model id on one HTTP port, with the model files on a volume
//! so a stopped pod restarts without downloading them again.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::client::{CreatePod, Mounts, PersistentMount, PodGpu};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PodTemplate {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// Pinned by tag or digest so a pod never pulls an untested build.
    pub image: &'static str,
    pub entrypoint: &'static [&'static str],
    pub port: u16,
    /// The server reads its API key from this variable.
    pub key_env: &'static str,
    pub default_model: &'static str,
    /// The smallest GPU this template offers.
    pub min_vram_gb: u32,
    pub container_disk_gb: u32,
    pub volume_gb: u32,
    pub volume_path: &'static str,
    pub env: &'static [(&'static str, &'static str)],
    pub context_window: u32,
}

/// The llama.cpp server image published by ggml-org for CUDA 12. RunPod's
/// per-build tags are not kept, so the image is pinned by digest.
pub const LLAMA_CPP_IMAGE: &str = "ghcr.io/ggml-org/llama.cpp:server-cuda12@sha256:ae96e52f8b2106ea82bad786ee2d08bb9b0a8c93403df9c27ae1bf065a118089";
pub const VLLM_IMAGE: &str = "vllm/vllm-openai:v0.33.0";

pub const TEMPLATES: [PodTemplate; 2] = [
    PodTemplate {
        id: "llama-cpp",
        name: "llama.cpp server (GGUF)",
        description: "llama-server with an OpenAI-compatible API. The model is a Hugging Face GGUF repo, optionally with a quant tag, such as ggml-org/gpt-oss-20b-GGUF or unsloth/Qwen3-8B-GGUF:Q4_K_M.",
        image: LLAMA_CPP_IMAGE,
        entrypoint: &["/app/llama-server"],
        port: 8080,
        key_env: "LLAMA_API_KEY",
        default_model: "ggml-org/gpt-oss-20b-GGUF",
        min_vram_gb: 24,
        container_disk_gb: 20,
        volume_gb: 50,
        volume_path: "/workspace",
        env: &[("LLAMA_CACHE", "/workspace/llama.cpp")],
        context_window: 32768,
    },
    PodTemplate {
        id: "vllm",
        name: "vLLM OpenAI server",
        description: "vLLM's OpenAI-compatible server for a Hugging Face model id in full precision, such as Qwen/Qwen3-8B.",
        image: VLLM_IMAGE,
        entrypoint: &["vllm", "serve"],
        port: 8000,
        key_env: "VLLM_API_KEY",
        default_model: "Qwen/Qwen3-8B",
        min_vram_gb: 48,
        container_disk_gb: 30,
        volume_gb: 80,
        volume_path: "/workspace",
        env: &[("HF_HOME", "/workspace/hf")],
        context_window: 32768,
    },
];

pub fn template(id: &str) -> Option<&'static PodTemplate> {
    TEMPLATES.iter().find(|template| template.id == id)
}

impl PodTemplate {
    /// The server's arguments for `model`, binding every interface so
    /// RunPod's proxy can reach it.
    pub fn cmd(&self, model: &str) -> Vec<String> {
        let context = self.context_window.to_string();
        let args: Vec<&str> = match self.id {
            "llama-cpp" => vec![
                "-hf", model, "--host", "0.0.0.0", "--port", "8080", "-c", &context, "--jinja",
                "-ngl", "999",
            ],
            _ => vec![
                model,
                "--host",
                "0.0.0.0",
                "--port",
                "8000",
                "--max-model-len",
                &context,
                "--served-model-name",
                model,
            ],
        };
        args.into_iter().map(str::to_string).collect()
    }

    /// The `POST /v2/pods` body for this template on one `gpu`. `pod_key` is
    /// the key the server will require; Fonsviva's proxy sends it.
    pub fn create_request(&self, name: &str, gpu: &str, model: &str, pod_key: &str) -> CreatePod {
        let mut env: BTreeMap<String, String> = self
            .env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        env.insert(self.key_env.to_string(), pod_key.to_string());
        CreatePod {
            name: name.to_string(),
            image: self.image.to_string(),
            entrypoint: self
                .entrypoint
                .iter()
                .map(|part| part.to_string())
                .collect(),
            cmd: self.cmd(model),
            disk: self.container_disk_gb,
            env,
            ports: vec![format!("{}/http", self.port)],
            cloud: "SECURE".to_string(),
            gpu: PodGpu {
                id: gpu.to_string(),
                count: 1,
            },
            mounts: Mounts {
                persistent: Some(PersistentMount {
                    size: self.volume_gb,
                    path: self.volume_path.to_string(),
                }),
            },
        }
    }
}

/// A model id is passed to the server as an argument, so it must look like
/// a Hugging Face repo (`owner/name`, optionally `:tag`) and nothing else.
pub fn valid_model(model: &str) -> bool {
    let (repo, tag) = model.split_once(':').unwrap_or((model, ""));
    let part_ok = |part: &str| {
        !part.is_empty()
            && !part.starts_with(['-', '.'])
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    let mut pieces = repo.split('/');
    let shape = matches!((pieces.next(), pieces.next(), pieces.next()), (Some(owner), Some(name), None) if part_ok(owner) && part_ok(name));
    shape && (tag.is_empty() || part_ok(tag)) && model.len() <= 200
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_cannot_smuggle_arguments() {
        assert!(valid_model("Qwen/Qwen3-8B"));
        assert!(valid_model("unsloth/Qwen3-8B-GGUF:Q4_K_M"));
        assert!(!valid_model("--model evil"));
        assert!(!valid_model("Qwen/Qwen3 8B"));
        assert!(!valid_model("a/b/c"));
        assert!(!valid_model("-x/y"));
        assert!(!valid_model("x/y:--api-key"));
    }

    #[test]
    fn create_request_carries_the_key_only_in_env() {
        let body = template("vllm").unwrap().create_request(
            "fonsviva-abc",
            "NVIDIA L40S",
            "Qwen/Qwen3-8B",
            "pod-secret",
        );
        assert_eq!(
            body.env.get("VLLM_API_KEY").map(String::as_str),
            Some("pod-secret")
        );
        assert!(!format!("{body:?}").contains("pod-secret"));
        assert_eq!(body.ports, ["8000/http"]);
        assert_eq!(body.cmd[0], "Qwen/Qwen3-8B");
    }
}
