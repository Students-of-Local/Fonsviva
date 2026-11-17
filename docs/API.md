# Fonsviva API, version 1

The daemon (`fonsviva serve`) listens on `127.0.0.1:7341`. The desktop app, Local Studio, Local Studio Remote (for the iPhone app), the CLI and agents all use this API. Status: proposed, 2026-11-17. Changes go through an issue on baney75/Fonsviva until v1 ships; after that only additive changes are allowed on `/api/v1`.

## Discovery and ownership

- Data dir: `$XDG_DATA_HOME/fonsviva` (default `~/.local/share/fonsviva`) on Linux, `~/Library/Application Support/Fonsviva` on macOS, `%LOCALAPPDATA%\Fonsviva` on Windows. `FONSVIVA_HOME` overrides it (tests always set it).
- One daemon owns a data dir. It holds `daemon.lock` and writes `daemon.json` (`{"pid","port","version","channel","started_at"}`, mode 0600). A client reads `daemon.json`, checks `GET /health`, and attaches; it starts its own `fonsviva serve` only when nothing answers.
- Models live in `<data dir>/models/`. Models found in Ollama, LM Studio and the Hugging Face cache are listed and served in place, never copied.

## Auth

- `GET /health` is open.
- Every other `/api/v1` route needs `Authorization: Bearer <control token>`. The token is in `<data dir>/control.token` (0600), created on first start. Local Studio and the desktop app read it from disk; a remote client gets it only through Local Studio Remote pairing.
- `/v1/*` (the OpenAI-compatible endpoint) takes per-app keys from docs/KEYS.md. Local requests without a key are allowed only while no app key exists (first-run convenience) and the UI says so.

## Routes

| Route | Purpose |
| --- | --- |
| `GET /health` | `{"status":"ok","version","channel","api":1}` |
| `GET /api/v1/hardware` | OS, arch, CPU, RAM, GPUs (vendor, name, VRAM, backend: metal, cuda, rocm, vulkan, cpu), unified memory flag, free memory now |
| `GET /api/v1/catalog` | Recommended models, each with `fit` (`fits`, `tight`, `no`), the reason in plain words, download size, memory needed at the default context, and any measured speed on similar hardware |
| `GET /api/v1/models` | Installed models across every store: id, name, source (`fonsviva`, `ollama`, `lmstudio`, `hf`), path, bytes, quant, params, context max, sha256 when known, duplicate group |
| `POST /api/v1/models/pull` | `{"id"}` starts a resumable, checksum-verified download; returns `{"job"}` |
| `DELETE /api/v1/models/{id}` | Removes a Fonsviva-downloaded model; refuses files it did not download unless `{"confirm":true}` |
| `POST /api/v1/run` | `{"model","engine"?,"context"?,"gpu_layers"?}` loads a model; the effective settings come back, never a silent default |
| `GET /api/v1/running` | Loaded models: engine, port, context, GPU layers, pid, resident memory, GPU memory |
| `POST /api/v1/stop` | `{"model"}` unloads it |
| `GET /api/v1/stats` | Per loaded model, last and rolling: time to first token (ms), prompt tokens/s, generation tokens/s, memory |
| `POST /api/v1/bench` | `{"model"}` runs the fixed benchmark prompt set and stores the result for this machine |
| `GET /api/v1/wire` | Each target (`pi`, `t3code`, `eukhe`, `openai`) with state (`absent`, `applied`, `drifted`), the file it writes and the backup it would restore |
| `POST /api/v1/wire/{target}` | Applies it: backs up, writes, reads back, returns the diff |
| `DELETE /api/v1/wire/{target}` | Reverts to the backup, then reads back |
| `GET /api/v1/cloud/runpod/templates` | Templates Fonsviva supports, with GPU options and live hourly prices |
| `POST /api/v1/cloud/runpod/estimate` | Cost per hour and for the planned duration, before anything starts |
| `POST /api/v1/cloud/runpod/pods` | Starts a pod; needs `{"confirm_cost_per_hour": <number>}` matching the current estimate |
| `GET /api/v1/cloud/runpod/pods` | Pods, state, running cost so far, idle minutes, auto-stop deadline |
| `POST /api/v1/cloud/runpod/pods/{id}/stop` | Stops it |
| `GET /api/v1/events` | Server-sent events: `job`, `run`, `stats`, `wire`, `cloud`. The app, Local Studio and Remote subscribe here instead of polling |
| `GET /v1/models`, `POST /v1/chat/completions`, `/v1/completions`, `/v1/embeddings` | OpenAI-compatible, routed by `model` to the engine that has it loaded, loading it on demand if it fits |

Errors are `{"error":{"code","message","fix"?}}` where `message` is plain words a person can act on.
