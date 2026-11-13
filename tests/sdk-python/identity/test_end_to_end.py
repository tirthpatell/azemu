"""The point of the service: an Azure SDK client whose credential got its token
from the emulator.

The identity binary has no storage service and the full `azemu` binary does not
mount identity (yet), so this runs two processes: the credential talks to the
identity emulator (the `emu` fixture), the blob client to a full `azemu` that the
test starts on its own port. Skipped when `target/debug/azemu` is not built.
"""

import os
import pathlib
import socket
import subprocess
import time
import urllib.request
import uuid

import pytest

import emubin
from azure.core.credentials import AccessToken
from azure.identity import ClientSecretCredential, DefaultAzureCredential, ManagedIdentityCredential
from azure.storage.blob import BlobServiceClient

import idutil
from idutil import CLIENT_ID, TENANT, decode

ROOT = pathlib.Path(__file__).resolve().parents[3]
FULL_BINARY = pathlib.Path(emubin.unified() or ROOT / "target" / "debug" / "azemu")


def _free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture(scope="module")
def full_emulator(tmp_path_factory):
    """(port, ca_file) of a full azemu started for this module."""
    if not FULL_BINARY.exists():
        pytest.fail(emubin.missing_message())
    cert_dir = tmp_path_factory.mktemp("full-azemu-certs")
    port = _free_port()
    log = open(cert_dir / "azemu.log", "wb")
    proc = subprocess.Popen(
        [str(FULL_BINARY), "--port", str(port), "--cert-dir", str(cert_dir)], stdout=log, stderr=subprocess.STDOUT
    )
    try:
        deadline = time.time() + 30
        while True:
            if proc.poll() is not None:
                pytest.skip(f"full azemu exited early with {proc.returncode}")
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/_azemu/health", timeout=1) as r:
                    if r.status == 200:
                        break
            except Exception:
                pass
            if time.time() > deadline:
                pytest.skip("full azemu did not become healthy")
            time.sleep(0.1)
        yield port, str(cert_dir / "ca.pem")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        log.close()


class Recording:
    """Wraps a credential and keeps the tokens the SDK client asked for."""

    def __init__(self, inner):
        self.inner = inner
        self.tokens = []

    def get_token(self, *scopes, **kwargs):
        tok = self.inner.get_token(*scopes, **kwargs)
        self.tokens.append((scopes, tok))
        return AccessToken(tok.token, tok.expires_on)


def blob_client(full, credential):
    port, ca = full
    return BlobServiceClient(f"https://e2eacct.blob.core.localhost:{port}", credential=credential, connection_verify=ca)


def _container_roundtrip(client):
    name = "c" + uuid.uuid4().hex[:12]
    container = client.create_container(name)
    try:
        assert name in [c.name for c in client.list_containers()]
        blob = container.get_blob_client("hello.txt")
        blob.upload_blob(b"hello from a token the emulator issued")
        assert blob.download_blob().readall() == b"hello from a token the emulator issued"
    finally:
        client.delete_container(name)


def test_blob_with_managed_identity_credential(emu, full_emulator, monkeypatch):
    monkeypatch.setenv("IDENTITY_ENDPOINT", idutil.app_service_endpoint(emu))
    monkeypatch.setenv("IDENTITY_HEADER", "azemu")
    cred = Recording(ManagedIdentityCredential())
    _container_roundtrip(blob_client(full_emulator, cred))
    scopes, tok = cred.tokens[0]
    assert scopes == ("https://storage.azure.com/.default",)
    claims = decode(emu, tok.token, "https://storage.azure.com")
    assert claims["idtyp"] == "app"


def test_blob_with_default_azure_credential_and_imds(emu, full_emulator, monkeypatch):
    monkeypatch.setenv("AZURE_POD_IDENTITY_AUTHORITY_HOST", idutil.imds_authority(emu))
    cred = Recording(DefaultAzureCredential())
    _container_roundtrip(blob_client(full_emulator, cred))
    decode(emu, cred.tokens[0][1].token, "https://storage.azure.com")


def test_blob_with_client_secret_credential(emu, full_emulator):
    cred = Recording(
        ClientSecretCredential(
            TENANT, CLIENT_ID, "s", authority=idutil.host_authority(emu), disable_instance_discovery=True,
            connection_verify=emu.ca_file,
        )
    )
    _container_roundtrip(blob_client(full_emulator, cred))
    claims = decode(emu, cred.tokens[0][1].token, "https://storage.azure.com")
    assert claims["appid"] == CLIENT_ID
