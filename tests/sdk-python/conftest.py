"""Shared fixtures for the SDK tests.

Each service directory has its own conftest.py that defines one fixture:

    @pytest.fixture(scope="session")
    def azemu_service():
        return "blob"          # which azemu-<name> binary to start

Tests then use the `emu` fixture from this file.

Environment overrides:
    AZEMU_GATEWAY            e.g. http://127.0.0.1:4577 - use an emulator that is
                             already running instead of starting one.
    AZEMU_BIN                path to the binary to start.
    AZEMU_STORAGE_CONN_STR   use this storage connection string as is (for
                             running the storage tests against Azurite).
"""

import os
import pathlib
import socket
import subprocess
import tempfile
import time
import urllib.request

import pytest

import emubin

ROOT = pathlib.Path(__file__).resolve().parents[2]

DEV_ACCOUNT = "devstoreaccount1"
DEV_KEY = (
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/"
    "K1SZFPTOtr/KBHBeksoGMGw=="
)


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Emu:
    """A running emulator and the URLs to reach it."""

    def __init__(self, port: int, ca_file: str | None, proc=None, external=False):
        self.port = port
        self.ca_file = ca_file
        self.proc = proc
        self.external = external

    # -- URLs ---------------------------------------------------------------
    def url(self, label: str, account: str, scheme: str = "http") -> str:
        """Host-style URL: {scheme}://{account}.{label}.localhost:{port}

        Storage labels get `.core.localhost`: the Python storage SDK treats any
        host containing "localhost" as Azurite path style unless it also
        contains ".core." (see `_from_blob_url`), and azemu ignores everything
        after the service label anyway.
        """
        suffix = "core.localhost" if label in ("blob", "queue", "table", "dfs") else "localhost"
        return f"{scheme}://{account}.{label}.{suffix}:{self.port}"

    def https_url(self, label: str, account: str) -> str:
        return self.url(label, account, "https")

    @property
    def gateway(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    # -- storage ------------------------------------------------------------
    @property
    def storage_is_external(self) -> bool:
        """True when the storage tests run against something else (Azurite)."""
        return bool(os.environ.get("AZEMU_STORAGE_CONN_STR"))

    def storage_conn_str(self, account: str = DEV_ACCOUNT, key: str = DEV_KEY) -> str:
        override = os.environ.get("AZEMU_STORAGE_CONN_STR")
        if override:
            return override
        return (
            "DefaultEndpointsProtocol=http;"
            f"AccountName={account};AccountKey={key};"
            f"BlobEndpoint={self.url('blob', account)};"
            f"QueueEndpoint={self.url('queue', account)};"
            f"TableEndpoint={self.url('table', account)};"
        )

    # -- admin --------------------------------------------------------------
    def reset(self) -> None:
        """Drop all emulator state. No-op against Azurite."""
        if self.storage_is_external:
            return
        req = urllib.request.Request(f"{self.gateway}/_azemu/reset", method="POST", data=b"")
        urllib.request.urlopen(req, timeout=10).read()


def _wait_healthy(port: int, proc, timeout: float = 20.0) -> None:
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        if proc is not None and proc.poll() is not None:
            raise RuntimeError(f"emulator exited early with code {proc.returncode}")
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/_azemu/health", timeout=1) as r:
                if r.status == 200:
                    return
        except Exception as e:  # not up yet
            last = e
        time.sleep(0.1)
    raise RuntimeError(f"emulator did not become healthy on port {port}: {last}")


@pytest.fixture(scope="session")
def emu(azemu_service):
    if os.environ.get("AZEMU_STORAGE_CONN_STR"):
        yield Emu(0, None, external=True)
        return

    gateway = os.environ.get("AZEMU_GATEWAY")
    if gateway:
        port = int(gateway.rsplit(":", 1)[1].rstrip("/"))
        yield Emu(port, os.environ.get("AZEMU_CA_FILE"), external=True)
        return

    # "azemu" is what the suites that need every service in one program ask for
    binary = emubin.unified() if azemu_service == "azemu" else emubin.for_service(azemu_service)
    if not binary:
        pytest.fail(emubin.missing_message())

    port = _free_port()
    with tempfile.TemporaryDirectory(prefix="azemu-certs-") as cert_dir:
        log = open(os.path.join(cert_dir, "azemu.log"), "wb")
        proc = subprocess.Popen(
            [binary, "--port", str(port), "--cert-dir", cert_dir, "--log-level", "debug"],
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        try:
            _wait_healthy(port, proc)
            yield Emu(port, os.path.join(cert_dir, "ca.pem"), proc=proc)
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
            log.close()
            keep = os.environ.get("AZEMU_KEEP_LOG")
            if keep:
                pathlib.Path(keep).write_bytes(
                    pathlib.Path(cert_dir, "azemu.log").read_bytes()
                )


@pytest.fixture(autouse=True)
def _reset_between_tests(request):
    """Fresh state for every test that uses the emulator."""
    if "emu" in request.fixturenames:
        request.getfixturevalue("emu").reset()
    yield
