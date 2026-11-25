# Fonsviva API, version 1

The daemon (`fonsviva serve`) listens on `127.0.0.1:7341`. The desktop app, Local Studio, Local Studio Remote (for the iPhone app), the CLI and agents all use this API. Status: proposed, 2026-11-17. Changes go through an issue on Students-of-Local/Fonsviva until v1 ships; after that only additive changes are allowed on `/api/v1`.

## Discovery and ownership

- Data dir: `$XDG_DATA_HOME/fonsviva` (default `~/.local/share/fonsviva`) on Linux, `~/Library/Application Support/Fonsviva` on macOS, `%LOCALAPPDATA%\Fonsviva` on Windows. Dev builds use `fonsviva-dev` / `Fonsviva Dev` instead, so a dev build never shares state with stable or nightly. `FONSVIVA_HOME` overrides it (tests always set it).
- Port: 7341 for stable and nightly, 7342 for dev, fixed, because wired apps (pi, T3 Code, OpenAI clients) store the URL. If the port is taken by something else, the daemon does not move; it refuses to start and names the process holding the port. `FONSVIVA_PORT` overrides it, and `FONSVIVA_PORT=0` picks a free port; tests always set one of these, so a test daemon never meets the real one.
- One daemon owns a data dir. It holds `daemon.lock` and writes `daemon.json` (`{"pid","port","version","channel","schema","data_dir_id","secret","started_at"}`, mode 0600). `data_dir_id` is a random id created with the data dir; it is public and `/health` returns it. `secret` is a random value made at each start and lives only in `daemon.json`. A client reads `daemon.json`, sends `GET /health?challenge=<random nonce>`, and attaches only if the answer has the same `data_dir_id` and `proof` equals HMAC-SHA256(`secret`, nonce), so a process that took the port and copied the public id still gets no token. It also checks that `channel` matches its own and that it supports the daemon's `api` and `schema`; on a mismatch it tells the person which channel holds this data dir and stops, instead of attaching. With no `daemon.json`, or no answer, it starts its own `fonsviva serve`.
- Stable and nightly share `models/` and `engines/` (content-addressed files that never migrate) and `shared/`, which holds app keys and wiring journals in a format that only ever gains fields, so a wired client keeps working when the person moves between stable and nightly. Settings and cloud state live in `state/<channel>/`, so a nightly schema migration can never lock stable out. `schema` in `daemon.json` is that channel's state version; a daemon refuses state newer than it knows and says which version to update to.
- When Local Studio has to start the daemon, it runs the newest `fonsviva` of its own channel installed on the machine, its own bundled copy or the standalone app's, so it never starts an older engine against state a newer one wrote. It never starts another channel's binary; if another channel's daemon already holds the data dir, it says which one and stops, as above.
- Models live in `<data dir>/models/`. Models found in Ollama, LM Studio and the Hugging Face cache are listed and served in place, never copied.

## Auth

- `GET /health` is open and returns no private data.
- The daemon binds 127.0.0.1 only and refuses any request whose `Host` header is not `127.0.0.1:<port>` or `localhost:<port>`, which blocks DNS rebinding. Local Studio Remote runs on the same machine and calls the daemon on 127.0.0.1 itself, so phone traffic passes this check without loosening it.
- `/api/v1/*` refuses browser requests whose `Origin` is not the desktop app's own (`tauri://localhost`, `http://tauri.localhost`). A dev-channel daemon also accepts `http://localhost:1430`, the app's dev server; stable and nightly never do. `/v1/*` accepts any `Origin` with a valid app key, so browser-based OpenAI clients work; the key is the guard there.
- Every other `/api/v1` route needs `Authorization: Bearer <token>`. Tokens are scoped:
  - `owner`: everything, including cloud. Only the desktop app and the CLI run by the owner get it, from `<data dir>/owner.token` (0600).
  - `control`: read, pull, run, stop, wiring, and stopping a pod. No cloud start, no terminate, and no keys except plugin keys (below). Local Studio reads `<data dir>/control.token` (0600). Wiring a target creates that target's app key inside the daemon, so `control` never needs the keys routes. No key can reach cloud models until the owner turns cloud on for that key, and that switch goes through the same daemon-raised dialog as a pod start, naming the app the key was made for. The app name on a key is what its creator said it is, so the block on UntoCaesar and Corunis holds against honest apps and agents, and the dialog is the guard against a key passed to the wrong app.
  - `watch`: read, run, stop, and stopping a pod. Minted only by `POST /api/v1/tokens {"scope":"watch","label"}` (`owner` or `control`), one per paired phone, revocable with `DELETE /api/v1/tokens/{id}`. Local Studio Remote mints one at pairing.
  - App keys (docs/KEYS.md): `/v1/*` only, each bound to one app and one profile. The create body names only the `app`; the profile follows from it in code, never from the caller: UntoCaesar keys are always `untocaesar`, Corunis keys always `corunis`, Gantrel keys `gantrel`; only the desktop app, the CLI and wired agents (pi, T3 Code, OpenAI clients) may hold `personal`.
  - Plugin keys: Local Studio creates one app key per plugin the person granted `local-models` (baney75/local-studio#18), with `control`. The key's app is `local-studio-plugin:<id>`; its profile is `plugin`, or `untocaesar`, `corunis` or `gantrel` for those ids. A plugin key never reaches a cloud model: it has no cloud switch, `/v1/models` lists only local models for it, and a request naming a cloud model gets `cloud_blocked`. Creating a key for an id that has one revokes the old one, so Local Studio can recover a lost secret.
- `/v1/*` always needs an app key. First run creates one for the desktop app and writes it to `<data dir>/desktop.key` (0600), next to `owner.token`; the app reads both when it attaches. The daemon writes both files before `/health` answers.

## Cloud gate

A pod start is a request, not a start. `POST /api/v1/cloud/runpod/pods` checks the gate below and returns `202 {"confirmation"}`; nothing is sent to RunPod yet. The daemon process itself then raises a native OS dialog (not a web view) showing who asked (token scope and label, and the calling program when the OS reports it), the GPU, the per-hour price, the planned hours, the stopped-volume cost and the caps, and asks for the OS's own user-presence check where the OS has one (Touch ID or the login password through LocalAuthentication on macOS, Windows Hello on Windows, polkit on Linux). The click and the check are handled inside the daemon; there is no route, token, key or CLI flag that confirms a start.

What this guarantees: an agent that holds `owner.token` or runs the CLI cannot start a pod by itself. What it does not: a program that can drive the person's screen, or that replaces the `fonsviva` binary, acts as the person; the OS user-presence check is the only defense there, so the dialog never offers to skip it. On a machine with no desktop session, pods cannot be started in v1.

Only one confirmation is open at a time; another request while one is open gets `pending`. A confirmation expires after 5 minutes. At the click the daemon runs the whole gate again (cloud on, profile, caps, price) before any call to RunPod, and refuses with that gate's code if anything changed.

The gate, checked in the daemon before any network call, with these error codes:

| Check | Code |
| --- | --- |
| Token scope is `owner` | `scope` |
| Cloud is turned on in Settings (off by default) | `cloud_off` |
| The active profile is `personal` (docs/CLOUD.md); UntoCaesar client or volunteer work and Corunis are always refused | `cloud_blocked` |
| `planned_hours` x price stays inside the session, day and month caps in docs/COSTS.md | `budget_cap` |
| `confirm_cost_per_hour` equals the current estimate | `price_changed` |

The token scope is checked first, then `idempotency_key`. A key is bound to the token that first used it: the same token sending it again gets that request's pod, or its unexpired confirmation, unchanged, and nothing new starts. Only a pod that already started skips the gate on replay; an expired confirmation is gone, and a new request runs the gate in full.

The supervisor stops a pod at its cap whatever `planned_hours` said. Stopped-volume cost counts against the day and month caps. When volume cost alone reaches a cap, Fonsviva notifies the person and offers to terminate; it never deletes a volume on its own.

`/v1/*` never starts, loads or resumes cloud capacity. A cloud model is routable only while a pod started through the dialog runs, and a request for one is still refused with the gate's codes when cloud is off, the key's profile is not `personal`, the key has no cloud permission, or a cap is reached. `/v1` traffic to a pod counts as activity for idle auto-stop; the caps stop the pod regardless. Cloud models carry `"location":"cloud"` in `/v1/models` and every other model carries `"location":"local"`, so a client such as Local Studio can tell them apart per model, not per provider.

## Routes

| Route | Purpose |
| --- | --- |
| `GET /health` | `{"status":"ok","version","channel","api":1,"schema","data_dir_id"}`, plus `proof` when called with `?challenge=` |
| `GET /api/v1/hardware` | OS, arch, CPU, RAM, GPUs (vendor, name, VRAM, backend: metal, cuda, rocm, vulkan, cpu), unified memory flag, free memory now |
| `GET /api/v1/catalog` | Recommended models, each with `fit` (`fits`, `tight`, `no`), the reason in plain words, download size, memory needed at the default context, and any measured speed on similar hardware |
| `GET /api/v1/models` | Installed models across every store: id, name, source (`fonsviva`, `ollama`, `lmstudio`, `hf`), path, bytes, quant, params, context max, sha256 when known, duplicate group |
| `POST /api/v1/models/pull` | `{"id"}` starts a resumable, checksum-verified download; returns `{"job"}` |
| `DELETE /api/v1/models/{id}` | Removes a Fonsviva-downloaded model. Models owned by Ollama or LM Studio are removed through that tool (Ollama's `DELETE /api/delete`), never by deleting a shared blob, and only with `{"confirm":true}` |
| `POST /api/v1/jobs/{id}/cancel` | Cancels a download or benchmark; a partial download is kept for resume unless `{"discard":true}` |
| `GET /api/v1/keys`, `POST /api/v1/keys`, `DELETE /api/v1/keys/{id}` | App keys: list (never the secret), create (`{"app"}`; the profile follows from the app, secret returned once), revoke. `owner` scope only |
| `POST /api/v1/plugin-keys`, `DELETE /api/v1/plugin-keys/{id}` | Create a plugin key (`{"plugin":"<id>"}`, `owner` or `control`; returns `{"id","secret"}`, secret once) and revoke one. Rules under Auth |
| `POST /api/v1/tokens`, `DELETE /api/v1/tokens/{id}` | Mint a `watch` token (`{"scope":"watch","label"}`, `owner` or `control`, secret returned once) and revoke one |
| `POST /api/v1/run` | `{"model","engine"?,"context"?,"gpu_layers"?}` loads a model; the effective settings come back, never a silent default |
| `GET /api/v1/running` | Loaded models: engine, port, context, GPU layers, pid, resident memory, GPU memory |
| `POST /api/v1/stop` | `{"model"}` unloads it |
| `GET /api/v1/stats` | Per loaded model, last and rolling: time to first token (ms), prompt tokens/s, generation tokens/s, memory |
| `POST /api/v1/bench` | `{"model"}` runs the fixed benchmark prompt set and stores the result for this machine |
| `GET /api/v1/wire` | Each target (`pi`, `t3code`, `eukhe`, `openai`) with state (`absent`, `applied`, `drifted`), the file it writes and the backup it would restore |
| `POST /api/v1/wire/{target}` | Applies it: when the target is `absent`, backs up the user's file as it is now; when it is already `applied`, keeps that backup. Then writes, reads back, returns the diff |
| `DELETE /api/v1/wire/{target}` | Reverts to the backup, then reads back |
| `GET /api/v1/cloud/runpod/templates` | Templates Fonsviva supports, with GPU options and live hourly prices: `[{"id","name","gpus":[{"gpu","per_hour"}]}]` |
| `POST /api/v1/cloud/runpod/estimate` | Cost per hour and for the planned duration, before anything starts |
| `POST /api/v1/cloud/runpod/pods` | Requests a pod, behind the cloud gate above; body `{"template","gpu","planned_hours","confirm_cost_per_hour","idempotency_key"}`; returns `202 {"confirmation"}`, and the daemon itself raises the native confirmation dialog. No route accepts a confirmation. `owner` only |
| `GET /api/v1/cloud/runpod/pods` | Pods, state, running cost so far, stopped-volume cost per hour, idle minutes, auto-stop deadline |
| `POST /api/v1/cloud/runpod/pods/{id}/stop` | Stops it. The volume still bills while stopped, and the response says how much per hour |
| `DELETE /api/v1/cloud/runpod/pods/{id}` | Asks to terminate it, which ends all billing and deletes its volume. `owner` only; the daemon raises the same native dialog and presence check as a start, and only that click deletes anything |
| `GET /api/v1/events` | Server-sent events: `job`, `run`, `stats`, `wire`, `cloud`. The app, Local Studio and Remote subscribe here instead of polling |
| `GET /v1/models`, `POST /v1/chat/completions`, `/v1/completions`, `/v1/embeddings` | OpenAI-compatible, routed by `model` to the engine that has it loaded, loading it on demand if it fits |

When a target is `drifted` (the person or another tool changed the file after Fonsviva wrote it), both apply and revert refuse with `drifted` and return the diff. Sent again with `{"confirm":true}`, they first save the file's current contents as a second backup, then proceed.

Errors are `{"error":{"code","message","fix"?}}` where `message` is plain words a person can act on.
