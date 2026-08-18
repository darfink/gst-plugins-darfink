#!/usr/bin/env bash
#
# Prove that vendor/scuffle-rtmp is exactly the published crate plus
# vendor/local-changes.patch.
#
# Vendoring a dependency makes local modifications easy to lose track of: after
# a few edits nobody can say what diverged from upstream. Downloading the
# pristine release, applying the recorded patch, and diffing keeps that answer
# mechanical rather than a matter of memory. Run it in CI so the patch cannot
# drift away from the tree it describes.

set -euo pipefail

version=${VERSION:-0.2.3}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "Downloading pristine scuffle-rtmp $version from crates.io"
curl -sSfL -o "$work/crate.tar.gz" \
  "https://static.crates.io/crates/scuffle-rtmp/scuffle-rtmp-$version.crate"
tar xzf "$work/crate.tar.gz" -C "$work"

mv "$work/scuffle-rtmp-$version" "$work/a"
patch -s -p1 -d "$work/a" < "$here/local-changes.patch"

# .cargo-checksum.json is written by `cargo vendor`, not shipped in the crate.
if ! diff -ruN -x .cargo-checksum.json -x target "$work/a" "$here/scuffle-rtmp"; then
  echo
  echo "vendor/scuffle-rtmp does not match scuffle-rtmp $version + local-changes.patch."
  echo "If you changed the vendored source, regenerate the patch:"
  echo "  diff -ruN -x target -x .cargo-checksum.json <pristine> vendor/scuffle-rtmp > vendor/local-changes.patch"
  exit 1
fi

echo "OK: vendor/scuffle-rtmp == scuffle-rtmp $version + local-changes.patch"
