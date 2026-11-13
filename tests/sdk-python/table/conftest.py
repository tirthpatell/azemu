"""Fixtures for the Table Storage SDK conformance tests.

Everything goes through `emu.storage_conn_str()`, so the same tests can run
against the azemu table service or an external target (Azurite).

Marker:
    not_on_azurite(reason="...")  real Azure behaviour that Azurite lacks or
                                  gets wrong. Skipped when the tests run
                                  against an external target (Azurite).
"""

import uuid

import pytest
from azure.data.tables import TableServiceClient

from tableutil import make_account_sas, parse_conn_str


def pytest_configure(config):
    config.addinivalue_line(
        "markers",
        "not_on_azurite(reason): real Azure behaviour that Azurite lacks or gets wrong; "
        "skipped when emu.storage_is_external is true",
    )


@pytest.fixture(scope="session")
def azemu_service():
    return "table"


@pytest.fixture(autouse=True)
def _skip_not_on_azurite(request):
    marker = request.node.get_closest_marker("not_on_azurite")
    if marker is None:
        return
    emu = request.getfixturevalue("emu")
    if emu.storage_is_external:
        reason = marker.kwargs.get("reason") or (marker.args[0] if marker.args else "")
        pytest.skip(f"not_on_azurite: {reason}")


@pytest.fixture
def conn_str(emu):
    return emu.storage_conn_str()


@pytest.fixture
def conn_parts(conn_str):
    return parse_conn_str(conn_str)


@pytest.fixture
def account_name(conn_parts):
    return conn_parts["AccountName"]


@pytest.fixture
def account_key(conn_parts):
    return conn_parts["AccountKey"]


@pytest.fixture
def service(conn_str):
    with TableServiceClient.from_connection_string(conn_str) as svc:
        yield svc


@pytest.fixture
def uniq():
    """Return a function that makes unique, table-name-safe names (letters + digits)."""

    def make(prefix="t"):
        return f"{prefix}{uuid.uuid4().hex[:12]}"

    return make


@pytest.fixture
def table_name(uniq):
    return uniq("tbl")


@pytest.fixture
def table(service, table_name):
    """A freshly created table (TableClient)."""
    client = service.create_table(table_name)
    yield client
    try:
        service.delete_table(table_name)
    except Exception:  # best effort; state is reset between tests on azemu
        pass


@pytest.fixture
def account_sas(account_name, account_key):
    """Factory for account SAS tokens (see tableutil.make_account_sas)."""

    def make(**kwargs):
        return make_account_sas(account_name, account_key, **kwargs)

    return make


@pytest.fixture
def table_url(service, table_name):
    """Base URL of the account's table endpoint, no trailing slash."""
    return service.url.rstrip("/")
