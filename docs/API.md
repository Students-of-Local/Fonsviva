# Fonsviva API, version 1

The daemon (`fonsviva serve`) listens on `127.0.0.1:7341`. The desktop app, Local Studio, Local Studio Remote (for the iPhone app), the CLI and agents all use this API. Status: proposed, 2026-11-17. Changes go through an issue on baney75/Fonsviva until v1 ships; after that only additive changes are allowed on `/api/v1`.

## Discovery and ownership

- Data dir: `$XDG_DATA_HOME/fonsviva` (default `~/.local/share/fonsviva`) on Linux, `~/Library/Application Support/Fonsviva` on macOS, `%LOCALAPPDATA%\Fonsviva` on Windows. Dev builds use `fonsviva-dev` / `Fonsviva Dev` instead, so a dev build never shares state with stable or nightly. `FONSVIVA_HOME` overrides it (tests always set it).
- Port: 7341 for stable and nightly, 7342 for dev, fixed, because wired apps (pi, T3 Code, OpenAI clients) store the URL. If the port is taken by something else, the daemon does not move; it refuses to start and names the process holding the port. `FONSVIVA_PORT` overrides it, and `FONSVIVA_PORT=0` picks a free port; tests always set one of these, so a test daemon never meets the real one.
- One daemon owns a data dir. It holds `daemon.lock` and writes `daemon.json` (`{"pid","port","version","channel","schema","data_dir_id","started_at"}`, mode 0600). `data_dir_id` is a random id created with the data dir. A client reads `daemon.json` and attaches only if `GET /health` returns the same `data_dir_id`. With no `daemon.json`, or no answer, it starts its own `fonsviva serve`. A daemon that answers with another `data_dir_id` is never used.
- Stable and nightly share only `models/` and `engines/` (content-addressed files that never migrate). Settings, keys, wiring journals and cloud state live in `state/<channel>/`, so a nightly schema migration can never lock stable out. `schema` in `daemon.json` is that channel's state version; a daemon refuses state newer than it knows and says which version to update to.
- Models live in `<data dir>/models/`. Models found in Ollama, LM Studio and the Hugging Face cache are listed and served in place, never copied.

## Auth

- `GET /health` is open and returns no private data.
- The daemon binds 127.0.0.1 only and refuses any request whose `Host` header is not `127.0.0.1:<port>` or `localhost:<port>`, which blocks DNS rebinding. Local Studio Remote runs on the same machine and calls the daemon on 127.0.0.1 itself, so phone traffic passes this check without loosening it.
- `/api/v1/*` refuses browser requests whose `Origin` is not the desktop app's own (`tauri://localhost`, `http://tauri.localhost`). `/v1/*` accepts any `Origin` with a valid app key, so browser-based OpenAI clients work; the key is the guard there.
- Every other `/api/v1` route needs `Authorization: Bearer <token>`. Tokens are scoped:
  - `owner`: everything, including cloud. Only the desktop app and the CLI run by the owner get it, from `<data dir>/owner.token` (0600).
  - `control`: read, pull, run, stop, wiring, and stopping a pod. No cloud start, no terminate, no keys. Local Studio reads `<data dir>/control.token` (0600). Wiring a target creates that target's app key inside the daemon, so `control` never needs the keys routes.
  - `watch`: read, run, stop, and stopping a pod. Minted only by `POST /api/v1/tokens {"scope":"watch","label"}` (`owner` or `control`), one per paired phone, revocable with `DELETE /api/v1/tokens/{id}`. Local Studio Remote mints one at pairing.
  - App keys (docs/KEYS.md): `/v1/*` only, each bound to one app and one profile. The profiles an app may hold are fixed in code, not chosen by the caller: UntoCaesar keys are always `untocaesar`, Corunis keys always `corunis`, Gantrel keys `gantrel`; only the desktop app, the CLI and wired agents (pi, T3 Code, OpenAI clients) may hold `personal`.
- `/v1/*` always needs an app key. First run creates one for the desktop app.

## Cloud gate

A pod start is a request, not a start. `POST /api/v1/cloud/runpod/pods` checks the gate below and returns `202 {"confirmation"}`; nothing is sent to RunPod yet. The desktop app then shows a native OS dialog (not a web view) with the GPU, the per-hour price, the planned hours and the caps, and only a click in that dialog makes the start real. No token, key, CLI flag or API call can confirm it, so an agent working as the owner's OS user still cannot spend money. On a machine without the desktop app, pods cannot be started in v1.

The gate, checked in the daemon before any network call, with these error codes:

| Check | Code |
| --- | --- |
| Token scope is `owner` | `scope` |
| Cloud is turned on in Settings (off by default) | `cloud_off` |
| The active profile is `personal` (docs/CLOUD.md); UntoCaesar client or volunteer work and Corunis are always refused | `cloud_blocked` |
| `planned_hours` x price stays inside the session, day and month caps in docs/COSTS.md | `budget_cap` |
| `confirm_cost_per_hour` equals the current estimate | `price_changed` |

`idempotency_key` is checked first: a key already used returns that request's pod or confirmation unchanged, with no gate re-run and nothing new started. The supervisor stops a pod at its cap whatever `planned_hours` said.

The same gate covers `/v1/*`: a request for a cloud model is refused with the same codes when cloud is off, the key's profile is not `personal`, or a cap is reached. Cloud models carry `"location":"cloud"` in `/v1/models`.

## Routes

| Route | Purpose |
| --- | --- |
| `GET /health` | `{"status":"ok","version","channel","api":1,"data_dir_id"}` |
| `GET /api/v1/hardware` | OS, arch, CPU, RAM, GPUs (vendor, name, VRAM, backend: metal, cuda, rocm, vulkan, cpu), unified memory flag, free memory now |
| `GET /api/v1/catalog` | Recommended models, each with `fit` (`fits`, `tight`, `no`), the reason in plain words, download size, memory needed at the default context, and any measured speed on similar hardware |
| `GET /api/v1/models` | Installed models across every store: id, name, source (`fonsviva`, `ollama`, `lmstudio`, `hf`), path, bytes, quant, params, context max, sha256 when known, duplicate group |
| `POST /api/v1/models/pull` | `{"id"}` starts a resumable, checksum-verified download; returns `{"job"}` |
| `DELETE /api/v1/models/{id}` | Removes a Fonsviva-downloaded model. Models owned by Ollama or LM Studio are removed through that tool (Ollama's `DELETE /api/delete`), never by deleting a shared blob, and only with `{"confirm":true}` |
| `POST /api/v1/jobs/{id}/cancel` | Cancels a download or benchmark; a partial download is kept for resume unless `{"discard":true}` |
| `GET /api/v1/keys`, `POST /api/v1/keys`, `DELETE /api/v1/keys/{id}` | App keys: list (never the secret), create (`{"app","profile"}`, secret returned once), revoke. `owner` scope only |
| `POST /api/v1/run` | `{"model","engine"?,"context"?,"gpu_layers"?}` loads a model; the effective settings come back, never a silent default |
| `GET /api/v1/running` | Loaded models: engine, port, context, GPU layers, pid, resident memory, GPU memory |
| `POST /api/v1/stop` | `{"model"}` unloads it |
| `GET /api/v1/stats` | Per loaded model, last and rolling: time to first token (ms), prompt tokens/s, generation tokens/s, memory |
| `POST /api/v1/bench` | `{"model"}` runs the fixed benchmark prompt set and stores the result for this machine |
| `GET /api/v1/wire` | Each target (`pi`, `t3code`, `eukhe`, `openai`) with state (`absent`, `applied`, `drifted`), the file it writes and the backup it would restore |
| `POST /api/v1/wire/{target}` | Applies it: when the target is `absent`, backs up the user's file as it is now; when it is already `applied`, keeps that backup. Then writes, reads back, returns the diff |
| `DELETE /api/v1/wire/{target}` | Reverts to the backup, then reads back |
| `GET /api/v1/cloud/runpod/templates` | Templates Fonsviva supports, with GPU options and live hourly prices |
| `POST /api/v1/cloud/runpod/estimate` | Cost per hour and for the planned duration, before anything starts |
| `POST /api/v1/cloud/runpod/pods` | Requests a pod, behind the cloud gate above; body `{"template","gpu","planned_hours","confirm_cost_per_hour","idempotency_key"}`; returns `202 {"confirmation"}` for the desktop app's native dialog. `owner` only |
| `GET /api/v1/cloud/runpod/pods` | Pods, state, running cost so far, stopped-volume cost per hour, idle minutes, auto-stop deadline |
| `POST /api/v1/cloud/runpod/pods/{id}/stop` | Stops it. The volume still bills while stopped, and the response says how much per hour |
| `DELETE /api/v1/cloud/runpod/pods/{id}` | Terminates it, which ends all billing and deletes its volume; needs `{"confirm":true}`. `owner` only |
| `GET /api/v1/events` | Server-sent events: `job`, `run`, `stats`, `wire`, `cloud`. The app, Local Studio and Remote subscribe here instead of polling |
| `GET /v1/models`, `POST /v1/chat/completions`, `/v1/completions`, `/v1/embeddings` | OpenAI-compatible, routed by `model` to the engine that has it loaded, loading it on demand if it fits |

Errors are `{"error":{"code","message","fix"?}}` where `message` is plain words a person can act on.
