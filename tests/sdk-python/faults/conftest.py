"""Fault injection (`docs/faults.md`) seen through the SDK clients: they retry what the emulator throttles, wait as long as the
retry header says where the SDK honours it, and raise the right error when retries are off.

These tests need several services in one emulator (the unified binary). A test adds its rules through the `faults` fixture;
the full reset the runner does between tests removes them again. The helpers of the Cosmos, Key Vault, App Configuration and
Service Bus suites are reused: their directories are put on the import path here.
"""

import pathlib
import sys

import pytest

HERE = pathlib.Path(__file__).resolve().parent
for sibling in ("cosmos", "keyvault", "appconfig", "servicebus"):
    sys.path.insert(0, str(HERE.parent / sibling))

import faultutil  # noqa: E402


def pytest_collection_modifyitems(config, items):
    # fault rules are a feature of the emulator: there is nothing to run against Azure
    for item in items:
        if "/faults/" in str(item.fspath).replace("\\", "/"):
            item.add_marker(pytest.mark.emulator_only)


@pytest.fixture(scope="session")
def azemu_service():
    return "azemu"


@pytest.fixture
def faults(emu):
    return faultutil.Faults(emu)
