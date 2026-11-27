#!/bin/sh
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
pack="$root/scripts/release/pack.sh"
sums="$root/scripts/release/sums.sh"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
fail() {
  echo "release-sums-test: $*" >&2
  exit 1
}
printf '#!/bin/sh\necho one\n' > "$scratch/one"
printf '#!/bin/sh\necho two\n' > "$scratch/two"
dist="$scratch/dist"
"$pack" "$scratch/one" v0.0.0-test x86_64-unknown-linux-gnu "$dist" >/dev/null
"$pack" "$scratch/two" v0.0.0-test aarch64-apple-darwin "$dist" >/dev/null
a="$dist/fonsviva-v0.0.0-test-x86_64-unknown-linux-gnu.tar.gz"
b="$dist/fonsviva-v0.0.0-test-aarch64-apple-darwin.tar.gz"
[ "$(tar -tzf "$a")" = fonsviva ] || fail "tarball holds more than fonsviva"
mkdir "$scratch/unpack"
tar -C "$scratch/unpack" -xzf "$a"
[ -x "$scratch/unpack/fonsviva" ] || fail "unpacked binary is not executable"
cmp -s "$scratch/one" "$scratch/unpack/fonsviva" || fail "unpacked binary differs"

"$sums" write "$dist" >/dev/null || fail "write refused fresh tarballs"
[ "$(wc -l < "$dist/SHA256SUMS" | tr -d ' ')" = 2 ] || fail "SHA256SUMS should have two lines"
grep -Eq '^[0-9a-f]{64}  fonsviva-v0.0.0-test-aarch64-apple-darwin.tar.gz$' "$dist/SHA256SUMS" || fail "SHA256SUMS line is not in sha256sum format"
"$sums" check "$dist" >/dev/null || fail "check refused matching tarballs"

cp "$b" "$scratch/b.keep"
printf 'x' >> "$b"
if "$sums" check "$dist" >/dev/null 2>&1; then fail "check accepted a changed tarball"; fi
cp "$scratch/b.keep" "$b"

cp "$dist/SHA256SUMS" "$scratch/sums.keep"
grep -v aarch64 "$scratch/sums.keep" > "$dist/SHA256SUMS"
if "$sums" check "$dist" >/dev/null 2>&1; then fail "check accepted a tarball with no line"; fi
cp "$scratch/sums.keep" "$dist/SHA256SUMS"

mv "$b" "$scratch/b.gone"
if "$sums" check "$dist" >/dev/null 2>&1; then fail "check accepted a line with no tarball"; fi
mv "$scratch/b.gone" "$b"

"$sums" check "$dist" >/dev/null || fail "check refused the restored tarballs"

printf '#!/bin/sh\necho install\n' > "$dist/install.sh"
"$sums" write "$dist" >/dev/null || fail "write refused the tarballs and install.sh"
grep -Eq '^[0-9a-f]{64}  install.sh$' "$dist/SHA256SUMS" || fail "SHA256SUMS has no line for install.sh"
printf '#\n' >> "$dist/install.sh"
if "$sums" check "$dist" >/dev/null 2>&1; then fail "check accepted a changed install.sh"; fi
echo "release-sums-test: ok"
