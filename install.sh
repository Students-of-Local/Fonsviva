#!/bin/sh
# Install the fonsviva CLI on Linux or macOS, with no sudo:
#   curl -fsSL https://raw.githubusercontent.com/baney75/Fonsviva/main/install.sh | sh
# Pick a channel (only dev is published today):
#   curl -fsSL https://raw.githubusercontent.com/baney75/Fonsviva/main/install.sh | sh -s -- --channel dev
# Builds are unsigned. This checks each download's sha256 against the channel's latest.json
# before extracting it, and installs to ~/.local/bin/fonsviva.
# FONSVIVA_DOWNLOAD_BASE replaces the URL the feed is read from (tests only).
set -eu
# A CDPATH from the user's shell would send `cd` to some other folder of the same name.
unset CDPATH
FEEDS="https://github.com/baney75/Fonsviva/releases/download"
say() { printf '%s\n' "$1"; }
die() {
  printf 'fonsviva install: %s\n' "$1" >&2
  exit 1
}

channel=dev
while [ $# -gt 0 ]; do
  case "$1" in
  --channel)
    [ $# -ge 2 ] || die "--channel needs a value, such as --channel dev."
    channel=$2
    shift 2
    ;;
  --channel=*)
    channel=${1#--channel=}
    shift
    ;;
  -h | --help)
    echo "usage: install.sh [--channel dev]"
    exit 0
    ;;
  *) die "Unknown option '$1'. The only option is --channel dev." ;;
  esac
done
case "$channel" in
dev) ;;
stable | nightly) die "The $channel channel is not published yet. Run this again without --channel to get the dev channel." ;;
*) die "Unknown channel '$channel'. Fonsviva publishes only the dev channel today." ;;
esac
[ -n "${HOME:-}" ] || die "HOME is not set, so there is no ~/.local/bin to install into."
base=${FONSVIVA_DOWNLOAD_BASE:-$FEEDS/$channel}
base=${base%/}

os=$(uname -s)
arch=$(uname -m)
case "$os" in
Linux) os_target=unknown-linux-gnu ;;
Darwin)
  os_target=apple-darwin
  # uname -m says x86_64 in a Rosetta terminal, so ask the hardware instead.
  [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" != 1 ] || arch=arm64
  ;;
*) die "Fonsviva has no build for $os. Builds exist for Linux and macOS (this script) and Windows (install.ps1)." ;;
esac
case "$arch" in
x86_64 | amd64) cpu=x86_64 ;;
aarch64 | arm64) cpu=aarch64 ;;
*) die "Fonsviva has no build for $os on $arch CPUs. Builds exist for x86_64 and arm64." ;;
esac
target=$cpu-$os_target

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  die "This needs curl or wget to download Fonsviva. Install one and run this again."
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
elif command -v openssl >/dev/null 2>&1; then
  sha256() { openssl dgst -sha256 -r "$1" | cut -d' ' -f1; }
else
  die "This needs sha256sum, shasum or openssl to check the download."
fi

dir="$HOME/.local/bin"
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t fonsviva)
staged=""
trap 'rm -rf "$tmp"; [ -z "$staged" ] || rm -f "$staged"' EXIT
trap 'exit 130' INT TERM

fetch "$base/latest.json" "$tmp/latest.json" ||
  die "Couldn't read the $channel feed at $base/latest.json. Check your connection and try again."
feed=$(tr -d '\r\n' <"$tmp/latest.json")
# The value of a string field in a flat piece of JSON. The feed is written by
# .github/workflows/release-dev.yml; this avoids needing jq or python on a clean machine.
field() { printf '%s' "$2" | sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p"; }
version=$(field version "$feed")
asset=$(printf '%s' "$feed" | sed -n "s/.*\"$target\"[[:space:]]*:[[:space:]]*{\([^}]*\)}.*/\1/p")
[ -n "$asset" ] || die "The $channel channel has no build for $target yet."
url=$(field url "$asset")
want=$(field sha256 "$asset" | tr 'A-F' 'a-f')
[ -n "$url" ] || die "The $channel feed lists $target without a download URL."
case "$want" in
*[!0-9a-f]* | "") die "The $channel feed lists $target without a valid sha256." ;;
esac
[ ${#want} -eq 64 ] || die "The $channel feed lists $target without a valid sha256."

say "Downloading fonsviva $version for $target"
archive="$tmp/fonsviva.tar.gz"
fetch "$url" "$archive" || die "Couldn't download $url. Check your connection and try again."
got=$(sha256 "$archive")
[ "$got" = "$want" ] ||
  die "The download's sha256 is $got, but the $channel feed says $want. It may be damaged or tampered with. Nothing was installed."
mkdir "$tmp/x"
tar -xzf "$archive" -C "$tmp/x" || die "Couldn't unpack $url. Nothing was installed."
bin=$(find "$tmp/x" -type f -name fonsviva | head -n 1)
[ -n "$bin" ] || die "The archive for $target has no fonsviva binary in it. Nothing was installed."

# Stage next to the target, check the binary runs here (a too-old glibc, a noexec /tmp),
# then move it into place, which also replaces a copy that is running.
mkdir -p "$dir"
staged="$dir/.fonsviva.new.$$"
cp "$bin" "$staged"
chmod 755 "$staged"
"$staged" --version >/dev/null 2>&1 ||
  die "The downloaded fonsviva won't run on this machine ($target). Nothing was installed."
mv -f "$staged" "$dir/fonsviva"
staged=""

case ":${PATH:-}:" in
*":$dir:"*) cmd=fonsviva ;;
*)
  cmd="$dir/fonsviva"
  case "$cmd" in *" "*) cmd="\"$cmd\"" ;; esac
  ;;
esac
say "Installed fonsviva $version ($channel channel, unsigned, sha256 verified) at $dir/fonsviva"
say "Next, get a first local answer:"
say "  $cmd setup --yes && $cmd ask \"Name three primary colors.\""
[ "$cmd" = fonsviva ] || say "($dir is not on your PATH yet. Add it to type plain fonsviva.)"
