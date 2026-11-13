"""Fixtures for the Cosmos DB (NoSQL API) SDK conformance tests.

Accounts are addressed by host name: `https://<account>.documents.localhost:<port>/`.
Every test gets its own account name, and its own database and container names, so
tests cannot see each other's data even before the automatic reset.

Everything here is written against the official `azure-cosmos` Python SDK. The
emulator is expected to behave like the real service as seen through that SDK.
"""

import os

import pytest
from azure.cosmos import CosmosClient, PartitionKey

from cosmosdata import FAMILIES, PRODUCTS
from cosmosutil import KEY, client_kwargs, new_client, uid


@pytest.fixture(scope="session")
def azemu_service():
    return "cosmos"


# --------------------------------------------------------------------------- #
# running against another Cosmos endpoint
# --------------------------------------------------------------------------- #
# AZEMU_COSMOS_ENDPOINT=http://127.0.0.1:18081/   use this endpoint instead of azemu
#                                                 (for example Microsoft's vnext emulator
#                                                 container). Every test then talks to the
#                                                 same single account.
# AZEMU_COSMOS_VERIFY=0                           turn TLS verification off (https endpoints
#                                                 with a self-signed certificate).
#
# Tests that check real Cosmos DB behaviour which the vnext container does not have are
# marked `@pytest.mark.not_on_vnext(reason=...)` and are skipped when the override is set.
EXTERNAL = os.environ.get("AZEMU_COSMOS_ENDPOINT")


def pytest_configure(config):
    config.addinivalue_line(
        "markers",
        "not_on_vnext(reason): real Cosmos DB behaviour that Microsoft's vnext emulator "
        "container lacks; skipped when AZEMU_COSMOS_ENDPOINT is set",
    )


def pytest_collection_modifyitems(config, items):
    if not EXTERNAL:
        return
    for item in items:
        m = item.get_closest_marker("not_on_vnext")
        if m:
            reason = m.kwargs.get("reason") or (m.args[0] if m.args else "")
            item.add_marker(pytest.mark.skip(reason=f"not on vnext: {reason}"))


@pytest.fixture(autouse=True)
def _reset_between_tests(request):
    """Same as the shared fixture of that name, but a no-op for an external endpoint."""
    if not EXTERNAL:
        request.getfixturevalue("emu").reset()
    yield
    if EXTERNAL:
        # the external endpoint is one shared account: remove what this test created
        verify = False if os.environ.get("AZEMU_COSMOS_VERIFY") == "0" else None
        try:
            with new_client(EXTERNAL, verify) as c:
                for d in list(c.list_databases()):
                    c.delete_database(d["id"])
        except Exception:
            pass


# --------------------------------------------------------------------------- #
# account and client
# --------------------------------------------------------------------------- #
@pytest.fixture
def acct():
    """A fresh account name for this test."""
    return "azemu" + uid("")


@pytest.fixture
def endpoint(request, acct):
    """Endpoint of this test's account (TLS against azemu)."""
    if EXTERNAL:
        return EXTERNAL
    emu = request.getfixturevalue("emu")
    return emu.https_url("documents", acct) + "/"


@pytest.fixture
def http_endpoint(request, acct):
    """Plain http endpoint of this test's account."""
    if EXTERNAL:
        return EXTERNAL
    emu = request.getfixturevalue("emu")
    return emu.url("documents", acct) + "/"


@pytest.fixture
def ca_file(request):
    """TLS verification setting for the SDK (`connection_verify`)."""
    if EXTERNAL:
        return False if os.environ.get("AZEMU_COSMOS_VERIFY") == "0" else None
    return request.getfixturevalue("emu").ca_file


@pytest.fixture
def make_client(endpoint, ca_file):
    """Factory for extra clients on this test's account. Closed at teardown."""
    made = []

    def _make(**overrides):
        c = new_client(endpoint, ca_file, **overrides)
        made.append(c)
        return c

    yield _make
    for c in made:
        try:
            c.close()
        except Exception:
            pass


@pytest.fixture
def client(make_client):
    """Sync client with master-key auth. Retries are kept short so a failing
    emulator makes the test fail quickly instead of waiting."""
    return make_client()


# --------------------------------------------------------------------------- #
# database and containers
# --------------------------------------------------------------------------- #
@pytest.fixture
def db_id():
    return uid("db")


@pytest.fixture
def db(client, db_id):
    return client.create_database(db_id)


@pytest.fixture
def make_container(db):
    """Factory: make_container(partition_key="/pk", **create_container_kwargs)."""

    def _make(partition_key="/pk", id=None, **kwargs):
        if isinstance(partition_key, str):
            partition_key = PartitionKey(path=partition_key)
        return db.create_container(id=id or uid("c"), partition_key=partition_key, **kwargs)

    return _make


@pytest.fixture
def container(make_container):
    """An empty container partitioned on /pk."""
    return make_container("/pk")


# --------------------------------------------------------------------------- #
# shared sample data (created per test: the emulator is reset between tests)
# --------------------------------------------------------------------------- #
@pytest.fixture
def products(make_container):
    """Container `/category` holding PRODUCTS (24 documents, 4 categories)."""
    c = make_container("/category")
    for doc in PRODUCTS:
        c.create_item(doc)
    return c


@pytest.fixture
def families(make_container):
    """Container `/lastName` holding FAMILIES (6 documents, 4 last names)."""
    c = make_container("/lastName")
    for doc in FAMILIES:
        c.create_item(doc)
    return c
