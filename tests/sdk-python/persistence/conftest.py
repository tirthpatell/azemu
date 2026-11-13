"""Helpers for the persistence suite: start the unified `azemu` on a data directory, stop it, start it again.

These tests manage their own processes (they restart the emulator), so they do not use the `emu` fixture of
the parent conftest. A test file for a service writes:

    def test_things_survive(persistent):
        p = persistent()                 # a fresh data directory, a new emulator process, started
        conn = p.emu.storage_conn_str()  # all URL helpers of the normal `emu` fixture (url(), gateway, ...)
        ... create state with an SDK ...
        p.stop()                         # SIGTERM, waits for the final save; p.stop(kill=True) is SIGKILL
        p.start()                        # same data directory, same port is NOT promised, so build clients after
        ... look at it with an SDK again ...

    p.save()          POST /_azemu/save (what to call before a SIGKILL), returns the JSON answer
    p.health()        GET /_azemu/health as a dict (its `persistence` object has the save results)
    p.start(extra_args=["--save-interval", "1"])   extra command line options for this start
    p.start(expect_failure=True)   returns (exit_code, log text) when the emulator is expected to refuse to start
    p.another()       a second Persistent on the same directory (for the "directory in use" refusal)
    p.data_dir        the directory (a pathlib.Path); p.log_text() the log of the last run

Everything is removed when the test ends. Keep the data small (a few MB): the directory is on /tmp.
"""

import json
import os
import pathlib
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

import pytest

import emubin

# The parent conftest.py has the `Emu` class (URL helpers). Importing it as `conftest` would clash with this file's
# own module name, so it is loaded from its path under another name.
import importlib.util  # noqa: E402

_spec = importlib.util.spec_from_file_location("azemu_root_conftest", pathlib.Path(__file__).resolve().parents[1] / "conftest.py")
_root = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_root)
ROOT, Emu, _free_port, _wait_healthy = _root.ROOT, _root.Emu, _root._free_port, _root._wait_healthy


def _binary() -> str:
    binary = emubin.unified() or str(ROOT / "target" / "debug" / "azemu")
    if not os.path.exists(binary):
        pytest.fail(f"no unified emulator binary at {binary}; run `cargo build -p azemu` first")
    return binary


class Persistent:
    """One data directory and the emulator process that is (or was) running on it."""

    def __init__(self, work: pathlib.Path):
        self.work = work
        self.data_dir = work / "data"
        self.cert_dir = work / "certs"
        self.proc: subprocess.Popen | None = None
        self.emu: Emu | None = None
        self._runs = 0

    # -- life cycle -------------------------------------------------------------
    def start(self, extra_args=(), expect_failure: bool = False, env: dict | None = None, data_dir: bool = True):
        """Start the emulator on the data directory and wait until it answers. `expect_failure`: it must exit
        instead, and (exit code, log text) is returned."""
        assert self.proc is None or self.proc.poll() is not None, "already running; stop() it first"
        port = _free_port()
        self._runs += 1
        self._log_path = self.work / f"run{self._runs}.log"
        log = open(self._log_path, "wb")
        args = [_binary(), "--port", str(port), "--cert-dir", str(self.cert_dir), "--log-level", "debug"]
        if data_dir:
            args += ["--data-dir", str(self.data_dir)]
        args += list(extra_args)
        full_env = dict(os.environ)
        full_env.update(env or {})
        self.proc = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT, env=full_env)
        log.close()
        if expect_failure:
            try:
                code = self.proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                raise AssertionError("emulator was expected to refuse to start but kept running")
            return code, self.log_text()
        _wait_healthy(port, self.proc)
        self.emu = Emu(port, str(self.cert_dir / "ca.pem"), proc=self.proc)
        return self

    def stop(self, kill: bool = False, timeout: float = 30.0) -> int:
        """SIGTERM (the emulator saves, then exits) or, with kill=True, SIGKILL. Returns the exit code."""
        p = self.proc
        if p is None or p.poll() is not None:
            return p.returncode if p else 0
        p.send_signal(signal.SIGKILL if kill else signal.SIGTERM)
        try:
            return p.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait()
            raise AssertionError("emulator did not stop within the time limit")

    def restart(self, kill: bool = False, extra_args=()):
        self.stop(kill=kill)
        return self.start(extra_args=extra_args)

    def another(self) -> "Persistent":
        """A second emulator process for the same data directory, to see what the lock says (use `start`
        with `expect_failure=True`). Do not `close()` it: the directory belongs to the first one."""
        other = Persistent(self.work)
        other._runs = 100 + self._runs
        return other

    # -- admin ------------------------------------------------------------------
    def _admin(self, method: str, path: str):
        req = urllib.request.Request(f"{self.emu.gateway}{path}", method=method, data=b"" if method == "POST" else None)
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read() or b"{}")

    def save(self) -> dict:
        """POST /_azemu/save: everything that changed is on disk when this returns."""
        return self._admin("POST", "/_azemu/save")

    def reset(self, save: bool = False) -> dict:
        return self._admin("POST", "/_azemu/reset" + ("?save=true" if save else ""))

    def health(self) -> dict:
        return self._admin("GET", "/_azemu/health")

    def log_text(self) -> str:
        return self._log_path.read_text(errors="replace") if getattr(self, "_log_path", None) else ""

    def close(self):
        if self.proc is not None and self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()
        shutil.rmtree(self.work, ignore_errors=True)


@pytest.fixture
def persistent():
    """Factory: `persistent()` makes a new data directory and starts the emulator on it (`start=False` only
    prepares it). Everything is stopped and removed when the test ends."""
    made: list[Persistent] = []

    def make(start: bool = True, extra_args=()) -> Persistent:
        work = pathlib.Path(tempfile.mkdtemp(prefix="azemu-persist-"))
        p = Persistent(work)
        made.append(p)
        if start:
            p.start(extra_args=extra_args)
        return p

    yield make
    for p in made:
        p.close()
