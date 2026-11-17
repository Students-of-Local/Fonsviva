# Decisions

Each entry records what was decided, why, and what would change it. Newest first.

## 2026-11-17: Fonsviva is a standalone app and Local Studio's built-in engine

**Decided by** the Fonsviva lead, from Donovan's brief of 2026-11-17: Fonsviva installs on its own like Silas and Tychicus, sets up and runs local models, wires them into T3 Code (through pi), Local Studio and anything OpenAI-compatible, adds RunPod, works on Windows, macOS, Linux and (as a remote) iOS, and ships stable, nightly and dev.

This replaces the v0 plan in the README that ran Fonsviva on top of the Local Studio controller. The controller stays useful for vLLM and SGLang fleets; Fonsviva now owns the path from a clean machine to a first local answer.

### Shape

- **`fonsviva` binary** (Rust): CLI plus `fonsviva serve`, the daemon on `127.0.0.1:7341`. One binary per OS and arch, no runtime to install.
- **Desktop app** (Tauri v2, like Silas): the window, tray and installer. It starts or attaches to the daemon and draws everything from its API. Windows, macOS and Linux.
- **Crates**: `fonsviva-core` (hardware, catalog, fit, store, engines, measurements), `fonsviva-wire` (one-click, reversible config writers), `fonsviva-runpod` (RunPod client, cost, idle stop, mock server).
- **Engines** behind one trait: llama.cpp (`llama-server`, downloaded and managed by Fonsviva, Metal, CUDA, Vulkan or CPU build chosen from detected hardware) and Ollama (adopted when already installed). Others only where a measurement shows they win on that hardware.
- **One model store** per machine, owned by whichever Fonsviva daemon holds the data dir lock. Ollama's blobs are GGUF files, so llama.cpp loads them in place instead of copying them. When upstream llama.cpp cannot load one (Ollama's own engine runs some architectures, and vision models keep a separate projector blob), Fonsviva serves that model through Ollama instead and says so.
- **Local Studio** bundles the same `fonsviva` binary as a built-in plugin. If a standalone Fonsviva already runs, Local Studio attaches to it; otherwise it starts the bundled one on the same data dir. Either way a person has one store, not two.
- **iOS** never runs models. The Local Studio iPhone app picks and watches them through Local Studio Remote, which forwards the Fonsviva API.

### Why llama.cpp first, Ollama second

- llama.cpp lets Fonsviva set context length, GPU layers and batch size per launch and report them back. Ollama's default context depends on VRAM (4096 below 24 GiB per its docs), which silently broke a fleet plugin check (field note "Eukhe agent turns ... fail with a 4096-token context"). When Fonsviva drives Ollama it sends `num_ctx` on every request and shows it.
- llama-server reports prompt and generation timings in every response, so speed shown in the app is measured, not estimated.
- Ollama stays because many people already have models in it; Fonsviva lists and serves them without a second download.

### Why a daemon with an HTTP API

The desktop app, Local Studio, the iPhone app, the CLI and agents all need the same state. One local API (docs/API.md) keeps them in agreement and lets Local Studio Remote forward it to a phone unchanged.

### What would change this

- A measured case where another engine (MLX on Apple Silicon, vLLM on a dedicated GPU) is faster at equal quality for the recommended models: add it behind the engine trait.
- The Local Studio core lead preferring to link `fonsviva-core` as a crate instead of the binary: the API stays the contract either way.
