# Install the org server

> **Status:** the installer is ready, but Fonsviva has no release yet, so the command below has nothing to download until the first one is out. [SUPPORTED.md](https://github.com/Students-of-Local/Fonsviva/blob/main/SUPPORTED.md) says what works now.

One command sets up the Fonsviva org server on a Linux or macOS machine. It runs as you, never as root, and asks for no password.

```
curl -fsSL https://github.com/Students-of-Local/Fonsviva/releases/latest/download/install.sh | sh -s -- --url https://fonsviva.example.org
```

Put your org's own address after `--url`. That is the address people will open in their browser; a tunnel or your org's VPN points it at this machine later.

## What it does

1. Picks the build for this machine and downloads it with the release's `SHA256SUMS`. It stops with nothing installed if the file doesn't match. Releases are not signed yet; when they are, the installer checks the signature too.
2. Puts the binary at `~/.local/bin/fonsviva`.
3. Makes the org's store at `~/.local/share/fonsviva/org`, readable only by you.
4. Starts the org server as a user service on `127.0.0.1:7350`: a systemd user unit `fonsviva-org.service` on Linux, a launchd agent `com.studentsoflocal.fonsviva-org` on macOS. It restarts if it stops.
5. Waits for the server to answer, then prints a sign-in link for the first admin.

The last lines look like this:

```
Fonsviva v0.1.0 is running at http://127.0.0.1:7350 for https://fonsviva.example.org.
Sign in as the first admin (the link works once, for 24 hours):
https://fonsviva.example.org/join/<token>
```

Open the link, sign in, add a model source, and chat. The link is the only secret the installer shows. It writes nothing to your shell profile.

On Linux the server stops when you log out unless lingering is on. The installer warns you and prints the command (`loginctl enable-linger <you>`) but doesn't run it. On macOS the agent runs in your login session, so on a Mac reached only over SSH, log in on it once, or install with `--no-service` and start the server another way.

## Options

| Option | What it does |
|---|---|
| `--url <org url>` | The org's address. Needed the first time. |
| `--version <tag>` | Install this release, such as `v0.1.0`. The default is the latest stable one. An older version than the one installed asks first. |
| `--prefix <dir>` | Install the binary in `<dir>/bin`. The default is `~/.local`. |
| `--data <dir>` | Keep the store here. The default is `~/.local/share/fonsviva/org`. |
| `--port <n>` | Serve on `127.0.0.1:<n>`. The default is 7350. |
| `--no-service` | Install the binary and the store only, and print the command that starts the server. |
| `--uninstall` | Stop and remove the service and the binary. The store stays. |
| `--yes` | Don't ask before installing an older version. |

Pass options after `sh -s --`, as in the command above.

## Run it again

Running the same command again upgrades the binary, keeps the store, restarts the service and prints a new admin link. An older unused link stops working. A `--url` that differs from the stored one is only a warning; the store keeps its address.

## Check

```
curl http://127.0.0.1:7350/api/org/v1/health
```

answers `{"ok":true}`.

If the server doesn't come up within 20 seconds, the installer stops, prints the last lines of the server's log and the command to read the rest. The log is at `~/.local/state/fonsviva/org.log` on Linux and `~/Library/Logs/fonsviva-org.log` on macOS. The binary and the store stay, so you can fix the problem and run the command again.

## Uninstall

```
curl -fsSL https://github.com/Students-of-Local/Fonsviva/releases/latest/download/install.sh | sh -s -- --uninstall
```

This removes the service and the binary and names the folder it kept. If you installed with `--prefix` or `--data`, pass the same ones here. Delete that folder yourself only if you mean to delete the org's members, keys and history.

## Next steps

The installer sets up the org server only. Machines that lend their compute run the node daemon, and the org's address reaches this server through a tunnel or the org's VPN; [ARCHITECTURE.md](../ARCHITECTURE.md) describes both. If anything fails, see [troubleshoot](troubleshoot.md).
