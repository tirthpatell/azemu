#!/usr/bin/env bash
# Runs the whole Java SDK suite. Extra arguments go to Maven, e.g. ./run.sh -Dtest=BlobTest
# --findings also runs the tests marked @Disabled("azemu: ...") (they fail while the finding stands).
set -euo pipefail
cd "$(dirname "$0")"
MVN="${MVN:-$(command -v mvn || true)}"
if [ -z "$MVN" ] || [ ! -x "$MVN" ]; then
  echo "Maven is not on the PATH: install Apache Maven 3.9 or newer, or set MVN=/path/to/mvn (see README.md)" >&2
  exit 1
fi
if [ -z "${AZEMU_BIN:-}" ] && [ ! -x .azemu-under-test ]; then
  cp ../../target/debug/azemu .azemu-under-test
fi
args=()
for a in "$@"; do
  if [ "$a" = "--findings" ]; then
    args+=("-Dfindings.deactivate=org.junit.jupiter.engine.extension.DisabledCondition")
  else
    args+=("$a")
  fi
done
exec "$MVN" -q test "${args[@]}"
