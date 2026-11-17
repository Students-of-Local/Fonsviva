# Install Fonsviva

> **Status: dev channel only.** The `fonsviva` CLI installs today from the dev channel: a build of every merge to `main`, unsigned, with each download checked against a sha256 before it is unpacked. Stable and nightly are not published yet (see [RELEASES.md](https://github.com/baney75/Fonsviva/blob/main/RELEASES.md)), and the desktop app has no release. [SUPPORTED.md](https://github.com/baney75/Fonsviva/blob/main/SUPPORTED.md) is the source of truth for what works now. An agent must check SUPPORTED.md first and tell the person plainly if a step isn't supported yet.

## Install the CLI (dev channel)

Linux or macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/baney75/Fonsviva/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/baney75/Fonsviva/main/install.ps1 | iex
```

The installer picks the build for this machine, reads the channel feed (`latest.json` on the [`dev` prerelease](https://github.com/baney75/Fonsviva/releases/tag/dev)), downloads the archive, refuses it if its sha256 doesn't match the feed, and installs `fonsviva` without sudo or admin rights:

| System | Build | Installs to |
| --- | --- | --- |
| Linux x86_64, arm64 (glibc 2.35 or newer) | `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` | `~/.local/bin/fonsviva` |
| macOS Apple silicon, Intel | `aarch64-apple-darwin`, `x86_64-apple-darwin` | `~/.local/bin/fonsviva` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `%LOCALAPPDATA%\Fonsviva\bin\fonsviva.exe`, added to your user PATH |

The builds are not code signed. The installer tests in `tests/install/` run on CI for Linux x86_64, macOS Apple silicon and Windows x86_64; the Linux arm64 and macOS Intel builds are published but their install is not tested yet. A file the installer downloads is not marked as coming from the internet; an archive you download in a browser is, and macOS or Windows may then warn about or block the unsigned binary.

It ends by printing the one command for a first local answer:

```sh
fonsviva setup --yes && fonsviva ask "Name three primary colors."
```

On Linux and macOS, if `~/.local/bin` is not on your PATH, the printed command uses the full path. On Windows the command is joined with `;` instead of `&&`.

Check: `fonsviva --version` prints `fonsviva X.Y.Z-dev.YYYYMMDDHHMM+sha7`. Dev builds keep their own data dir and run the daemon on port 7342, so they never touch a stable install.

Options: `install.sh` takes `--channel dev` (pass it as `sh -s -- --channel dev`), and `install.ps1` takes `-Channel dev` and `-NoModifyPath`. `stable` and `nightly` refuse with "not published yet" until those channels publish.

## Planned

Fonsviva can be set up on its own, before any app. Apps that find it will use it; apps that don't will install it for you (see each app's install page).

### Steps

1. Install Fonsviva from the [latest Stable release](https://github.com/baney75/Fonsviva/releases). (No stable release yet; use the dev channel above.)
2. Run `fonsviva setup`. It scans this machine, lists models already installed by Ollama, LM Studio, llama.cpp, vLLM, SGLang and the Hugging Face cache, and recommends a first model that fits.
3. Open the web app at `http://127.0.0.1:7341`.

### Check

Run `fonsviva status`. Expected output:

```
Fonsviva <version> running at http://127.0.0.1:7341
Profile: personal
Nodes: 1 (this machine)
Models: <number found>
Cloud providers: off
```

`fonsviva status --json` returns the same as JSON with `"running": true`. `curl http://127.0.0.1:7341/health` returns `{"status":"ok"}`.

If any check fails, see [troubleshoot](troubleshoot.md).
