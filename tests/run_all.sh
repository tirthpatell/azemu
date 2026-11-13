#!/usr/bin/env bash
# Run every test suite against one emulator binary and print a short summary.
#
#   tests/run_all.sh                      # uses target/debug/azemu
#   tests/run_all.sh /path/to/azemu       # another binary (for example a copy, while the tree is being rebuilt)
#   SKIP="java rust" tests/run_all.sh     # leave suites out: python replay rust js go java dotnet
#   SCORES_DIR=/path tests/run_all.sh     # the replay writes its scores there, not into fidelity/
#
# Takes about 55 minutes; Service Bus and Event Hubs in the Python suite are the slow part.
# Run it alone: it starts many processes at once and needs a few GB of memory.
# Give it a binary that is not under /tmp when /tmp is memory-backed, for example a copy in ~/.cache/azemu/.
# Not included: tests/terraform/run.py (needs the network) and `cargo test` for the emulator's own crates.
# The Java suite needs Maven (on the PATH, or MVN=/path/to/mvn) and the .NET suite the .NET SDK (on the PATH, or
# DOTNET_ROOT=/path/to/its/directory): see the README of each suite.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$(realpath "${1:-$ROOT/target/debug/azemu}")"
OUT="${OUT:-$(mktemp -d)}"
mkdir -p "$OUT"
SKIP=" ${SKIP:-} "
export AZEMU_BIN="$BIN"
cd "$ROOT"
echo "binary:  $BIN ($(stat -c %s "$BIN") bytes)"
echo "outputs: $OUT"

want() { [[ "$SKIP" != *" $1 "* ]]; }

if want python; then
  .venv/bin/python tests/run_sdk_suites.py --bin "$BIN" --jobs "${JOBS:-4}" > "$OUT/python.txt" 2>&1
  echo "== python (exit $?)"; cat "$OUT/python.txt"
fi
if want replay; then
  # run_all.py writes fidelity/SCORES.md and fidelity/reports, unless SCORES_DIR is set: then it writes
  # $SCORES_DIR/SCORES.md and $SCORES_DIR/reports (CI does that, and compares with the committed scores:
  # tools/release/check_scores.py)
  if [ -n "${SCORES_DIR:-}" ]; then
    mkdir -p "$SCORES_DIR"
    .venv/bin/python tools/replay/run_all.py --start-bin "$BIN" --scores "$SCORES_DIR/SCORES.md" --out-dir "$SCORES_DIR/reports" > "$OUT/replay.txt" 2>&1
  else
    .venv/bin/python tools/replay/run_all.py --start-bin "$BIN" > "$OUT/replay.txt" 2>&1
  fi
  echo "== replay (exit $?)"; grep -A14 "^package" "$OUT/replay.txt" | cut -c1-112
fi
if want rust; then
  (cd tests/sdk-rust && cargo test --no-fail-fast 2>&1; cd beta && cargo test --no-fail-fast 2>&1) > "$OUT/rust.txt"
  echo "== rust"
  awk '/^test result/ {p+=$4; f+=$6; i+=$8} END {print "passed", p, "failed", f, "ignored", i}' "$OUT/rust.txt"
  grep -E "^test .* FAILED" "$OUT/rust.txt"
fi
if want js; then
  (cd tests/sdk-js && npm test) > "$OUT/js.txt" 2>&1
  echo "== js (exit $?)"; tail -8 "$OUT/js.txt"
fi
if want go; then
  (cd tests/sdk-go && go test -count=1 -p 2 ./...) > "$OUT/go.txt" 2>&1
  echo "== go (exit $?)"; grep -E "^(ok|FAIL|---)" "$OUT/go.txt" | cut -c1-100
fi
if want java; then
  (cd tests/sdk-java && ./run.sh) > "$OUT/java.txt" 2>&1
  echo "== java (exit $?)"
  python3 - <<'PY'
import glob, re
t = f = e = s = 0
for p in glob.glob("tests/sdk-java/target/surefire-reports/*.txt"):
    m = re.search(r"Tests run: (\d+), Failures: (\d+), Errors: (\d+), Skipped: (\d+)", open(p).read())
    if m:
        a, b, c, d = map(int, m.groups()); t += a; f += b; e += c; s += d
print("run", t, "failures", f, "errors", e, "skipped", s)
PY
fi
if want dotnet; then
  (cd tests/sdk-dotnet && ./run.sh) > "$OUT/dotnet.txt" 2>&1
  echo "== dotnet (exit $?)"; grep -E "^(Passed!|Failed!)|Total tests|Passed:|Failed:|Skipped:" "$OUT/dotnet.txt" | tail -6
  "${DOTNET_ROOT:+$DOTNET_ROOT/}dotnet" build-server shutdown > /dev/null 2>&1
fi
