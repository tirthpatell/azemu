"""Fixtures for the Queue Storage SDK conformance tests.

The tests describe real Azure Queue Storage behaviour as seen through
azure-storage-queue 12.18.1. They run against the emulator by default, or
against Azurite when AZEMU_STORAGE_CONN_STR is set.

A test marked ``not_on_azurite(reason=...)`` asserts real Azure behaviour that
Azurite does not implement. It is skipped when the storage target is external.

Note: this directory is called ``queue``, the same as a standard library
module. Do not add an __init__.py here.
"""

import pytest
from azure.storage.queue import QueueServiceClient

from queue_support import unique_queue_name


def pytest_configure(config):
    config.addinivalue_line(
        "markers",
        "not_on_azurite(reason): test asserts real Azure behaviour that Azurite "
        "does not implement; skipped when running against Azurite",
    )


@pytest.fixture(scope="session")
def azemu_service():
    return "queue"


@pytest.fixture(autouse=True)
def _skip_known_azurite_gaps(request):
    marker = request.node.get_closest_marker("not_on_azurite")
    if marker is None:
        return
    reason = marker.kwargs.get("reason")
    assert reason, "not_on_azurite needs reason=..."
    emu = request.getfixturevalue("emu")
    if emu.storage_is_external:
        pytest.skip(f"Azurite gap: {reason}")


@pytest.fixture
def service(emu):
    """Queue service client for the target under test."""
    return QueueServiceClient.from_connection_string(emu.storage_conn_str())


@pytest.fixture
def queue_name():
    return unique_queue_name()


@pytest.fixture
def queue(service, queue_name):
    """A freshly created queue with a unique name."""
    return service.create_queue(queue_name)
