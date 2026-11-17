# Rivals: clean machine to first local answer (Linux, headless)

Measured 2026-11-17 on abbey1 (WSL2 Ubuntu host, AMD Ryzen 5 3600, 12 threads). Each run starts from a fresh `ubuntu:24.04` container with only curl and ca-certificates added (that prerequisite step is timed but not counted). Containers are limited to 8 CPUs and 12 GB, CPU only. Two runs per rival, one at a time, each started only while abbey1's 1-minute load was 10 or less; numbers are the median (the mean of the two). Four earlier runs were replaced; see Replaced runs. abbey1 is a shared host, so its load is listed beside every row. All runs are CPU only: untested on NVIDIA.

Network baseline: 104,857,600 bytes from `https://cachefly.cachefly.net/100mb.test` in 1.20 s (median of 3), 87.4 MB/s, about 699 Mbit/s (abbey1 load 4.92 11.37 9.75 at start).

Normalized model: `gemma3:1b` from the Ollama library and `ggml-org/gemma-3-1b-it-GGUF` (`gemma-3-1b-it-Q4_K_M.gguf`, 806,058,240 bytes) from Hugging Face; the Ollama blob is the same size within 160 bytes. Prompt: "Name three primary colors.", max 64 tokens, temperature 0, streamed.

TTFT is measured from the request send to the first streamed token, on a warm model after load. Total is wall clock from the first install command to that first token (installer, engine start, model download, load and first answer, including the time lost to failed steps).

| Rival | Actions | Install s | Download bytes | Model | Default context | TTFT ms | Gen tok/s | Total s to first token | abbey1 load (1 min, start/end) | Needs sudo on a real machine | Notes |
|---|---|---|---|---|---|---|---|---|---|---|---|
| Ollama (install.sh), normalized model | 6 | 23.8 | 2,368 MB | gemma3:1b | 4,096 | 268 | 25.2 | 41.3 | 8.94/8.22, 8.22/10.87 | yes: install.sh writes /usr/local (asks for the password); zstd via apt | failed first: install |
| Ollama (install.sh), docs quickstart model | 6 | 20.3 | 6,201 MB | gemma4:e2b | 4,096 | 269 | 22.3 | 87.0 | 3.81/3.01, 2.85/3.05 | yes: install.sh writes /usr/local (asks for the password); zstd via apt | failed first: install; thinking model: no visible answer within 64 tokens; first visible answer after 6,918 ms uncapped |
| llama.cpp, GitHub release tarball + llama-server | 7 | 5.5 | 834 MB | ggml-org/gemma-3-1b-it-GGUF (gemma-3-1b-it-Q4_K_M.gguf, the -hf default quant) | 32,768 | 147 | 38.9 | 19.8 | 0.6/0.76, 0.76/0.85 | only if libgomp1 is missing (apt) |  |
| llama.cpp, llama.app installer + llama serve (README today) | 3 | 1.6 | 820 MB | ggml-org/gemma-3-1b-it-GGUF (gemma-3-1b-it-Q4_K_M.gguf, the -hf default quant) | 32,768 | 166 | 38.9 | 13.7 | 0.68/1.11, 1.34/1.93 | no |  |
| LM Studio headless (llmster + lms) | 12 | 79.2 | 2,083 MB | ggml-org/gemma-3-1b-it-GGUF (gemma-3-1b-it-Q4_K_M.gguf) | 8,192 | 283 | 23.3 | 203.5 | 3.84/6.23, 5.65/11.49 | only if libatomic1 or libgomp1 is missing (apt) | failed first: install, load |
| Open WebUI :ollama image (Docker) | 8 | 120.3 | 5,102 MB | gemma3:1b | 4,096 | 486 | 22.5 | 203.5 | 9.82/6.94, 7.58/9.17 | Docker install and docker group |  |
| GPT4All Python SDK | 6 | 24.0 | 911 MB | Llama-3.2-1B-Instruct-Q4_0.gguf | 2,048 | 862 | 16.7 | 39.6 | 8.91/8.97, 5.06/5.81 | yes: python3-venv via apt |  |
| Jan | 4 | 7.4 | 718 MB | n/a | n/a | n/a | n/a | n/a | 9.39/7.69, 5.81/6.26 | AppImage no (libfuse2 via apt if missing); .deb and Flatpak setup yes | GUI only, not measured headless (installer download timed, not run) |
| GPT4All desktop | 10 | 2.1 | 39 MB | n/a | n/a | n/a | n/a | n/a | 7.69/7.95, 6.26/5.83 | no (installs to home) | GUI only, not measured headless (installer download timed, not run) |
| Msty Studio | 7 | 15.6 | 472 MB | n/a | n/a | n/a | n/a | n/a | 7.95/7.71, 5.83/5.71 | AppImage no (libfuse2 via apt if missing); .deb yes | GUI only, not measured headless (installer download timed, not run) |
| Pinokio | 9 | 1.9 | 168 MB | n/a | n/a | n/a | n/a | n/a | 7.71/6.67, 5.65/6.24 | AppImage no (libfuse2 via apt if missing); .deb yes | GUI only, not measured headless (installer download timed, not run) |

## Phase breakdown (median)

| Rival | Install s | Installer bytes | Engine start s | Model download s | Model bytes | Load s | First answer tokens | Runs |
|---|---|---|---|---|---|---|---|---|
| Ollama (install.sh), normalized model | 23.8 | 1,555 MB | 0.4 | 10.6 | 813 MB | 2.6 | 48 | 38.0 s, 44.6 s |
| Ollama (install.sh), docs quickstart model | 20.3 | 1,555 MB | 0.1 | 54.8 | 4,646 MB | 9.4 | 64 | 84.5 s, 89.4 s |
| llama.cpp, GitHub release tarball + llama-server | 5.5 | 24 MB | 0.0 | 10.7 | 811 MB | 1.1 | 47 | 17.7 s, 21.9 s |
| llama.cpp, llama.app installer + llama serve (README today) | 1.6 | 9 MB | 0.0 | 8.6 | 810 MB | 0.9 | 47 | 13.2 s, 14.3 s |
| LM Studio headless (llmster + lms) | 79.2 | 1,267 MB | 7.1 | 106.4 | 816 MB | 6.2 | 45 | 260.8 s, 146.2 s |
| Open WebUI :ollama image (Docker) | 120.3 | 3,487 MB | 63.3 | 12.3 | 1,615 MB | 3.5 | 48 | 206.1 s, 200.9 s |
| GPT4All Python SDK | 24.0 | 134 MB | 0.0 | 12.2 | 777 MB | 1.2 | 16 | 40.9 s, 38.4 s |
| Jan | 7.4 | 718 MB | n/a | n/a | n/a | n/a | n/a | 7.8 s download, 7.0 s download |
| GPT4All desktop | 2.1 | 39 MB | n/a | n/a | n/a | n/a | n/a | 3.6 s download, 0.7 s download |
| Msty Studio | 15.6 | 472 MB | n/a | n/a | n/a | n/a | n/a | 23.4 s download, 7.8 s download |
| Pinokio | 1.9 | 168 MB | n/a | n/a | n/a | n/a | n/a | 2.0 s download, 1.9 s download |

## Actions and what went wrong

### Ollama (install.sh), normalized model

Version: `Warning: client version is 0.40.16`

Actions (run 1):

1. `curl -fsSL https://ollama.com/install.sh | sh`
2. `apt-get install zstd (named only in the installer's error)`
3. `curl -fsSL https://ollama.com/install.sh | sh (again)`
4. `ollama serve (installer did not start a server: no systemd)`
5. `ollama run gemma3:1b`
6. type the prompt

Notes:

- install.sh failed on a clean ubuntu:24.04: 'This version requires zstd for extraction'. The docs do not list zstd; only the error names it. Full Ubuntu desktop and server images usually ship zstd, minimal containers do not.
- Run as root in the container; on a desktop install.sh asks for the sudo password once (one more prompt).
- The container shares abbey1's WSL2 kernel, so install.sh takes its WSL2 branch and exits before any service setup.
- install.sh printed 'The Ollama API is now available at 127.0.0.1:11434' (an exit trap) but no server was running; 'ollama serve' was needed (docs: start it 'if it is not already running').

First answer (run 1): "Here are three primary colors:\n\n1.  **Red**\n2.  **Yellow**\n3.  **Blue**\n\nThese colors are considered \"primary\" because they cannot be created by mixing other colors together.\n\n"

### Ollama (install.sh), docs quickstart model

Version: `Warning: client version is 0.40.16`

Actions (run 1):

1. `curl -fsSL https://ollama.com/install.sh | sh`
2. `apt-get install zstd (named only in the installer's error)`
3. `curl -fsSL https://ollama.com/install.sh | sh (again)`
4. `ollama serve (installer did not start a server: no systemd)`
5. `ollama run gemma4:e2b`
6. type the prompt

Notes:

- install.sh failed on a clean ubuntu:24.04: 'This version requires zstd for extraction'. The docs do not list zstd; only the error names it. Full Ubuntu desktop and server images usually ship zstd, minimal containers do not.
- Run as root in the container; on a desktop install.sh asks for the sudo password once (one more prompt).
- The container shares abbey1's WSL2 kernel, so install.sh takes its WSL2 branch and exits before any service setup.
- install.sh printed 'The Ollama API is now available at 127.0.0.1:11434' (an exit trap) but no server was running; 'ollama serve' was needed (docs: start it 'if it is not already running').
- No visible answer within 64 tokens: the model spent them reasoning. Asked again with max_tokens 2048 to time the first visible answer (ttft_uncapped.json).

### llama.cpp, GitHub release tarball + llama-server

Version: `b12478`

Actions (run 1):

1. open github.com/ggml-org/llama.cpp/releases and copy the ubuntu-x64 CPU asset link from the newest b* build
2. `curl -LO https://github.com/ggml-org/llama.cpp/releases/download/b12478/llama-b12478-bin-ubuntu-x64.tar.gz`
3. `tar xzf llama-b12478-bin-ubuntu-x64.tar.gz`
4. cd into the extracted folder and run ./llama-server -hf ggml-org/gemma-3-1b-it-GGUF
5. `apt-get install libgomp1 (named only by the loader error)`
6. start the server again
7. send the prompt (open http://localhost:9931, or curl /v1/chat/completions)

Notes:

- GitHub marks v1.0.0 as the Latest release, and it carries no binaries (only nightly-tag.txt). The CPU tarball is on the b* builds, one of about 20 Linux assets.
- Server failed on a clean ubuntu:24.04: 'error while loading shared libraries: libgomp.so.1'. Installed libgomp1; the docs do not list it. Full Ubuntu installs usually have it.
- The server listened on port 9931, not 8080.

First answer (run 1): "Here are three primary colors:\n\n1.  **Red**\n2.  **Yellow**\n3.  **Blue**\n\nThese colors are considered the foundational colors because they cannot be created by mixing other colors together."

### llama.cpp, llama.app installer + llama serve (README today)

Version: `version: 1.0.0-dev (build 12375, commit 9d25e0014)`

Actions (run 1):

1. `curl -LsSf https://llama.app/install.sh | sh`
2. ~/.llama-app/llama serve -hf ggml-org/gemma-3-1b-it-GGUF (the installer's no-PATH-change command)
3. send the prompt (open http://localhost:9931, or curl /v1/chat/completions)

Notes:

- README's 'llama serve' gives 'command not found' right after install: ~/.local/bin is not on PATH. The installer says to add it to the shell profile and open a new terminal (2 more actions), or to run ~/.llama-app/llama serve now; the count uses that shorter path.
- The server listened on port 9931, not 8080.

First answer (run 1): "Here are three primary colors:\n\n1.  **Red**\n2.  **Yellow**\n3.  **Blue**\n\nThese colors are considered the foundational colors because they cannot be created by mixing other colors together."

### LM Studio headless (llmster + lms)

Version: `[38;5;166m   __   __  ___  ______          ___        _______   ____[0m [38;5;214m  / /  /  |/  / / __/ /___ _____/ (_)__    / ___/ /  /  _/[0m [38;5;226m / /__/ /|_/ / _\ \/ __/ // / _  / / _ \  / /__/ /___/ /  [0m [38;5;46m/____/_/  /_/ /___/\__/\_,_/\_,_/_/\___/  \___/____/___/  [0m  lms is LM Studio's CLI utility for your models, server, and inference runtime. CLI commit: 138dcff  Docs: https://lmstudio.ai/docs/developer Join our Discord: https://discord.gg/lmstudio Contribute: https://github.com/lmstudio-ai/lms`

Actions (run 1):

1. `curl -fsSL https://lmstudio.ai/install.sh | bash`
2. `apt-get install -y libatomic1 (printed by the installer)`
3. `curl -fsSL https://lmstudio.ai/install.sh | bash (again)`
4. restart the shell or export PATH (installer: 'To add lms to your PATH, either restart your shell or run export PATH=...')
5. `lms daemon up`
6. `lms get https://huggingface.co/ggml-org/gemma-3-1b-it-GGUF`
7. `lms ls (docs: find the model_key with lms ls)`
8. `lms load gemma-3-1b-it`
9. `apt-get install libgomp1`
10. `lms load gemma-3-1b-it (again)`
11. `lms server start`
12. send the prompt (curl to http://localhost:1234/v1/chat/completions, or lms chat)

Notes:

- install.sh stopped on a clean ubuntu:24.04: 'Missing required dependency' (libatomic1). It prints the apt command and asks to run the installer again.
- install.sh only warned 'Could not detect libgomp. You might need to install libgomp' and reported success.
- lms get and lms load ran with --yes so no confirmation prompt could block; without it they may ask to confirm.
- lms load failed: 'Engine protocol runtime llama-server ... exited before becoming healthy. exitCode=127' (missing libgomp). The error does not name the library; the installer's warning did.

First answer (run 1): "Here are three primary colors:\n\n1.  **Red**\n2.  **Yellow**\n3.  **Blue** \n\nThese colors can be mixed to create a wide range of other colors! \ud83d\ude0a"

### Open WebUI :ollama image (Docker)

Version: `{"version":"0.13.4","deployment_id":""}`

Actions (run 1):

1. `docker run -d -p 3000:8080 -v ollama:/root/.ollama -v open-webui:/app/backend/data --name open-webui --restart always ghcr.io/open-webui/open-webui:ollama`
2. open http://localhost:3000 in a browser
3. create the admin account (name, email, password)
4. Admin Panel > Settings > Connections
5. click Manage on the Ollama connection
6. type gemma3:1b and click download
7. pick gemma3:1b in the chat's model selector
8. type the prompt

Notes:

- Docker is a prerequisite and was already on the host; installing Docker is not counted.
- Ran without '-p 3000:8080' so nothing listened on abbey1's network; the API was reached inside the container.

First answer (run 1): "Here are three primary colors:\n\n1.  **Red**\n2.  **Yellow**\n3.  **Blue**\n\nThese colors are considered \u201cprimary\u201d because they cannot be created by mixing other colors together.\n\n"

### GPT4All Python SDK

Version: `gpt4all 2.8.2`

Actions (run 1):

1. `apt-get install python3-venv (docs: install into its own virtual environment)`
2. `python3 -m venv venv`
3. `source venv/bin/activate`
4. `pip install gpt4all`
5. write chat.py from the docs example
6. `python chat.py`

Notes:

- python3 is preinstalled on Ubuntu desktop but not in the container, so install_python includes it.
- The normalized gemma-3-1b-it-Q4_K_M.gguf does not load in the SDK: 'RuntimeError: Unable to instantiate model: Unsupported model architecture: gemma3' (separate untimed probe, gpt4all 2.8.2), so the closest catalog model was used.

First answer (run 1): "Three primary colors are:\n\n1. Red\n2. Blue\n3. Yellow"

### Jan

Actions (run 1):

1. download the AppImage from jan.ai
2. `chmod +x ./Jan_{version}_amd64.AppImage`
3. `./Jan_{version}_amd64.AppImage`
4. type the prompt (Jan downloads its default model on first launch)

Notes:

- No headless local path: since Desktop 0.8.5 the jan CLI is Jan Agent, which has no local inference engine; the old 'jan serve' commands are gone.
- Docs recommend Flatpak (flatpak install flathub ai.jan.Jan, flatpak run ai.jan.Jan); stock Ubuntu has no flatpak, so that path adds apt install flatpak and flatpak remote-add.
- AppImages need libfuse2 (libfuse2t64 on Ubuntu 24.04), which stock Ubuntu 24.04 lacks; Jan's docs do not mention it.
- The default model's size is not stated in the docs and was not measured.

### GPT4All desktop

Actions (run 1):

1. download the Linux installer (.run)
2. `chmod +x gpt4all-installer-linux.run (not in the docs, required to run it)`
3. run the installer and click through its wizard (several screens, counted as one)
4. open GPT4All
5. click Start Chatting
6. click + Add Model
7. download a model (docs suggest Llama 3)
8. go to Chats
9. click Load Default Model
10. type the prompt

Notes:

- This is the online installer; it downloads the app components during the wizard, so these bytes are only the installer. The offline installer on GitHub v3.10.0 is 732,745,048 bytes.
- Last desktop release v3.10.0 is from 2025-02-25. Headless path measured separately: gpt4all.sh (Python SDK).

### Msty Studio

Actions (run 1):

1. download Msty Studio Desktop (x64 AppImage)
2. `chmod a+x ./MstyStudio_x86_64.AppImage`
3. `./MstyStudio_x86_64.AppImage --no-sandbox`
4. choose Just exploring, For work or I'm a developer
5. accept the recommended local model (Msty installs Ollama or llama.cpp plus the model)
6. choose Open a normal conversation
7. type the prompt

Notes:

- Docs: 'You may need to install appropriate packages like libfuse2' to run the AppImage.
- Msty Sidecar is a separate tray app that serves a desktop to the browser app, not a headless local model path.

### Pinokio

Actions (run 1):

1. download the AppImage from GitHub releases
2. `chmod +x Pinokio-*.AppImage`
3. run the AppImage
4. first-run settings: confirm the Pinokio home folder and save
5. open Discover and pick a chat app script (for example Open WebUI or Ollama)
6. click Install and confirm
7. click Start
8. download a model inside that app
9. type the prompt

Notes:

- Pinokio is a launcher for other apps' install scripts, not a model runner; the steps after Install depend on the chosen app. Counted as a lower bound.
- Pinokio installs its own conda, git and node on first use, which adds downloads not measured here.

## Replaced runs

These runs were measured, then replaced by later runs of the same rival. The batch logs in `logs/batch-logs/` keep them, and no reason was recorded for replacing them. The last column is the median over every run, kept and replaced. The target Fonsviva has to beat stays the lower kept figure.

| Rival | Replaced run (total s, download s) | Kept runs (total s) | Median of every run (total s) |
|---|---|---|---|
| llama.cpp, llama.app installer | run 1: 19.93, 14.82 | 13.17, 14.30 | 14.30 |
| llama.cpp, release tarball | run 1: 639.23, 617.81 | 17.74, 21.86 | 21.86 |
| Ollama (install.sh), normalized model | run 1: 56.90, 23.94; run 2: 39.10, 10.16 | 37.95, 44.58 | 41.84 |

Known limits, to fix in the run that measures Fonsviva: with two runs the median is the mean, so one slow download sets a row (LM Studio run 1 downloaded at about 4.4 MB/s against an 87 MB/s baseline). Every rival that answers over HTTP is charged about 2 s between load end and the request, for starting the helper container; the GPT4All SDK answers in process and is not.

## Reproduce

On a Linux docker host: `./netspeed.sh`, then each rival script with its variant and run number (`./ollama.sh normalized 1`, `./llamacpp.sh release 1`, `./lmstudio.sh 1`, `./openwebui.sh 1`, `./gpt4all.sh 1`, `./gui-installers.sh 1`), or `./run-all.sh`. Then `python3 -I collect.py`. Containers are named `fv-rival-*` and removed when each run ends.
