"""Fixtures for the identity (token service) SDK tests.

The `emu` fixture (../conftest.py) starts `target/debug/azemu-identity` on a free
port. Every test builds the real azure-identity credential classes by hand and
points them at it with `monkeypatch.setenv` or constructor arguments.

Nothing here may need the internet. Tests that make MSAL call a Microsoft host
(instance discovery) go through `idutil.RoutingTransport`, which refuses every
host that is not the emulator.
"""

import pytest

import idutil


@pytest.fixture(scope="session")
def azemu_service():
    return "identity"


@pytest.fixture(autouse=True)
def clean_identity_env(monkeypatch):
    """Nothing from the developer's shell may change which credential is used."""
    for name in idutil.IDENTITY_ENV:
        monkeypatch.delenv(name, raising=False)
