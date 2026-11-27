#!/bin/sh
set -eu
check="$(cd "$(dirname "$0")/../.." && pwd)/scripts/release/check-tag.sh"
fail() {
  echo "check-tag-test: $*" >&2
  exit 1
}
for tag in v0.1.0 v0.1.0-nightly.3 v0.1.0-rc1; do
  "$check" "$tag" 0.1.0 2>/dev/null || fail "refused $tag for 0.1.0"
done
for tag in v0.2.0 nightly 0.1.0 v0.1.0x v0.1.0- v0.1; do
  if "$check" "$tag" 0.1.0 2>/dev/null; then fail "accepted $tag for 0.1.0"; fi
done
if "$check" v-1 "" 2>/dev/null; then fail "accepted a tag with no version"; fi
if "$check" v 2>/dev/null; then fail "accepted one argument"; fi
echo "check-tag-test: ok"
