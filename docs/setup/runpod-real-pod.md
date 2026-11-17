# Run a real RunPod pod (personal use only)

> **Status: planned, not released.** Fonsviva has no release yet, so there is nothing to install today. These steps are the target behavior. Each one becomes true only when its test in [TESTING.md](https://github.com/baney75/Fonsviva/blob/main/TESTING.md) passes, and [SUPPORTED.md](https://github.com/baney75/Fonsviva/blob/main/SUPPORTED.md) is the source of truth for what works now. An agent must check SUPPORTED.md first and tell the person plainly if a step isn't supported yet.

A real pod costs real money from the moment it starts until you terminate it. Fonsviva shows the price before anything starts, asks you in a native dialog, and stops the pod on its own when it sits idle or reaches a cap. The rules are in [CLOUD](../CLOUD.md) and [COSTS](../COSTS.md); the routes are in [API](../API.md#cloud-gate).

## Before you start

- Cloud only runs on the **personal** profile. UntoCaesar and Corunis work is refused before Fonsviva sends anything to RunPod, and Local Studio plugin keys never reach a cloud model.
- Set a session, day and month cap first. Cloud can't be turned on until all three are set.
- Use your own RunPod account. Fonsviva never resells GPU time and never sees your bill.

## 1. Put your key in the keychain

1. Create an API key at <https://console.runpod.io/user/settings>. Give it the least access RunPod allows for pods.
2. Run `fonsviva cloud add runpod --profile personal` and paste the key once.

Fonsviva stores it in the OS keychain under service `fonsviva`, account `runpod`: Keychain Services on macOS, Credential Manager on Windows, the Secret Service on Linux. It is never written to a file, a log or a response, and every debug print shows `<redacted>`. To remove it, remove RunPod in Settings > Cloud, then revoke the key on RunPod's site.

## 2. Turn cloud on and set caps

1. In Settings > Cloud, set the session, day and month caps in dollars.
2. Run `fonsviva cloud enable runpod --profile personal`, or turn it on in Settings > Cloud.

Idle auto-stop defaults to 10 minutes with no request. Change it in Settings > Cloud.

## 3. Pick a template and a GPU

`GET /api/v1/cloud/runpod/templates` lists the two templates Fonsviva supports, each with the GPUs that have enough memory and their live per-hour prices, cheapest first.

| Template | Server | Default model | Minimum GPU memory | Container disk | Volume |
| --- | --- | --- | --- | --- | --- |
| `llama-cpp` | llama.cpp `llama-server`, image pinned by digest | `ggml-org/gpt-oss-20b-GGUF` | 24 GB | 20 GB | 50 GB at `/workspace` |
| `vllm` | `vllm/vllm-openai:v0.33.0` | `Qwen/Qwen3-8B` | 48 GB | 30 GB | 80 GB at `/workspace` |

Both run on RunPod's Secure Cloud, serve an OpenAI-compatible API, and keep downloaded weights on the volume so a restart doesn't download them again. Each pod gets its own random API key, which the daemon keeps in memory and never shows.

## 4. Read the estimate

`POST /api/v1/cloud/runpod/estimate` with `{"template","gpu","planned_hours"}` returns the full per-hour cost: the GPU, the container disk, and the volume. RunPod bills by the second.

Examples at RunPod's Secure Cloud prices on 2026-11-17 (check the live estimate; prices change):

| Template and GPU | GPU per hour | Disk and volume per hour | Total per hour | 3 hours | Stopped volume |
| --- | --- | --- | --- | --- | --- |
| `llama-cpp` on RTX 4090 | $0.89 | $0.0097 | $0.8997 | $2.70 | $0.0139 an hour, $10.00 a month |
| `vllm` on L40S | $1.09 | $0.0153 | $1.1053 | $3.32 | $0.0222 an hour, $16.00 a month |

Storage rates: container disk $0.10 per GB per month while running; volume $0.10 per GB per month while running and $0.20 while stopped.

## 5. Start it

`POST /api/v1/cloud/runpod/pods` with `{"template","gpu","planned_hours","confirm_cost_per_hour","idempotency_key"}`. The template fixes the model; any other field is refused. `confirm_cost_per_hour` must equal the estimate's `total_per_hour`, or the start is refused with `price_changed`. The planned hours times the price, plus anything already running, must fit under every cap, or it is refused with `budget_cap`.

The route returns `202 {"confirmation"}` and sends nothing to RunPod. Only one confirmation is open at a time; another request while one is open is refused with `pending`. The daemon then opens a native dialog with who asked, the GPU, the price, the planned hours, the stopped-volume cost and your caps, and asks for Touch ID, Windows Hello or your password. The dialog expires after 5 minutes. When you approve, the daemon checks everything again with a fresh price, then creates the pod.

The first start downloads the model to the volume. Expect several minutes before the server answers; RunPod's `RUNNING` status means the container is up, not that the model is loaded.

## 6. Watch the cost

- `GET /api/v1/cloud/runpod/pods` shows each pod's state, cost so far, idle minutes and auto-stop time.
- `GET /api/v1/events` streams `cloud` events: started, stopped (with the reason), budget alerts at 50, 80 and 100 percent of each cap, and the volume cap notice.

Fonsviva stops a running pod on its own when:

| Reason | When |
| --- | --- |
| `idle` | No request for the idle time (10 minutes by default) |
| `budget_cap` | The session, day or month cap is reached |
| `dead_man` | Fonsviva can't read the pod's status from RunPod 3 times over 2 minutes |
| `clock_jump` | The computer's clock jumped or it slept, so Fonsviva can't trust its own timing |
| `orphan` | A `fonsviva-` pod on your account that Fonsviva has no record of |

`fonsviva cloud stop-all` stops every pod and turns cloud off.

## 7. Stop or terminate

- **Stop** (`POST /api/v1/cloud/runpod/pods/{id}/stop`) ends GPU billing but keeps the volume, which still bills. The response says how much per hour and per month.
- **Terminate** (`DELETE /api/v1/cloud/runpod/pods/{id}`, owner only) ends all billing and deletes the volume and the downloaded model. The daemon asks in the same native dialog and presence check as a start, and only that click deletes anything.

Stopped volumes count against your day and month caps. When a cap is reached by volumes alone, Fonsviva tells you and offers to terminate; it never deletes a volume on its own.

## Check

- The estimate's total per hour is the GPU price plus disk and volume, and matches the dialog.
- A start from a `control` or `watch` token, with cloud off, or under another profile is refused before any call to RunPod.
- After the idle time with no requests, the pod shows `stopped` with reason `idle`, and RunPod's console shows it exited.
- After terminate, RunPod's console no longer lists the pod.
