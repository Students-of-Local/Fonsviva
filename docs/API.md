# Fonsviva API, version 1

The daemon (`fonsviva serve`) listens on `127.0.0.1:7341`. The desktop app, Local Studio, Local Studio Remote (for the iPhone app), the CLI and agents all use this API. Status: proposed, 2026-11-17. Changes go through an issue on baney75/Fonsviva until v1 ships; after that only additive changes are allowed on `/api/v1`.

## Discovery and ownership

- Data dir: `$XDG_DATA_HOME/fonsviva` (default `~/.local/share/fonsviva`) on Linux, `~/Library/Application Support/Fonsviva` on macOS, `%LOCALAPPDATA%\Fonsviva` on Windows. Dev builds use `fonsviva-dev` / `Fonsviva Dev` instead, so a dev build never shares state with stable or nightly. `FONSVIVA_HOME` overrides it (tests always set it).
- Port: 7341 for stable and nightly, 7342 for dev. If the port is taken by something that is not this data dir's daemon, the daemon takes a free port and records it. Clients never assume the port; they read it from `daemon.json`.
- One daemon owns a data dir. It holds `daemon.lock` and writes `daemon.json` (`{"pid","port","version","channel","schema","data_dir_id","started_at"}`, mode 0600). `data_dir_id` is a random id created with the data dir. A client reads `daemon.json`, checks that `GET /health` returns the same `data_dir_id`, and attaches; it starts its own `fonsviva serve` only when nothing answers. So a test daemon on a scratch `FONSVIVA_HOME` never attaches to the real one.
- `schema` is the data dir's migration version. A daemon refuses to open a data dir with a newer schema than it knows and says which version to update to; migrations keep the previous schema's files until the next successful start, so a nightly that migrated can be rolled back to stable.
- Models live in `<data dir>/models/`. Models found in Ollama, LM Studio and the Hugging Face cache are listed and served in place, never copied.

## Auth

- `GET /health` is open and returns no private data.
- The daemon binds 127.0.0.1 only and refuses any request whose `Host` header is not `127.0.0.1:<port>` or `localhost:<port>`, and any browser request with an `Origin` other than the desktop app's own. This blocks DNS rebinding and drive-by pages.
- Every other `/api/v1` route needs `Authorization: Bearer <token>`. Tokens are scoped:
  - `owner`: everything, including cloud. Only the desktop app and the CLI run by the owner get it, from `<data dir>/owner.token` (0600).
  - `control`: read, pull, run, stop, wiring. No cloud start, no keys. Local Studio reads `<data dir>/control.token` (0600). Local Studio Remote maps a paired phone to a narrower `watch` scope: read, run, stop, and stop a pod.
  - App keys (docs/KEYS.md): `/v1/*` only, each bound to one app and one profile.
- `/v1/*` always needs an app key. First run creates one for the desktop app; the Connect screen creates one per target it wires.

## Cloud gate

Every `/api/v1/cloud/*` route that can spend money refuses unless all of these hold, checked in the daemon before any network call, with these error codes:

| Check | Code |
| --- | --- |
| Token scope is `owner` | `scope` |
| Cloud is turned on in Settings (off by default) | `cloud_off` |
| The active profile is `personal` (docs/CLOUD.md); UntoCaesar client or volunteer work and Corunis are always refused | `cloud_blocked` |
| The start would stay inside the session, day and month caps in docs/COSTS.md | `budget_cap` |
| `confirm_cost_per_hour` equals the current estimate | `price_changed` |
| `idempotency_key` is new, or matches a start already made (which is returned, not repeated) | none: same pod returned |

A `/v1/*` request whose app key is bound to a non-personal profile is never routed to a cloud model, whatever model name it sends.

## Routes

| Route | Purpose |
| --- | --- |
| `GET /health` | `{"status":"ok","version","channel","api":1}` |
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
| `POST /api/v1/wire/{target}` | Applies it: backs up the user's file (only the first time; applying again keeps that original backup), writes, reads back, returns the diff |
| `DELETE /api/v1/wire/{target}` | Reverts to the backup, then reads back |
| `GET /api/v1/cloud/runpod/templates` | Templates Fonsviva supports, with GPU options and live hourly prices |
| `POST /api/v1/cloud/runpod/estimate` | Cost per hour and for the planned duration, before anything starts |
| `POST /api/v1/cloud/runpod/pods` | Starts a pod, behind the cloud gate above; body `{"template","gpu","confirm_cost_per_hour","idempotency_key"}` |
| `GET /api/v1/cloud/runpod/pods` | Pods, state, running cost so far, stopped-volume cost per hour, idle minutes, auto-stop deadline |
| `POST /api/v1/cloud/runpod/pods/{id}/stop` | Stops it. The volume still bills while stopped, and the response says how much per hour |
| `DELETE /api/v1/cloud/runpod/pods/{id}` | Terminates it, which ends all billing and deletes its volume; needs `{"confirm":true}` |
| `GET /api/v1/events` | Server-sent events: `job`, `run`, `stats`, `wire`, `cloud`. The app, Local Studio and Remote subscribe here instead of polling |
| `GET /v1/models`, `POST /v1/chat/completions`, `/v1/completions`, `/v1/embeddings` | OpenAI-compatible, routed by `model` to the engine that has it loaded, loading it on demand if it fits |

Errors are `{"error":{"code","message","fix"?}}` where `message` is plain words a person can act on.
