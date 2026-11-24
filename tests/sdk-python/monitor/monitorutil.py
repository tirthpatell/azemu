"""Helpers shared by the monitor SDK tests."""

import json
import os
import pathlib
import re
import time
import urllib.request

IKEY = "11111111-2222-3333-4444-555555555555"

# `INFO request service=monitor account= method=POST path=//v2.1/track status=200 ms=1`
_REQUEST_LINE = re.compile(r"request service=monitor account=\S* method=(\S+) path=(\S+) status=(\d+)")


def conn_string(emu, ikey: str = IKEY, host: str = "127.0.0.1") -> str:
    """Instrumentation key and the two endpoints, as in the documentation (`IngestionEndpoint=http://azemu:10015/`)."""
    base = f"http://{host}:{emu.port}/"
    return f"InstrumentationKey={ikey};IngestionEndpoint={base};LiveEndpoint={base}"


def log_path(emu):
    """The emulator's log file, when the fixture started the emulator (it keeps the log next to the certificates)."""
    if emu.proc is None or not emu.ca_file:
        return None
    return pathlib.Path(emu.ca_file).parent / "azemu.log"


def requests_since(log: pathlib.Path, offset: int):
    """(method, path, status) of every monitor request logged after `offset` bytes."""
    with open(log, "rb") as f:
        f.seek(offset)
        text = f.read().decode("utf-8", "replace")
    return [(m.group(1), m.group(2), int(m.group(3))) for m in _REQUEST_LINE.finditer(text)]


def refused_since(log: pathlib.Path, offset: int):
    return [f"{m} {p} -> {s}" for m, p, s in requests_since(log, offset) if s >= 400]


# -- the admin API -------------------------------------------------------------
def admin(emu, path: str, method: str = "GET"):
    req = urllib.request.Request(f"{emu.gateway}/_azemu/monitor{path}", method=method)
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read() or b"null")


def telemetry(emu, **query) -> list:
    q = "&".join(f"{k}={v}" for k, v in query.items())
    return admin(emu, "/telemetry" + (f"?{q}" if q else ""))["items"]


def logs(emu, **query) -> list:
    q = "&".join(f"{k}={v}" for k, v in query.items())
    return admin(emu, "/logs" + (f"?{q}" if q else ""))["items"]


def wait_for(fn, what: str, timeout: float = 15.0, step: float = 0.1):
    """Call `fn` until it returns something truthy; fail with `what` after `timeout` seconds."""
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = fn()
        if last:
            return last
        time.sleep(step)
    raise AssertionError(f"timed out waiting for {what}; last value: {last!r}")
