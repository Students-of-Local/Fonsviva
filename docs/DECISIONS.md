# Decisions

Each entry records what was decided, why, and what would change it. Newest first.

## 2026-11-26: Fonsviva is a platform for organizations, with a node daemon and an org server

**Decided by** the owner, 2026-11-26. Fonsviva is no longer a single-user app with app profiles; it is an open source platform for organizations (universities, companies, libraries, community hubs) to share their own compute. Design: [ARCHITECTURE](ARCHITECTURE.md).

- **Two processes, one binary.** The node daemon (`fonsviva serve`, 127.0.0.1:7341, one per machine) keeps the engines, store, node channel and RunPod. The org server (`fonsviva org serve`) serves the web app and the org API at the org's URL.
- **The agent loop runs on the server.** The org brain's tools (search, artifacts, memory, compute requests) are server code that checks the person's role on every call.
- **One SQLite store** on the org server for members, sessions, invites and the audit log, and later threads, memory and costs.
- The profile model (personal, client and institutional profiles bound to app keys) and the personal-use-only cloud rule are replaced: cloud is off until an admin adds a provider, with a plain notice of what leaves the org's machines, and the org's privacy settings decide what is stored and who sees it.

### Why

- **Permissions in code.** A prompt can be talked out of a rule; a route handler can't. With the loop and its tools on the server, the model only ever sees what the server already cleared for that person.
- **One store.** One place for members, roles, privacy settings and costs means one place to enforce and audit them, instead of state split across clients.

**What would change this:** an org that needs its data split across servers it doesn't share, or a measured case where running the loop on the client is needed for speed.

## 2026-11-17: Running a model across devices is planned from measurements

**Decided by** the Local Studio iOS lead (Claude), from Donovan's goal of 2026-11-17 to run models across devices very well, by cable or network, with registry recipes and RunPod. Design: [DISTRIBUTED](DISTRIBUTED.md).

- One machine first; a split is offered only when its measured tok/s wins, or when no single machine can hold the model.
- Layer (pipeline) splits over ordinary links, through llama.cpp RPC from the pinned release; tensor splits only over RDMA.
- `ggml-rpc-server` binds to 127.0.0.1 only; the only way between machines is Fonsviva's paired, encrypted node channel. Pairing trusts every local process on both machines, and the channel's listeners exist only while a plan runs.
- RunPod runs whole models only, and gains a pod-side idle stop and proxy join reused from Hildegard's RunPod boot script.
- Ratchet tests on `benchmarks/results/topology/` keep the planner on the measured winner and catch silent slowdowns.

**What would change this:** a measured case where a tensor split over a non-RDMA link wins, or a RunPod setup where a home plus pod split beats the pod alone.

## 2026-11-17: Fonsviva is a standalone app and Local Studio's built-in engine

**Decided by** the Fonsviva lead, from Donovan's brief of 2026-11-17: Fonsviva installs on its own like Silas and Tychicus, sets up and runs local models, wires them into T3 Code (through pi), Local Studio and anything OpenAI-compatible, adds RunPod, works on Windows, macOS, Linux and (as a remote) iOS, and ships stable, nightly and dev.

This replaces the v0 plan in the README that ran Fonsviva on top of the Local Studio controller. The controller stays useful for vLLM and SGLang fleets; Fonsviva now owns the path from a clean machine to a first local answer.

### Shape

- **`fonsviva` binary** (Rust): CLI plus `fonsviva serve`, the daemon on `127.0.0.1:7341`. One binary per OS and arch, no runtime to install.
- **Desktop app** (Tauri v2, like Silas): the window, tray and installer. It starts or attaches to the daemon and draws everything from its API. Windows, macOS and Linux.
- **Crates**: `fonsviva-core` (hardware, catalog, fit, store, engines, measurements), `fonsviva-wire` (one-click, reversible config writers), `fonsviva-runpod` (RunPod client, cost, idle stop, mock server).
- **Engines** behind one trait: llama.cpp (`llama-server`, downloaded and managed by Fonsviva, Metal, CUDA, Vulkan or CPU build chosen from detected hardware) and Ollama (adopted when already installed). Others only where a measurement shows they win on that hardware.
- **One model store** per machine, owned by whichever Fonsviva daemon holds the data dir lock. Ollama's blobs are GGUF files, so llama.cpp loads them in place instead of copying them. When upstream llama.cpp cannot load one (Ollama's own engine runs some architectures, and vision models keep a separate projector blob), Fonsviva serves that model through Ollama instead and says so.
- **Local Studio** bundles the same `fonsviva` binary as a built-in plugin. If a standalone Fonsviva of the same channel already runs, Local Studio attaches to it; otherwise it starts the newest installed `fonsviva` of its channel, bundled or standalone, on the same data dir. Either way a person has one store, not two.
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
