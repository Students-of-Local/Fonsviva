# What Fonsviva supports

This file is the honest list of what Fonsviva can and cannot do today. Only a situation with a passing test suite in `tests/` may be listed as supported: an engine, a device type, a harness, or a feature.

## Supported

- **Installing the `fonsviva` CLI from the dev channel.** `install.sh` (Linux, macOS) and `install.ps1` (Windows) pick the build for the machine, read the dev feed, refuse a download whose sha256 doesn't match it, and install without sudo or admin rights ([install](docs/setup/install.md)). Tested on CI with a local fake feed by `tests/install/install_test.sh` on Linux x86_64 and macOS Apple silicon, and `tests/install/install_test.ps1` on Windows x86_64 under Windows PowerShell 5.1 and PowerShell 7. Dev builds only: unsigned, sha256-verified. Stable and nightly are not published.

## Not supported yet

- Installing on Linux arm64 or an Intel Mac: the builds are published, but no test installs them yet.
- Signed builds, and the stable and nightly channels.
- Everything else. Items move up only when their tests pass.
