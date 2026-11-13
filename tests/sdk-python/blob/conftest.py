"""Fixtures for the Blob Storage SDK conformance tests.

Everything goes through `emu.storage_conn_str()`, so the same tests can run
against the azemu blob service or an external target (Azurite).

Markers:
    not_on_azurite(reason="...")  real Azure behaviour that Azurite lacks or
                                  gets wrong. Skipped when the tests run
                                  against an external target (Azurite).
    versioning                    needs blob versioning on for the account; run
                                  by the runner's second pass with
                                  AZEMU_BLOB_VERSIONING=1.
    no_versioning                 what an account without versioning does (the
                                  default); skipped when versioning is on.
"""

import os
import uuid

import pytest
from azure.storage.blob import BlobServiceClient

import autocreate_gate
from blobutil import parse_conn_str


def pytest_configure(config):
    config.addinivalue_line(
        "markers",
        "not_on_azurite(reason): real Azure behaviour that Azurite lacks or gets wrong; "
        "skipped when emu.storage_is_external is true",
    )
    autocreate_gate.add_versioning_markers(config)


def pytest_collection_modifyitems(config, items):
    autocreate_gate.gate_versioning(items)


@pytest.fixture(scope="session")
def azemu_service():
    return "blob"


@pytest.fixture(autouse=True)
def _skip_not_on_azurite(request):
    marker = request.node.get_closest_marker("not_on_azurite")
    if marker is None:
        return
    emu = request.getfixturevalue("emu")
    # BLOB_RUN_NOT_ON_AZURITE=1 runs them anyway (to see how Azurite fails them).
    if emu.storage_is_external and not os.environ.get("BLOB_RUN_NOT_ON_AZURITE"):
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
    return BlobServiceClient.from_connection_string(conn_str)


@pytest.fixture
def uniq():
    """Return a function that makes unique, container-name-safe names."""

    def make(prefix="t"):
        return f"{prefix}{uuid.uuid4().hex[:12]}"

    return make


@pytest.fixture
def container(service, uniq):
    """A freshly created container with a unique name."""
    return service.create_container(uniq("c"))


@pytest.fixture
def make_container(service, uniq):
    def make(prefix="c", **kwargs):
        return service.create_container(uniq(prefix), **kwargs)

    return make


@pytest.fixture
def blob_name(uniq):
    return uniq("b")
