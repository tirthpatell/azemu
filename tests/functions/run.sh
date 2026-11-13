#!/usr/bin/env bash
# Builds the function app (net10.0, isolated worker) if needed and runs tests/functions/run.py. See README.md.
#   tests/functions/run.sh                    the .NET app, every group (about a minute)
#   tests/functions/run.sh --only sb,eh       some groups
#   tests/functions/run.sh --app node         the Node app (needs network the first time: npm packages, extension bundle)
#   tests/functions/run.sh --app durable      the Durable Functions app
#   REBUILD=1 tests/functions/run.sh          rebuild first
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
# the .NET SDK: DOTNET_ROOT if set, else the one on the PATH (refs/tools/dotnet is tried last)
if [ -z "${DOTNET_ROOT:-}" ] && ! command -v dotnet >/dev/null 2>&1 && [ -x "$root/refs/tools/dotnet/dotnet" ]; then export DOTNET_ROOT="$root/refs/tools/dotnet"; fi
[ -n "${DOTNET_ROOT:-}" ] && export PATH="$DOTNET_ROOT:$PATH"
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1 FUNCTIONS_CORE_TOOLS_TELEMETRY_OPTOUT=1
export DOTNET_gcServer=0 DOTNET_GCHeapHardLimit=0x30000000     # small heap, one dotnet at a time
mkdir -p "${AZEMU_FUNC_SCRATCH:-$HOME/.cache/azemu-functions}"
app=dotnet
prev=""
for a in "$@"; do
  [ "$prev" = "--app" ] && app="$a"
  prev="$a"
done
build() {  # project dir, output dir, main source file
  if [ ! -d "$here/$2" ] || [ "${REBUILD:-0}" = 1 ] || [ "$here/$1/$3" -nt "$here/$2/$1.dll" ]; then
    (cd "$here/$1" && dotnet build -c Release -o "../$2" -m:1 -nodeReuse:false -p:UseSharedCompilation=false -v:q -nologo)
    dotnet build-server shutdown >/dev/null 2>&1 || true
  fi
}
case "$app" in
  dotnet)  build FuncApp out Functions.cs ;;
  durable) build DurableApp out-durable Functions.cs ;;
  node)    [ -d "$here/NodeApp/node_modules" ] || (cd "$here/NodeApp" && npm install --no-audit --no-fund) ;;
esac
PY="${PYTHON:-$root/.venv/bin/python}"
[ -x "$PY" ] || PY=python3
exec "$PY" "$here/run.py" "$@"
