#!/usr/bin/env bash
# Packs one release binary into azemu-<version>-<target>.tar.gz, with the licence files inside.
#
#   tools/release/package.sh <binary> <version> <target> <out-dir>
#
#   tools/release/package.sh target/release/azemu 0.1.0 x86_64-unknown-linux-gnu dist
#     -> dist/azemu-0.1.0-x86_64-unknown-linux-gnu.tar.gz, which holds
#        azemu-0.1.0-x86_64-unknown-linux-gnu/{azemu, README.md, LICENSE-MIT, LICENSE-APACHE, NOTICE, THIRD-PARTY-LICENSES.md, LICENSES/}
#
# Run it from anywhere; it finds the licence files next to this script's repository. Works with GNU tar and with the tar of macOS.
set -euo pipefail
if [ "$#" -ne 4 ]; then echo "usage: $0 <binary> <version> <target> <out-dir>" >&2; exit 2; fi
bin="$1"; version="$2"; target="$3"; out="$4"
root="$(cd "$(dirname "$0")/../.." && pwd)"
[ -x "$bin" ] || { echo "not an executable file: $bin" >&2; exit 1; }
name="azemu-${version}-${target}"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name" "$out"
cp "$bin" "$stage/$name/azemu"
chmod 755 "$stage/$name/azemu"
for f in README.md LICENSE-MIT LICENSE-APACHE NOTICE THIRD-PARTY-LICENSES.md; do
    [ -f "$root/$f" ] || { echo "missing $root/$f" >&2; exit 1; }
    cp "$root/$f" "$stage/$name/$f"
done
[ -d "$root/LICENSES" ] || { echo "missing $root/LICENSES" >&2; exit 1; }
cp -R "$root/LICENSES" "$stage/$name/LICENSES"
archive="$(cd "$out" && pwd)/$name.tar.gz"
(cd "$stage" && tar -czf "$archive" "$name")
echo "$archive"
