#!/bin/sh
set -eu
if [ $# -ne 4 ]; then
  echo "usage: pack.sh <binary> <tag> <target> <out dir>" >&2
  exit 2
fi
bin=$1 tag=$2 target=$3 out=$4
name="fonsviva-$tag-$target.tar.gz"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp "$bin" "$stage/fonsviva"
chmod 0755 "$stage/fonsviva"
mkdir -p "$out"
COPYFILE_DISABLE=1 tar -C "$stage" -czf "$out/$name" fonsviva
listed=$(tar -tzf "$out/$name")
if [ "$listed" != "fonsviva" ]; then
  echo "pack.sh: $name holds more than the binary: $listed" >&2
  exit 1
fi
echo "$out/$name"
