#!/usr/bin/env python3
"""Run the replay for every package whose service the emulator has mounted.

    run_all.py --gateway http://127.0.0.1:4577
    run_all.py --start-bin target/debug/azemu        # starts one on a free port for each package, stops it afterwards

The list of mounted services comes from `GET /_azemu/health`.  For each package that
has one, this writes `fidelity/reports/<name>.json`, prints one table and writes the same
table to `fidelity/SCORES.md` (date, emulator binary, services).  Standard library only.

With `--start-bin` the emulator is started again for every package, with exactly the settings that package needs
(`Package.env`, and why in `tools/replay/README.md`), and without any `AZEMU_*` variable of your shell.  With `--gateway` the
emulator is yours: start it with those settings or the scores of the packages that need them fall.
"""
from __future__ import annotations

import argparse
import datetime
import glob
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from typing import Optional
from urllib.parse import urlsplit

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import replay  # noqa: E402

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SDK = "refs/recordings/python/sdk"


class Package:
    def __init__(self, name: str, service: str, path: str, exclude: tuple = (), env: Optional[dict] = None, why: str = "") -> None:
        self.name, self.service, self.path, self.exclude = name, service, path, exclude
        #: settings the emulator needs for this package's recordings, and the reason (shown in SCORES.md and the README)
        self.env, self.why = dict(env or {}), why


VERSIONING = (
    {"AZEMU_BLOB_VERSIONING": "1"},
    "the recordings come from storage accounts with blob versioning turned on (`x-ms-version-id` answers, versions in listings, "
    "deletes that keep a version); without it new accounts have versioning off, as on Azure",
)


#: report name, service in /_azemu/health, recordings directory (below SDK), `--exclude` substrings
PACKAGES = [
    Package("blob", "blob", "storage/azure-storage-blob", env=VERSIONING[0], why=VERSIONING[1]),
    Package("queue", "queue", "storage/azure-storage-queue"),
    Package("table", "table", "tables/azure-data-tables", ("cosmos",)),
    Package("keyvault-secrets", "keyvault", "keyvault/azure-keyvault-secrets"),
    Package("keyvault-keys", "keyvault", "keyvault/azure-keyvault-keys", ("mhsm",)),
    Package("keyvault-certificates", "keyvault", "keyvault/azure-keyvault-certificates"),
    Package("appconfig", "appconfig", "appconfiguration/azure-appconfiguration"),
    Package("eventgrid", "eventgrid", "eventgrid/azure-eventgrid", env={"AZEMU_EVENTGRID_AUTOCREATE": "1"},
            why="the recorded publishes go to topics that were made outside the recordings (in the Azure portal); without auto-creation they answer 404"),
    Package("servicebus", "servicebus", "servicebus/azure-servicebus"),
    Package("datalake", "dfs", "storage/azure-storage-file-datalake"),
    # the 34 azure-mgmt-* recordings (Resource Manager); the host management.azure.com is the emulator's `management`
    Package("arm", "arm", "*/azure-mgmt-*"),
]


#: How many groups of value differences per package go into SCORES.md (the reports hold all of them).
TOP_VALUE_GROUPS = 6


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def health(gateway: str, timeout: float = 5.0) -> Optional[dict]:
    try:
        with urllib.request.urlopen(gateway.rstrip("/") + "/_azemu/health", timeout=timeout) as r:
            return json.loads(r.read().decode())
    except Exception:  # noqa: BLE001
        return None


def mounted_services(h: dict) -> list[str]:
    svcs = h.get("services", [])
    out = []
    for x in svcs:
        out.append(x if isinstance(x, str) else str(x.get("name", x)) if isinstance(x, dict) else str(x))
    return out


def run_package(pkg: Package, gateway: str, recordings_root: str, out_dir: str, timeout: float) -> dict:
    gw = urlsplit(gateway)
    settings = replay.Settings(
        gateway=gateway, gateway_host=gw.hostname or "127.0.0.1",
        gateway_port=gw.port or (443 if gw.scheme == "https" else 80), force_label=None,
        auth_defaults=replay.parse_auth_overrides(None), timeout=timeout, reset=True, dry_run=False,
        time_shift=True, classify=True, gateway_scheme=gw.scheme or "http")
    root = os.path.join(recordings_root, pkg.path)
    roots = sorted(glob.glob(root)) or [root]
    files = []
    for r in roots:
        # with several directories the name says which one (two packages may have a recording of the same name)
        prefix = (os.path.basename(r) + "/") if len(roots) > 1 else ""
        files += [(prefix + n, p) for n, p in replay.find_recordings([r])]
    files = [f for f in files if not any(x in os.path.basename(f[0]) for x in pkg.exclude)]
    client = replay.Client(gateway, timeout)
    t0 = time.time()
    results = [replay.run_recording(n, p, settings, client) for n, p in files]
    client.close()
    agg = replay.aggregate(results)
    report = replay.build_report(
        agg, results, {"package": pkg.name, "recordings": root, "exclude": list(pkg.exclude), "gateway": gateway,
                       "classify": True}, time.time() - t0)
    os.makedirs(out_dir, exist_ok=True)
    with open(os.path.join(out_dir, f"{pkg.name}.json"), "w", encoding="utf-8") as fh:
        json.dump(report, fh, indent=1, ensure_ascii=False)
    s = agg["summary"]
    classes = s["classes"]
    return {
        "values_compared": s["values_compared"], "values_differ": s["values_differ"],
        "values_differ_pct": s["values_differ_pct"],
        "value_groups": [{"label": g.label, "count": g.count, "recorded": g.example[0], "actual": g.example[1]}
                         for g in agg["value_groups"][:TOP_VALUE_GROUPS]],
        "env": pkg.env, "why": pkg.why,
        "name": pkg.name, "recordings": s["recordings_total"],
        "rec_pass_raw": s["recordings_passing"], "rec_pass_adj": s["recordings_passing_adjusted"],
        "entries_raw_pct": s["entries_pass_raw_pct"], "entries_adj_pct": s["entries_pass_adjusted_pct"],
        "unreplayable": s["entries_unreplayable"], "ahead_async": s["entries_ahead_async"],
        "hard_failing": s["entries_hard_failing"], "classes": classes, "seconds": round(time.time() - t0, 1),
        "errored": s["recordings_errored"],
    }


def start_emulator(binary: str, env: dict) -> tuple:
    """Starts the emulator on a free port with the settings of one package, and with no other `AZEMU_*` variable of the
    caller's shell (`AZEMU_LENIENT` set for a test run must not change a score). -> (process, temp dir, gateway URL)."""
    tmp = tempfile.TemporaryDirectory(prefix="azemu-replay-")
    port = free_port()
    gateway = f"http://127.0.0.1:{port}"
    child_env = {k: v for k, v in os.environ.items() if not k.startswith("AZEMU_")}
    child_env.update(env)
    proc = subprocess.Popen(
        [binary, "--port", str(port), "--cert-dir", tmp.name, "--log-level", "warn"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=child_env)
    for _ in range(120):
        if proc.poll() is not None:
            code = proc.returncode
            tmp.cleanup()
            raise RuntimeError(f"emulator exited with {code}")
        if health(gateway, 1.0) is not None:
            return proc, tmp, gateway
        time.sleep(0.25)
    stop_emulator(proc, tmp)
    raise RuntimeError("emulator did not become healthy in 30s")


def stop_emulator(proc: Optional[subprocess.Popen], tmp: Optional[tempfile.TemporaryDirectory]) -> None:
    if proc is not None:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    if tmp is not None:
        tmp.cleanup()


def format_table(rows: list[dict], markdown: bool) -> str:
    head = ["package", "recordings passing (adjusted / total)", "recordings passing (raw)", "entries raw %",
            "entries adjusted %", "unreplayable", "ahead-async", "hard failures left", "values that differ / compared"]
    body = [[r["name"], f"{r['rec_pass_adj']}/{r['recordings']}", f"{r['rec_pass_raw']}/{r['recordings']}",
             f"{r['entries_raw_pct']:.2f}", f"{r['entries_adj_pct']:.2f}", str(r["unreplayable"]),
             str(r["ahead_async"]), str(r["hard_failing"]),
             f"{r['values_differ']}/{r['values_compared']} ({r['values_differ_pct']:.2f}%)"] for r in rows]
    if markdown:
        out = ["| " + " | ".join(head) + " |", "|" + "|".join(["---"] * len(head)) + "|"]
        out += ["| " + " | ".join(b) + " |" for b in body]
        return "\n".join(out)
    widths = [max(len(h), *(len(b[i]) for b in body)) if body else len(h) for i, h in enumerate(head)]
    line = lambda cells: "  ".join(c.ljust(widths[i]) for i, c in enumerate(cells))  # noqa: E731
    return "\n".join([line(head), line(["-" * w for w in widths])] + [line(b) for b in body])


def _short(v: object, n: int = 70) -> str:
    t = str(v).replace("`", "'").replace("\n", " ")
    return t if len(t) <= n else t[: n - 1] + "…"


def write_scores(path: str, rows: list[dict], binary: str, services: list[str], skipped: list[str]) -> None:
    classes: dict[str, int] = {}
    for r in rows:
        for k, n in r["classes"].items():
            if k not in ("pass", "fail", "skipped"):
                classes[k] = classes.get(k, 0) + n
    lines = [
        "# Fidelity scores",
        "",
        f"- Date: {datetime.date.today().isoformat()}",
        f"- Emulator binary: `{os.path.basename(str(binary))}`",
        f"- Services mounted (from `/_azemu/health`): {', '.join(services) or 'none'}",
        "- Produced by `tools/replay/run_all.py`; reports are in `fidelity/reports/`. "
        "Rules for every class: `tools/replay/README.md`.",
        "",
        "Raw = every entry that is not a pass counts against. Adjusted = unreplayable entries "
        "removed from the denominator and ahead-async counted as passing. "
        "A recording passes (adjusted) when no hard failure is left after classification.",
        "",
        "**Values that differ** is a third number next to the status and error code score: for every answer whose status "
        "matched, the values of the headers, of the body and the first line of error texts that exist on both sides, "
        "except those a rule excuses (ids, times, ETags, sanitized values...; the rules and their reasons are in "
        "`tools/replay/README.md`). It does not change the other columns. Most of what it finds is a property of "
        "Microsoft's test account in the recording (logging settings, access tier), not a mistake of the emulator.",
        "",
        format_table(rows, markdown=True),
        "",
        "Classified entries (all packages above): "
        + (", ".join(f"{k}: {n}" for k, n in sorted(classes.items())) or "none"),
    ]
    lines += ["", "## Settings the emulator was started with, per package", ""]
    for r in rows:
        env = " ".join(f"`{k}={v}`" for k, v in sorted(r["env"].items())) or "none"
        lines.append(f"- {r['name']}: {env}" + (f" ({r['why']})" if r["why"] else ""))
    lines += ["", "## Most frequent value differences, per package", "",
              "Operation, status, then the header or the path of the value. `Azure` is the recorded value, `emulator` ours. "
              "The full lists are in `fidelity/reports/<package>.json` under `value_groups`.", ""]
    for r in rows:
        if not r["value_groups"]:
            lines.append(f"**{r['name']}**: none")
            lines.append("")
            continue
        lines.append(f"**{r['name']}** ({r['values_differ']} of {r['values_compared']} values)")
        lines.append("")
        for g in r["value_groups"]:
            lines.append(f"- {g['count']} x `{g['label']}`: Azure `{_short(g['recorded'])}`, emulator `{_short(g['actual'])}`")
        lines.append("")
    if skipped:
        lines += ["", "Not run (service not mounted): " + ", ".join(skipped)]
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines) + "\n")


def main(argv: Optional[list[str]] = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--gateway", help="address of a running emulator, e.g. http://127.0.0.1:4577")
    g.add_argument("--start-bin", help="emulator binary to start on a free port (stopped afterwards)")
    ap.add_argument("--recordings-root", default=os.path.join(REPO, SDK))
    ap.add_argument("--out-dir", default=os.path.join(REPO, "fidelity", "reports"))
    ap.add_argument("--scores", default=os.path.join(REPO, "fidelity", "SCORES.md"))
    ap.add_argument("--only", action="append", default=[], metavar="NAME", help="run only these report names")
    ap.add_argument("--timeout", type=float, default=30.0)
    args = ap.parse_args(argv)

    binary = os.path.abspath(args.start_bin) if args.start_bin else "(already running; binary unknown)"
    rows, skipped = [], []
    services: list[str] = []
    if args.gateway:
        h = health(args.gateway)
        if h is None:
            print(f"no health answer from {args.gateway}/_azemu/health", file=sys.stderr)
            return 2
        services = mounted_services(h)
    for pkg in PACKAGES:
        if args.only and pkg.name not in args.only:
            continue
        proc: Optional[subprocess.Popen] = None
        tmp: Optional[tempfile.TemporaryDirectory] = None
        try:
            if args.start_bin:
                try:
                    proc, tmp, gateway = start_emulator(binary, pkg.env)
                except RuntimeError as exc:
                    print(str(exc), file=sys.stderr)
                    return 2
                h = health(gateway)
                if h is None:
                    print(f"no health answer from {gateway}/_azemu/health", file=sys.stderr)
                    return 2
                if not services:
                    services = mounted_services(h)
                here = mounted_services(h)
            else:
                gateway, here = args.gateway, services
                if pkg.env:
                    print(f"note: {pkg.name} needs the emulator started with "
                          + " ".join(f"{k}={v}" for k, v in pkg.env.items()) + "; it is yours, so the score depends on that",
                          file=sys.stderr)
            if pkg.service not in here:
                skipped.append(f"{pkg.name} ({pkg.service})")
                continue
            print(f"running {pkg.name} ...", file=sys.stderr, flush=True)
            rows.append(run_package(pkg, gateway, args.recordings_root, args.out_dir, args.timeout))
        finally:
            stop_emulator(proc, tmp)
    print(format_table(rows, markdown=False))
    if skipped:
        print("\nnot run (service not mounted): " + ", ".join(skipped))
    errs = sum(r["errored"] for r in rows)
    if errs:
        print(f"\n{errs} recordings errored (see the reports)")
    write_scores(args.scores, rows, binary, services, skipped)
    print(f"\nreports: {args.out_dir}\nscores:  {args.scores}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
