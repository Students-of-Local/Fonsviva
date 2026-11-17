# AGENTS.md

Instructions for AI coding agents (Claude Code, Codex, Cursor and others) working in Fonsviva.

## What this project is

A standalone app (Windows, macOS, Linux) that takes a clean machine to a first local model answer faster than any rival, wires that model into T3 Code (through pi), Local Studio and anything OpenAI-compatible in one reversible click, and rents RunPod GPUs with the cost shown before and while they run. The same binary is Local Studio's built-in engine, so one machine keeps one model store. It is also the shared model-serving layer for UntoCaesar, Gantrel and Corunis. Shape and reasons: `docs/DECISIONS.md`. The API every client uses: `docs/API.md`.

## Code layout

- `crates/fonsviva`: the `fonsviva` binary (CLI, and `fonsviva serve`, the daemon on 127.0.0.1:7341)
- `crates/fonsviva-core`: hardware detection, catalog and fit, model store, engines (llama.cpp, Ollama), measurements
- `crates/fonsviva-wire`: reversible config writers for pi, T3 Code, Eukhe (Local Studio) and the OpenAI endpoint
- `crates/fonsviva-runpod`: RunPod client, cost, idle auto-stop, and the mock RunPod server tests use
- `app/`: the Tauri v2 desktop app (UI only; all state comes from the daemon API)

## Check

`cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`. CI runs it on Linux, macOS and Windows. On shared fleet hosts build with `CARGO_BUILD_JOBS=3`.

Tests never touch real user state: set `FONSVIVA_HOME`, `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME` (and `APPDATA`/`LOCALAPPDATA` on Windows) to a scratch dir, and `FONSVIVA_PORT=0`. Tests never start a real RunPod pod; they use the mock in `fonsviva-runpod`. A real pod start costs money and needs the owner's yes for that exact charge.

## Ground rules

- **Build on, don't rebuild.** Use Local Studio, the Local AI registry, harness-bridge and the engines (vLLM, SGLang, llama.cpp) as they are. Contribute fixes upstream where they belong. Only build what Fonsviva adds.
- **Local first.** Every feature works on the user's own hardware with no cloud account.
- **Cloud is personal use only.** Cloud providers (RunPod and others) are off by default, available only on a personal profile, and hard-blocked for UntoCaesar client or volunteer work and for Corunis institutional data. Fonsviva is the only place cloud providers, their keys and costs live; the apps never call a provider directly. Follow `docs/CLOUD.md` exactly. Never weaken this block, and never add a setting that bypasses it.
- **Private and secure by default.** API keys are scoped per app and stored in the OS keychain or an encrypted store, never in plain config.
- **Never burn money.** Cloud spend protection in `docs/COSTS.md` is always on. Never add a way around budget caps or auto-stop. Never log, print or send secrets, prompts or outputs.
- **Don't break the apps that depend on it.** API changes stay backward compatible. A breaking change needs a new version path and a migration plan.
- **Easy beats clever.** Every screen has to make sense to a non-technical person.
- **Design comes from DESIGN.md.** Follow it for all UI work. If it is empty or silent on something, ask the designer rather than inventing a style.
- **Prove it works.** Back every change with behavior tests or a recorded real run. Don't claim something works without evidence.
- **Small, reviewable changes.** One concern per pull request, with a clear description.

## Install and setup (for agents setting this up for a person)

> **Status: planned, not released.** Fonsviva has no release yet, so there is nothing to install today. These steps are the target behavior. Each one becomes true only when its test in [TESTING.md](https://github.com/baney75/Fonsviva/blob/main/TESTING.md) passes, and [SUPPORTED.md](https://github.com/baney75/Fonsviva/blob/main/SUPPORTED.md) is the source of truth for what works now. An agent must check SUPPORTED.md first and tell the person plainly if a step isn't supported yet.

Follow [docs/agents.md](docs/agents.md) ([web](https://baney75.github.io/Fonsviva/agents)). In short:

1. **Install:** [docs/setup/install.md](docs/setup/install.md). Check: `fonsviva status` prints `Fonsviva <version> running at http://127.0.0.1:7341`.
2. **Connect apps and agents:** [docs/setup/connect-fonsviva.md](docs/setup/connect-fonsviva.md). Check: `fonsviva keys list` shows each app as `active`.
3. **Select models:** [docs/setup/select-models.md](docs/setup/select-models.md). Check: `fonsviva models list` shows the chosen models with gates `pass`.
4. **Cloud (optional, personal only):** [docs/setup/add-cloud-provider.md](docs/setup/add-cloud-provider.md). Only in Fonsviva. Ask the person first.
5. **If anything fails:** [docs/setup/troubleshoot.md](docs/setup/troubleshoot.md) and `fonsviva doctor`.

Ask before installing anything or spending money, never print keys (share them only by one-time link, pairing code or keychain handoff, per `docs/KEYS.md`), only create keys narrower than your own, and never try to get around the cloud block. Machine-readable index: [llms.txt](llms.txt).

## Skills

Skills live in `skills/maintainer/` (for building this project) and `skills/runtime/` (used by agents inside the app). None exist yet. Each one must be original and tested before it merges; see `skills/README.md`. Load the skill that matches your task before starting, and improve it when you learn something.

## Testing and honesty

Never claim the software can do something without a passing test. Follow `TESTING.md`, keep `SUPPORTED.md` honest, and benchmark models per `BENCHMARKS.md`. Model-specific instructions live in `models/<model>/`.

## Releases

Fonsviva ships on three channels, stable, nightly and dev, under the Local Studio family scheme. See `RELEASES.md`. Never push directly to `stable`.

## Repository layout

- `AGENTS.md`: this file
- `CLAUDE.md`: Claude Code entry point, points here
- `DESIGN.md`: design system and UX rules, owned by the designer
- `RELEASES.md`: stable, nightly and dev channels
- `CONTRIBUTING.md`: how to contribute, including design work and coauthorship
- `TESTING.md`, `SUPPORTED.md`, `BENCHMARKS.md`: behavior tests, honest scope and model benchmarks
- `docs/DECISIONS.md`, `docs/API.md`: the shape and why, and the daemon API
- `docs/ARCHITECTURE.md`: what Fonsviva builds on and what it adds
- `docs/MODELS.md`: the model manager
- `docs/CONNECT.md`: connecting agents and apps
- `docs/CLOUD.md`: cloud providers, personal use only
- `docs/providers.md`: vetted provider list with sources
- `docs/COMPUTE.md`, `docs/COSTS.md`, `docs/JOBS.md`, `docs/STORAGE.md`, `docs/KEYS.md`: adding compute, costs and spend protection, job types, model storage, API keys
- `docs/setup/`, `docs/agents.md`, `llms.txt`: setup for agents and people; `docs/` is also the GitHub Pages site
- `skills/maintainer/`, `skills/runtime/`: original, tested skills
- `models/`: per-model instructions
- `tests/`, `benchmarks/results/`: tests and published results
- `design/`: design exports and notes
