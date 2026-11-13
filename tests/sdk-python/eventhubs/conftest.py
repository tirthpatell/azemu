"""Fixtures for the Event Hubs SDK conformance tests (azure-eventhub 5.15.1).

Every client is made through ehutil (one connection string for plain AMQP, see docs/sdk-notes.md). Hubs are created through the
emulator's admin API with unique names, so no test depends on auto-creation (except the few marked `emulator_only`
whose subject it is). State is reset before every test by the autouse fixture in ../conftest.py.
"""

import pytest
import pytest_asyncio  # noqa: F401  (asyncio_mode=auto comes from ../pytest.ini)

import autocreate_gate
import ehutil


@pytest.fixture(scope="session")
def azemu_service():
    return "eventhubs"


def pytest_configure(config):
    config.addinivalue_line("markers", "slow: needs a wait of several seconds (still under 10 s)")
    config.addinivalue_line(
        "markers", "emulator_only: behaviour of the emulator itself (auto-creation, the admin API), not of real Event Hubs"
    )
    autocreate_gate.add_markers(config)
    config.addinivalue_line("markers", "needs_blob: needs the full emulator binary (target/debug/azemu) with eventhubs and blob")


def pytest_collection_modifyitems(config, items):
    autocreate_gate.gate(items, "AZEMU_EVENTHUBS_AUTOCREATE")


@pytest.fixture
def new_hub(emu):
    """Factory: `new_hub(partitions=4, groups=())` creates a uniquely named hub and returns its name."""

    def make(partitions: int = 4, groups=(), prefix: str = "hub") -> str:
        name = ehutil.uniq(prefix)
        ehutil.admin_create_hub(emu, name, partitions, groups)
        return name

    return make


@pytest.fixture
def hub(new_hub):
    """A hub with 4 partitions ("0".."3") and only the `$Default` consumer group."""
    return new_hub(4)


@pytest.fixture
def hub2(new_hub):
    """A hub with 2 partitions ("0", "1")."""
    return new_hub(2)


@pytest.fixture
def hub1(new_hub):
    """A hub with a single partition ("0")."""
    return new_hub(1)


@pytest.fixture
def track():
    """`track(client)` registers a client so it is closed at the end of the test even when the test fails."""
    clients = []

    def add(client):
        clients.append(client)
        return client

    yield add
    import asyncio
    import inspect

    for c in clients:
        try:
            res = c.close()
            if inspect.isawaitable(res):
                asyncio.new_event_loop().run_until_complete(res)
        except Exception:  # noqa: BLE001
            pass


@pytest.fixture
def producer(emu, hub, track):
    return track(ehutil.make_producer(emu, hub))


@pytest.fixture
def producer1(emu, hub1, track):
    return track(ehutil.make_producer(emu, hub1))


@pytest.fixture
def producer2(emu, hub2, track):
    return track(ehutil.make_producer(emu, hub2))
