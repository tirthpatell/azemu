"""Fixtures for the Key Vault SDK conformance tests (secrets, keys, certificates).

Every client is created through kvutil so the constructor arguments are the same
everywhere (see kvutil.client_kwargs). State is reset before every test by the
autouse fixture in ../conftest.py, but tests still use unique object names.
"""

import pytest
import pytest_asyncio

import kvutil


@pytest.fixture(scope="session")
def azemu_service():
    return "keyvault"


# -- sync clients (default vault) --------------------------------------------
@pytest.fixture
def secrets(emu):
    client = kvutil.make_secret_client(emu)
    yield client
    client.close()


@pytest.fixture
def keys(emu):
    client = kvutil.make_key_client(emu)
    yield client
    client.close()


@pytest.fixture
def certs(emu):
    client = kvutil.make_cert_client(emu)
    yield client
    client.close()


@pytest.fixture
def make_clients(emu):
    """Factory: make_clients("othervault") -> (secret_client, key_client, cert_client)."""
    created = []

    def factory(vault):
        trio = (
            kvutil.make_secret_client(emu, vault),
            kvutil.make_key_client(emu, vault),
            kvutil.make_cert_client(emu, vault),
        )
        created.extend(trio)
        return trio

    yield factory
    for c in created:
        c.close()


@pytest.fixture
def server_crypto(emu):
    """Factory: server_crypto(key_id) -> CryptographyClient that never works locally."""
    created = []

    def factory(key_id):
        client = kvutil.make_server_side_crypto_client(emu, key_id)
        created.append(client)
        return client

    yield factory
    for c in created:
        c.close()


# -- async clients (default vault) -------------------------------------------
@pytest_asyncio.fixture
async def async_secrets(emu):
    client = kvutil.make_async_secret_client(emu)
    yield client
    await client.close()


@pytest_asyncio.fixture
async def async_keys(emu):
    client = kvutil.make_async_key_client(emu)
    yield client
    await client.close()


@pytest_asyncio.fixture
async def async_certs(emu):
    client = kvutil.make_async_cert_client(emu)
    yield client
    await client.close()
