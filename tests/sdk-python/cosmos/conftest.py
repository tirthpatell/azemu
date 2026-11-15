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
from cosmosutil import AZURE, AZURE_ENDPOINT, KEY, client_kwargs, new_client, uid


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


# --------------------------------------------------------------------------- #
# real Azure (AZEMU_AZURE=1): one account, so every database a test creates is removed after it
# --------------------------------------------------------------------------- #
_CREATED_DBS: list[str] = []

if AZURE:
    from azure.cosmos import CosmosClient as _SyncClient
    from azure.cosmos.aio import CosmosClient as _AsyncClient

    from azure.cosmos import DatabaseProxy as _Db

    def _no_throughput(kw):
        # The account is serverless and nothing here may ask for throughput (it would be billed by the hour on any other kind).
        if kw.get("offer_throughput") is not None:
            raise RuntimeError("a throughput request must not be sent to the Azure test account")

    def _guard(orig):
        def wrapper(self, *a, **kw):
            _no_throughput(kw)
            return orig(self, *a, **kw)
        return wrapper

    _Db.create_container = _guard(_Db.create_container)
    _Db.create_container_if_not_exists = _guard(_Db.create_container_if_not_exists)

    def _remember_sync(orig):
        def create_database(self, id, *a, **kw):
            _no_throughput(kw)
            _CREATED_DBS.append(id)
            return orig(self, id, *a, **kw)
        return create_database

    def _remember_async(orig):
        async def create_database(self, id, *a, **kw):
            _no_throughput(kw)
            _CREATED_DBS.append(id)
            return await orig(self, id, *a, **kw)
        return create_database

    # the raw HTTP tests create databases without the SDK
    from cosmosutil import RawCosmos as _Raw

    _raw_request = _Raw.request

    def _raw_tracked(self, verb, path, *, body=None, **kw):
        if isinstance(body, dict) and verb == "POST" and path.strip("/") == "dbs" and isinstance(body.get("id"), str):
            _CREATED_DBS.append(body["id"])
        return _raw_request(self, verb, path, body=body, **kw)

    _Raw.request = _raw_tracked

    _SyncClient.create_database = _remember_sync(_SyncClient.create_database)
    _AsyncClient.create_database = _remember_async(_AsyncClient.create_database)


def _drop_created_databases():
    """Delete the databases this process created (never one it did not create: several runs share the account).

    They are deleted by resource id: a database whose name holds `#` or `?` cannot be addressed by name."""
    names, _CREATED_DBS[:] = set(_CREATED_DBS), []
    if not names:
        return
    from cosmosutil import RawCosmos

    with new_client(AZURE_ENDPOINT, None) as c:
        raw = RawCosmos(AZURE_ENDPOINT, None)
        for d in list(c.list_databases()):
            if d["id"] in names:
                try:
                    # an id-based link is signed as the lower-cased resource id alone
                    raw.request("DELETE", f"dbs/{d['_rid']}", resource_type="dbs", signed_link=d["_rid"].lower())
                except Exception:
                    pass


@pytest.fixture(autouse=True)
def _reset_between_tests(request):
    """Same as the shared fixture of that name, but a no-op for an external endpoint; on Azure it
    removes the databases the test created."""
    if not EXTERNAL and not AZURE:
        request.getfixturevalue("emu").reset()
    yield
    if AZURE:
        _drop_created_databases()
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
    if AZURE:
        return AZURE_ENDPOINT
    if EXTERNAL:
        return EXTERNAL
    emu = request.getfixturevalue("emu")
    return emu.https_url("documents", acct) + "/"


@pytest.fixture
def http_endpoint(request, acct):
    """Plain http endpoint of this test's account."""
    if AZURE:
        pytest.skip("Azure has no plain http endpoint")
    if EXTERNAL:
        return EXTERNAL
    emu = request.getfixturevalue("emu")
    return emu.url("documents", acct) + "/"


@pytest.fixture
def ca_file(request):
    """TLS verification setting for the SDK (`connection_verify`)."""
    if AZURE:
        return None
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
