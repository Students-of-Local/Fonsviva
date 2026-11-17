#!/usr/bin/env bash
# Tests install.sh against a fake dev feed served from a scratch dir, with a scratch HOME.
# usage: tests/install/install_test.sh <fonsviva binary> <version it was built with>
# CI builds the binary with FONSVIVA_VERSION=<version> so `--version` proves the stamp too.
set -euo pipefail
bin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
version=$2
repo=$(cd "$(dirname "$0")/../.." && pwd)
[ -x "$bin" ] || { echo "no binary at $bin" >&2; exit 2; }

scratch=$(mktemp -d)
server=""
cleanup() {
  [ -z "$server" ] || kill "$server" 2>/dev/null || true
  rm -rf "$scratch"
}
trap cleanup EXIT

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1" >&2; fails=$((fails + 1)); }

case "$(uname -s)" in
Linux) os=unknown-linux-gnu ;;
Darwin) os=apple-darwin ;;
*) echo "run this on Linux or macOS" >&2; exit 2 ;;
esac
case "$(uname -m)" in
x86_64 | amd64) cpu=x86_64 ;;
aarch64 | arm64) cpu=aarch64 ;;
esac
# An arm Mac running this under Rosetta still gets the arm64 build.
[ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" != 1 ] || cpu=aarch64
target=$cpu-$os

# The archive, laid out as release-dev.yml packs it.
srv=$scratch/srv
name="fonsviva-$version-$target"
mkdir -p "$srv/pkg/$name" "$srv/dev" "$srv/bad"
cp "$bin" "$srv/pkg/$name/fonsviva"
tar -C "$srv/pkg" -czf "$srv/$name.tar.gz" "$name"
if command -v sha256sum >/dev/null; then sum=$(sha256sum "$srv/$name.tar.gz" | cut -d' ' -f1); else sum=$(shasum -a 256 "$srv/$name.tar.gz" | cut -d' ' -f1); fi

port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
url="http://127.0.0.1:$port"
feed() { # sha256 to list
  printf '{\n  "channel": "dev",\n  "version": "%s",\n  "sha": "0000000000000000000000000000000000000000",\n  "published_at": "2000-01-01T00:00:00Z",\n  "assets": {\n    "%s": {\n      "url": "%s/%s.tar.gz",\n      "sha256": "%s"\n    }\n  }\n}\n' \
    "$version" "$target" "$url" "$name" "$1"
}
feed "$sum" >"$srv/dev/latest.json"
feed 0000000000000000000000000000000000000000000000000000000000000000 >"$srv/bad/latest.json"

python3 -m http.server --bind 127.0.0.1 --directory "$srv" "$port" >"$scratch/http.log" 2>&1 &
server=$!
for _ in $(seq 50); do
  python3 -c "import urllib.request; urllib.request.urlopen('$url/dev/latest.json')" 2>/dev/null && break
  sleep 0.1
done

# Runs install.sh with nothing from the real user: a fresh HOME and XDG dirs per case.
run() { # case name, feed dir, extra PATH dir or "", install.sh args...
  local home=$scratch/$1/home feed=$2 extra=$3
  mkdir -p "$home"
  shift 3
  env -i HOME="$home" PATH="${extra:+$extra:}/usr/bin:/bin:/usr/sbin:/sbin" \
    XDG_CONFIG_HOME="$home/.config" XDG_DATA_HOME="$home/.local/share" XDG_CACHE_HOME="$home/.cache" \
    FONSVIVA_HOME="$home/fonsviva" FONSVIVA_PORT=0 FONSVIVA_DOWNLOAD_BASE="$url/$feed" \
    sh "$repo/install.sh" "$@"
}

# 1. The good feed installs a binary that runs and reports the feed's version.
if out=$(run good dev "" 2>&1); then pass "install.sh exits 0"; else fail "install.sh exited non-zero: $out"; fi
home=$scratch/good/home
if cmp -s "$bin" "$home/.local/bin/fonsviva"; then pass "binary lands in ~/.local/bin"; else fail "~/.local/bin/fonsviva missing or different"; fi
got=$("$home/.local/bin/fonsviva" --version 2>&1 || true)
if [ "$got" = "fonsviva $version" ]; then pass "--version prints fonsviva $version"; else fail "--version printed '$got'"; fi
case "$out" in
*"$home/.local/bin/fonsviva setup --yes && $home/.local/bin/fonsviva ask"*) pass "prints the next step with the full path" ;;
*) fail "no next step in: $out" ;;
esac
if [ -z "$(find "$home/.local/bin" -name '.fonsviva.new.*')" ]; then pass "no staged file left"; else fail "staged file left behind"; fi
# Installing again over an existing copy works.
if run good dev "" >/dev/null 2>&1 && cmp -s "$bin" "$home/.local/bin/fonsviva"; then pass "reinstall replaces the binary"; else fail "reinstall failed"; fi

# 2. A sha256 that doesn't match is refused and nothing is installed.
if out=$(run bad bad "" 2>&1); then fail "bad sha256 was accepted"; else pass "bad sha256 exits non-zero"; fi
case "$out" in *"sha256"*"Nothing was installed"*) pass "bad sha256 says why" ;; *) fail "bad sha256 message: $out" ;; esac
if [ ! -e "$scratch/bad/home/.local/bin" ] || [ -z "$(ls -A "$scratch/bad/home/.local/bin")" ]; then pass "bad sha256 installs nothing"; else fail "bad sha256 left files in ~/.local/bin"; fi

# 3. An unknown CPU gets a plain error before any download.
fake=$scratch/fakebin
mkdir -p "$fake"
printf '#!/bin/sh\ncase "$1" in -s) echo Linux ;; -m) echo riscv64 ;; *) echo Linux ;; esac\n' >"$fake/uname"
chmod 755 "$fake/uname"
if out=$(run arch dev "$fake" 2>&1); then fail "unknown arch was accepted"; else pass "unknown arch exits non-zero"; fi
case "$out" in *"no build for Linux on riscv64"*) pass "unknown arch says why" ;; *) fail "unknown arch message: $out" ;; esac
if [ ! -e "$scratch/arch/home/.local/bin/fonsviva" ]; then pass "unknown arch installs nothing"; else fail "unknown arch installed a binary"; fi

# 4. Channels that aren't published refuse plainly.
for ch in stable nightly; do
  if out=$(run "$ch" dev "" --channel "$ch" 2>&1); then fail "--channel $ch was accepted"; else
    case "$out" in *"$ch channel is not published yet"*) pass "--channel $ch refuses" ;; *) fail "--channel $ch message: $out" ;; esac
  fi
done

if [ "$fails" -ne 0 ]; then
  echo "$fails install test(s) failed" >&2
  exit 1
fi
echo "all install tests passed"
