"""Fixtures for the strict authorisation suite (`docs/strict-auth.md`).

The same tests run on both sides: against the emulator started with `--strict-auth` (the `emu` fixture of `../conftest.py`, with
`AZEMU_STRICT_AUTH=1` set for this suite's emulator), and against real Azure with `AZEMU_AZURE=1`, with the four principals of the
environment (`AZURE_*`, `AZ_READER_*`, `AZ_NOROLE_*`, `AZ_SCOPED_*`). On the emulator the fixture makes the accounts through Resource
Manager and gives the four principals the roles the Azure side has; on Azure it creates nothing on the management plane.
"""

import os

import pytest

import authutil

# The emulator that `emu` starts inherits this environment: strict mode on. (`tests/run_sdk_suites.py` sets it too.)
if os.environ.get("AZEMU_AZURE") != "1":
    os.environ["AZEMU_STRICT_AUTH"] = "1"


@pytest.fixture(scope="session")
def azemu_service():
    return "azemu"  # the unified binary: Resource Manager and the data planes it makes accounts in


@pytest.fixture(autouse=True)
def _reset_between_tests():
    """The shared reset of `../conftest.py` would drop the world this suite builds once per session."""
    yield


@pytest.fixture(scope="session")
def world(emu):
    w = authutil.build_world(emu)
    yield w
    w.close()


@pytest.fixture(scope="session")
def main(world):
    return world.principals["main"]


@pytest.fixture(scope="session")
def reader(world):
    return world.principals["reader"]


@pytest.fixture(scope="session")
def norole(world):
    return world.principals["norole"]


@pytest.fixture(scope="session")
def scoped(world):
    return world.principals["scoped"]
