"""Fixtures for the ARM (management plane) SDK tests.

The `emu` fixture (../conftest.py) starts `target/debug/azemu`, the unified binary: ARM plus the
data plane services it drives, with Blob, Event Grid, Queue, Service Bus and Event Hubs wired together
(events), on one free port. A plain `pytest tests/sdk-python/arm` does that too; build `azemu` first. Management clients talk to
`https://management.localhost:{port}` (bearer tokens need https) and trust the
emulator's CA through `connection_verify`.

Every "exists in the data plane" test creates the resource with the management SDK and
then uses it with the data plane SDK.
"""

import time
import uuid

import pytest
from azure.core.credentials import AccessToken

import autocreate_gate

SUB = "11111111-2222-3333-4444-555555555555"


class FakeCredential:
    """The emulator accepts any bearer token."""

    def get_token(self, *scopes, **kwargs):
        return AccessToken("azemu-test-token", int(time.time()) + 3600)


class FakeAsyncCredential:
    async def get_token(self, *scopes, **kwargs):
        return AccessToken("azemu-test-token", int(time.time()) + 3600)

    async def close(self):
        pass

    async def __aenter__(self):
        return self

    async def __aexit__(self, *exc):
        pass


@pytest.fixture(scope="session")
def azemu_service():
    return "azemu"  # the unified binary, not azemu-<service>


def pytest_configure(config):
    autocreate_gate.add_versioning_markers(config)


def pytest_collection_modifyitems(config, items):
    autocreate_gate.gate_versioning(items)


@pytest.fixture
def arm_url(emu):
    return f"https://management.localhost:{emu.port}"


@pytest.fixture
def mgmt(emu, arm_url):
    """Keyword arguments every management client needs to reach the emulator."""
    return dict(
        base_url=arm_url,
        credential_scopes=["https://management.azure.com/.default"],
        connection_verify=emu.ca_file,
        retry_total=0,
    )


@pytest.fixture
def cred():
    return FakeCredential()


@pytest.fixture
def uniq():
    def make(prefix="t", n=8):
        return f"{prefix}{uuid.uuid4().hex[:n]}"

    return make


@pytest.fixture
def resource_client(cred, mgmt):
    from azure.mgmt.resource.resources import ResourceManagementClient

    with ResourceManagementClient(cred, SUB, **mgmt) as c:
        yield c


@pytest.fixture
def rg(resource_client, uniq):
    """A resource group (created through ARM)."""
    name = uniq("rg-")
    resource_client.resource_groups.create_or_update(name, {"location": "eastus"})
    return name


@pytest.fixture
def make_group(resource_client, uniq):
    def make(prefix="rg-"):
        name = uniq(prefix)
        resource_client.resource_groups.create_or_update(name, {"location": "eastus"})
        return name

    return make


@pytest.fixture
def raw(emu, arm_url):
    """Plain REST calls to ARM, for properties the SDK models do not carry. Returns (status, parsed JSON or None)."""
    import json
    import ssl
    import urllib.error
    import urllib.request

    ctx = ssl.create_default_context(cafile=emu.ca_file)

    def call(method, path, body=None, api="2024-01-01"):
        sep = "&" if "?" in path else "?"
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(
            f"{arm_url}{path}{sep}api-version={api}",
            data=data,
            method=method,
            headers={"Authorization": "Bearer azemu-test-token", "Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(req, context=ctx, timeout=30) as r:
                status, payload = r.status, r.read()
        except urllib.error.HTTPError as e:
            status, payload = e.code, e.read()
        return status, (json.loads(payload) if payload else None)

    return call
