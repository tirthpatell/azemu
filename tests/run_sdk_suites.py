#!/usr/bin/env python3
"""Run every SDK suite in tests/sdk-python and print one line per suite.

    .venv/bin/python tests/run_sdk_suites.py                 # each suite on its own binary
    .venv/bin/python tests/run_sdk_suites.py --bin target/debug/azemu   # all on the unified binary
    .venv/bin/python tests/run_sdk_suites.py --only blob --only queue
    .venv/bin/python tests/run_sdk_suites.py --jobs 6

Each pytest process starts its own emulator on a free port, so suites run in
parallel. Slow suites (Service Bus waits on receives) are split per file.

The emulator does not create queues, event hubs, consumer groups or Event Grid topics by being named (as Azure does not),
so the first pass runs every suite against the default emulator, with `AZEMU_LENIENT` and the services' `..._AUTOCREATE`
variables removed from the environment. The tests that are about auto-creation itself carry the marker `autocreate`
(see `tests/sdk-python/autocreate_gate.py`); they are skipped in the first pass and run in a second pass of the suites that
have such variables (`AUTOCREATE_VARS`), with the variables set to 1. The numbers of both passes are added up per suite.

Blob versioning (`AZEMU_BLOB_VERSIONING`) is off by default as well. The tests that need an account with versions and
cannot turn it on through ARM carry the marker `versioning` (blob; `arm` has one for the emulator's default); they run in
a second pass with `AZEMU_BLOB_VERSIONING=1` (`SECOND_PASS` below says which suites and which marker).
"""

import argparse
import concurrent.futures
import os
import pathlib
import re
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
SUITES = ROOT / "tests" / "sdk-python"
SPLIT_PER_FILE = {"servicebus", "eventhubs"}
# Suites that need several services in one process: always run on the unified binary.
NEEDS_UNIFIED = {"storage_events", "arm", "kafka", "persistence"}
# The variables that switch auto-creation on for a suite's emulator in the second pass (marker `autocreate`).
AUTOCREATE_VARS = {
    "servicebus": ["AZEMU_SERVICEBUS_AUTOCREATE"],
    "eventhubs": ["AZEMU_EVENTHUBS_AUTOCREATE"],
    "kafka": ["AZEMU_EVENTHUBS_AUTOCREATE"],
    "eventgrid": ["AZEMU_EVENTGRID_AUTOCREATE"],
}
# Suites with a second pass: the variables set to 1 for it, and the marker of the tests that run in it.
SECOND_PASS = {name: (vars_, "autocreate") for name, vars_ in AUTOCREATE_VARS.items()}
SECOND_PASS["blob"] = (["AZEMU_BLOB_VERSIONING"], "versioning")
SECOND_PASS["arm"] = (["AZEMU_BLOB_VERSIONING"], "versioning")
ALL_AUTOCREATE_VARS = {v for vs, _ in SECOND_PASS.values() for v in vs} | {"AZEMU_LENIENT"}


def jobs_for(suite: pathlib.Path):
    if suite.name in SPLIT_PER_FILE:
        return [(suite.name, str(f)) for f in sorted(suite.glob("test_*.py"))]
    return [(suite.name, str(suite))]


def run(job, binary, autocreate=False):
    """One pytest process. `autocreate`: the second pass (only the tests carrying the suite's second-pass marker, the
    suite's variables on)."""
    name, target = job
    env = {k: v for k, v in os.environ.items() if k not in ALL_AUTOCREATE_VARS}
    if autocreate:
        for var in SECOND_PASS[name][0]:
            env[var] = "1"
    if binary:
        env["AZEMU_BIN"] = str(pathlib.Path(binary).resolve())
    elif name in NEEDS_UNIFIED:
        # no binary named: the suite's fixture finds the unified one (target/release/azemu or target/debug/azemu)
        env.pop("AZEMU_BIN", None)
    started = time.time()
    proc = subprocess.run(
        [sys.executable, "-m", "pytest", target, "-q", "-p", "no:cacheprovider"] + (["-m", SECOND_PASS[name][1]] if autocreate else []),
        cwd=ROOT, env=env, capture_output=True, text=True,
    )
    out = proc.stdout
    counts = {}
    last = out.strip().splitlines()[-1] if out.strip() else ""
    for n, word in re.findall(r"(\d+) (passed|failed|skipped|xfailed|xpassed|errors?)", last):
        counts[word.rstrip("s") if word.startswith("error") else word] = int(n)
    failed = [l[len("FAILED "):].split(" - ")[0] for l in out.splitlines() if l.startswith("FAILED ")]
    errored = [l[len("ERROR "):].split(" - ")[0] for l in out.splitlines() if l.startswith("ERROR ")]
    return name, counts, failed + errored, time.time() - started, proc.returncode, last


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", help="emulator binary for every suite (default: each suite's own binary)")
    ap.add_argument("--only", action="append", help="suite name; repeatable")
    ap.add_argument("--jobs", type=int, default=4)
    args = ap.parse_args()

    suites = sorted(p for p in SUITES.iterdir() if p.is_dir() and (p / "conftest.py").exists())
    if args.only:
        suites = [s for s in suites if s.name in args.only]
    jobs = [(j, False) for s in suites for j in jobs_for(s)]
    jobs += [(j, True) for s in suites if s.name in SECOND_PASS for j in jobs_for(s)]

    totals = {}
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for name, counts, bad, secs, rc, last in pool.map(lambda j: run(j[0], args.bin, j[1]), jobs):
            t = totals.setdefault(name, {"counts": {}, "bad": [], "secs": 0.0, "broken": []})
            for k, v in counts.items():
                t["counts"][k] = t["counts"].get(k, 0) + v
            t["bad"] += bad
            t["secs"] += secs
            if rc not in (0, 1, 5) or (not counts and rc != 5):
                t["broken"].append(last or f"pytest exit code {rc}")

    grand = {}
    print(f"{'suite':<12} {'passed':>7} {'failed':>7} {'skipped':>8} {'xfailed':>8} {'error':>6}  seconds")
    for name, t in totals.items():
        c = t["counts"]
        for k, v in c.items():
            grand[k] = grand.get(k, 0) + v
        print(f"{name:<12} {c.get('passed', 0):>7} {c.get('failed', 0):>7} {c.get('skipped', 0):>8} "
              f"{c.get('xfailed', 0):>8} {c.get('error', 0):>6}  {t['secs']:.0f}")
        for b in t["bad"]:
            print(f"    FAILED {b}")
        for b in t["broken"]:
            print(f"    DID NOT RUN CLEANLY: {b}")
    print(f"{'total':<12} {grand.get('passed', 0):>7} {grand.get('failed', 0):>7} {grand.get('skipped', 0):>8} "
          f"{grand.get('xfailed', 0):>8} {grand.get('error', 0):>6}")
    bad = grand.get("failed", 0) + grand.get("error", 0) + sum(len(t["broken"]) for t in totals.values())
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
