#!/usr/bin/env bash
# Writes SHA256SUMS for every azemu-*.tar.gz in a directory (one line per file, `<hash>  <file name>`, the format `sha256sum -c` reads).
#
#   tools/release/checksums.sh <dir>        writes <dir>/SHA256SUMS
#
# Uses sha256sum, or `shasum -a 256` where that is missing (macOS).
set -euo pipefail
[ "$#" -eq 1 ] || { echo "usage: $0 <dir>" >&2; exit 2; }
cd "$1"
shopt -s nullglob
files=(azemu-*.tar.gz)        # the shell sorts a glob
[ "${#files[@]}" -gt 0 ] || { echo "no azemu-*.tar.gz in $1" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then sum() { sha256sum "$@"; }; else sum() { shasum -a 256 "$@"; }; fi
sum "${files[@]}" > SHA256SUMS
cat SHA256SUMS
