#!/bin/sh
set -eu
if [ $# -ne 2 ]; then
  echo "usage: check-tag.sh <tag> <Cargo.toml version>" >&2
  exit 2
fi
tag=$1
version=$2
if [ -z "$version" ]; then
  echo "check-tag.sh: no version in Cargo.toml" >&2
  exit 1
fi
case "$tag" in
  "v$version" | "v$version"-?*) ;;
  *)
    echo "check-tag.sh: tag $tag does not match the Cargo.toml version $version (want v$version or v$version-<label>)" >&2
    exit 1
    ;;
esac
