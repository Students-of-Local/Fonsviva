#!/bin/sh
set -eu
if [ $# -ne 1 ]; then
  echo "usage: version-test.sh <fonsviva binary>" >&2
  exit 2
fi
out=$("$1" --version)
tag=${FONSVIVA_RELEASE_TAG:-}
want_version=${tag#v}
want_version=${want_version:-$(sed -n 's/^version = "\(.*\)"$/\1/p' "$(dirname "$0")/../../Cargo.toml" | head -n 1)}
want_commit=$(git -C "$(dirname "$0")" rev-parse --short HEAD)
want="fonsviva $want_version ($want_commit)"
if [ "$out" != "$want" ]; then
  echo "version-test: got '$out', want '$want'" >&2
  exit 1
fi
echo "version-test: $out"
