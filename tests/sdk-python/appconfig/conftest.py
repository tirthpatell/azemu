"""Fixtures for the App Configuration SDK conformance tests.

Stores are addressed by host name: `http://<store>.azconfig.localhost:<port>`.
Every test gets its own store name, so tests cannot see each other's settings
even before the automatic reset.
"""

import uuid

import pytest
from azure.appconfiguration import AzureAppConfigurationClient

from appconfigutil import Raw, FakeCredential, conn_string, uid


@pytest.fixture(scope="session")
def azemu_service():
    return "appconfig"


@pytest.fixture
def store():
    return f"azemu{uuid.uuid4().hex[:12]}"


@pytest.fixture
def endpoint(emu, store):
    """http endpoint of this test's store."""
    return emu.url("azconfig", store)


@pytest.fixture
def https_endpoint(emu, store):
    return emu.https_url("azconfig", store)


@pytest.fixture
def other_endpoint(emu):
    """http endpoint of a second, different store."""
    return emu.url("azconfig", f"azemu{uuid.uuid4().hex[:12]}")


@pytest.fixture
def uniq():
    return uid


@pytest.fixture
def client(endpoint):
    """Sync client, HMAC (connection string) auth over plain http.

    The SDK's HMAC signing policy does not require https (only the bearer-token
    policy does), so this is the simplest way to talk to the emulator.
    retry_total=0 keeps a misbehaving emulator from making tests slow.
    """
    with AzureAppConfigurationClient.from_connection_string(conn_string(endpoint), retry_total=0) as c:
        yield c


@pytest.fixture
def other_client(other_endpoint):
    with AzureAppConfigurationClient.from_connection_string(conn_string(other_endpoint), retry_total=0) as c:
        yield c


@pytest.fixture
def entra_client(emu, https_endpoint):
    """Sync client with a (fake) Entra ID token credential over https."""
    with AzureAppConfigurationClient(
        https_endpoint, credential=FakeCredential(), connection_verify=emu.ca_file, retry_total=0
    ) as c:
        yield c


@pytest.fixture
def raw(emu, https_endpoint):
    """Plain `requests` access (bearer header, https, trusts the emulator CA)."""
    return Raw(https_endpoint, emu.ca_file)


@pytest.fixture
def raw_http(endpoint):
    """Same as `raw` but over plain http."""
    return Raw(endpoint, None)
