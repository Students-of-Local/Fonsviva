#!/bin/sh
set -eu
export LC_ALL=C
if [ $# -ne 2 ] || { [ "$1" != write ] && [ "$1" != check ]; }; then
  echo "usage: sums.sh write|check <dir>" >&2
  exit 2
fi
mode=$1
cd "$2"
if command -v sha256sum >/dev/null 2>&1; then
  sum() { sha256sum "$@"; }
else
  sum() { shasum -a 256 "$@"; }
fi
files=$(for f in fonsviva-*.tar.gz install.sh; do if [ -f "$f" ]; then echo "$f"; fi; done | sort)
if ! ls fonsviva-*.tar.gz >/dev/null 2>&1; then
  echo "sums.sh: no fonsviva-*.tar.gz in $(pwd)" >&2
  exit 1
fi
if [ "$mode" = write ]; then
  sum $files > SHA256SUMS
fi
if [ ! -f SHA256SUMS ]; then
  echo "sums.sh: no SHA256SUMS in $(pwd)" >&2
  exit 1
fi
listed=$(awk '{ sub(/^\*/, "", $2); print $2 }' SHA256SUMS | sort)
if [ "$listed" != "$files" ]; then
  echo "sums.sh: SHA256SUMS lists other files than the tarballs here" >&2
  exit 1
fi
if ! sum -c SHA256SUMS; then
  echo "sums.sh: a tarball does not match SHA256SUMS" >&2
  exit 1
fi
