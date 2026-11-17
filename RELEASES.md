# Releases: stable, nightly and dev

Fonsviva follows the Local Studio family scheme in [local-studio docs/channels.md](https://github.com/baney75/local-studio/blob/main/docs/channels.md). Where this file and that one disagree, that one wins. Fonsviva is a binary app, so it publishes a binary feed per channel like Silas, not only a git ref.

| Channel | Built from | When | Gate |
| --- | --- | --- | --- |
| stable | `stable` branch, tag `vX.Y.Z` | When a nightly is promoted | 72 hours on nightly, no open regression, TESTING.md, API compatibility tests for every wired harness and each dependent app (Local Studio, UntoCaesar, Gantrel, Corunis) run against the candidate, a live check of the app, release notes, the baney-review gate |
| nightly | `main` | 07:00 UTC daily, skipped when `main` has not changed | CI green on Windows, macOS and Linux plus the end-to-end checks in AGENTS.md (setup to first answer on a real small model, and the wiring round trips); until those exist, nightly does not publish |
| dev | `main` | Every merge to `main` | CI green |

Versions: `X.Y.Z`, `X.Y.Z-nightly.YYYYMMDDHHMM+sha7`, `X.Y.Z-dev.YYYYMMDDHHMM+sha7`. Each channel has its own `latest.json` feed and its own signing key; nothing built from `main` can install over a stable app. Windows installers use NSIS, because Tauri's MSI target rejects non-numeric pre-release versions.

Hotfix: a fix for a security, data-loss or spend bug in stable goes to `stable` by pull request with the baney-review gate, skips the 72 hours, ships as the next patch version (`v1.2.3` becomes `v1.2.4`), and is merged back into `main` the same day.

Stable and nightly share the data dir and the model store. Dev uses its own data dir, so a dev build can never damage a working install.

## Update rules for the app

- Updates are signed and verified before they install.
- Settings, keys and the model inventory are backed up before every update, and migrations are reversible.
- Every update can be rolled back to the previous version.
- Updates wait for running jobs and loaded models to finish or pause them safely.
- Downloaded models are never deleted by an update.
- The API stays backward compatible. A breaking change needs a new version path and a migration window announced in the release notes.
- The app shows what changed, in plain words, before and after updating.
