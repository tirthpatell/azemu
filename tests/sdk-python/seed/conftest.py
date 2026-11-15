"""Start the unified `azemu` with a seed file, for the tests of this directory.

These tests start their own emulator (the seed file is an option of the program), so they do not use the `emu` fixture of
the parent conftest. A test writes:

    def test_things(seeded):
        s = seeded({...the seed file as a dict...})   # a seed file in a new directory, an emulator started with it
        s.emu          the URL helpers of the normal `emu` fixture
        s.reset(query) POST /_azemu/reset<query>, returns the JSON answer
        s.config()     GET /_azemu/config as a dict

    seeded(text, expect_failure=True)   a seed file with this text; returns (exit code, log text) when the program refuses to start
"""

import importlib.util
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import urllib.request

import pytest

import emubin

# The parent conftest.py has the `Emu` class (URL helpers); it is loaded from its path, as the persistence suite does.
_spec = importlib.util.spec_from_file_location("azemu_root_conftest", pathlib.Path(__file__).resolve().parents[1] / "conftest.py")
_root = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_root)
ROOT, Emu, _free_port, _wait_healthy = _root.ROOT, _root.Emu, _root._free_port, _root._wait_healthy


class Seeded:
    def __init__(self, work: pathlib.Path):
        self.work = work
        self.seed_file = work / "seed.json"
        self.proc = None
        self.emu = None
        self.log_path = work / "azemu.log"

    def start(self, text: str, expect_failure: bool = False):
        self.seed_file.write_text(text)
        binary = emubin.unified() or str(ROOT / "target" / "debug" / "azemu")
        if not os.path.exists(binary):
            pytest.fail(f"no unified emulator binary at {binary}; run `cargo build -p azemu` first")
        port = _free_port()
        # a relative path, as a user gives it: the program is started in the directory that holds the file
        args = [binary, "--port", str(port), "--cert-dir", str(self.work / "certs"), "--seed", self.seed_file.name]
        with open(self.log_path, "wb") as log:
            self.proc = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT, cwd=self.work)
        if expect_failure:
            code = self.proc.wait(timeout=30)
            return code, self.log_path.read_text(errors="replace")
        _wait_healthy(port, self.proc)
        self.emu = Emu(port, str(self.work / "certs" / "ca.pem"), proc=self.proc)
        return self

    def reset(self, query: str = "") -> dict:
        req = urllib.request.Request(f"{self.emu.gateway}/_azemu/reset{query}", method="POST", data=b"")
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read() or b"{}")

    def config(self) -> dict:
        with urllib.request.urlopen(f"{self.emu.gateway}/_azemu/config", timeout=10) as r:
            return json.loads(r.read())

    def log_text(self) -> str:
        return self.log_path.read_text(errors="replace")

    def close(self):
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        shutil.rmtree(self.work, ignore_errors=True)


@pytest.fixture
def seeded():
    """Factory: `seeded(content)` writes the seed file (a dict, or the text of the file) and starts the emulator with it."""
    made = []

    def make(content, expect_failure: bool = False):
        s = Seeded(pathlib.Path(tempfile.mkdtemp(prefix="azemu-seed-")))
        made.append(s)
        text = content if isinstance(content, str) else json.dumps(content)
        return s.start(text, expect_failure=expect_failure)

    yield make
    for s in made:
        s.close()
