#!/usr/bin/env bash
# Runs the whole .NET suite against a copy of target/debug/azemu. Needs the .NET 10 SDK (on the PATH, or DOTNET_ROOT).
#   ./run.sh                          everything
#   ./run.sh --filter "FullyQualifiedName~ServiceBus"   any dotnet test arguments
#   AZEMU_BIN=/path/to/azemu ./run.sh another emulator binary
#   ./run.sh --refresh                copy ../../target/debug/azemu to .azemu-under-test first
#   ./run.sh --findings               also run the tests skipped as findings (they fail while the finding stands)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1
export DOTNET_SYSTEM_GLOBALIZATION_INVARIANT="${DOTNET_SYSTEM_GLOBALIZATION_INVARIANT:-0}"
if [ -n "${DOTNET_ROOT:-}" ] && [ -x "$DOTNET_ROOT/dotnet" ]; then
  DOTNET="$DOTNET_ROOT/dotnet"
elif command -v dotnet >/dev/null 2>&1; then
  DOTNET="$(command -v dotnet)"
else
  echo "the .NET SDK is not installed: install .NET 10 or set DOTNET_ROOT to its directory (see README.md)" >&2; exit 2
fi
args=()
export AZEMU_FINDINGS=0
for a in "$@"; do
  if [ "$a" = "--findings" ]; then export AZEMU_FINDINGS=1; elif [ "$a" = "--refresh" ]; then cp "$root/target/debug/azemu" "$here/.azemu-under-test"; echo "refreshed copy: $(ls -l --time-style=long-iso "$here/.azemu-under-test" | awk '{print $5, $6, $7}')"; else args+=("$a"); fi
done
if [ -z "${AZEMU_BIN:-}" ] && [ ! -x "$here/.azemu-under-test" ]; then
  cp "$root/target/debug/azemu" "$here/.azemu-under-test"
fi
mkdir -p "$here/.tmp" "$here/logs"
export TMPDIR="$here/.tmp"
# keep one workstation GC, one build node, no compiler servers left behind
export DOTNET_gcServer=0
cd "$here"
"$DOTNET" build Azemu.DotnetTests/Azemu.DotnetTests.csproj -m:1 -nodeReuse:false -p:UseSharedCompilation=false -v:q -nologo
"$DOTNET" build-server shutdown >/dev/null 2>&1 || true
# the test process gets a 768 MB heap limit
export DOTNET_GCHeapHardLimit="${DOTNET_GCHeapHardLimit:-0x30000000}"
"$DOTNET" test Azemu.DotnetTests/Azemu.DotnetTests.csproj --no-build -m:1 "${args[@]}" --logger "console;verbosity=normal"
