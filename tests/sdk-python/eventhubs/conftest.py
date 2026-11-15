"""Fixtures for the Event Hubs SDK conformance tests (azure-eventhub 5.15.1).

Every client is made through ehutil (one connection string for plain AMQP, see docs/sdk-notes.md). Hubs are created through the
emulator's admin API with unique names, so no test depends on auto-creation (except the few marked `emulator_only`
whose subject it is). State is reset before every test by the autouse fixture in ../conftest.py.

With AZEMU_AZURE=1 the same tests run against a real namespace (azure_eh.py, tests/sdk-python/README.md): nothing is reset, hubs come from
Resource Manager and are shared (`new_hub` hands out the one hub with the asked partition count and `ehutil.record_baseline` notes where
each partition ends when the test starts).
"""

import os

import pytest
import pytest_asyncio  # noqa: F401  (asyncio_mode=auto comes from ../pytest.ini)

import autocreate_gate
import azure_eh
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


@pytest.fixture(scope="session", autouse=True)
def _azure_hubs_are_removed_at_the_end(emu):
    """Against Azure the shared hubs are deleted when the session ends (AZEMU_AZURE_KEEP_HUBS=1 keeps them for the next run)."""
    yield
    if emu.azure and os.environ.get("AZEMU_AZURE_KEEP_HUBS") != "1":
        azure_eh.delete_all_hubs()


# Partition counts that most tests use: on Azure they get one shared hub for the whole session. Any other count gets a hub of its own
# that is deleted when its test ends (a namespace holds at most 10 hubs).
SHARED_COUNTS = (1, 2, 4)


@pytest.fixture
def new_hub(emu, request):
    """Factory: `new_hub(partitions=4, groups=())` creates a uniquely named hub and returns its name."""

    def make(partitions: int = 4, groups=(), prefix: str = "hub") -> str:
        if emu.azure and partitions in SHARED_COUNTS:  # see ehutil.pool_hub
            return ehutil.pool_hub(emu, partitions, groups)
        if emu.azure:
            name = ehutil.uniq("azemu-" + prefix)
            azure_eh.ensure_hub(partitions, name=name)
            request.addfinalizer(lambda: azure_eh.delete_hub(name))
            for g in groups:
                azure_eh.ensure_group(name, g)
            return name
        name = ehutil.uniq(prefix)
        ehutil.admin_create_hub(emu, name, partitions, groups)
        return name

    return make


@pytest.fixture
def fresh_hub(emu, new_hub):
    """A hub with 4 partitions that no test ever sent to: `is_empty` is true everywhere. On Azure it is one hub, kept empty."""
    if emu.azure:
        return azure_eh.ensure_hub(4, name="azemu-pool-empty")
    return new_hub(4)


@pytest.fixture
def solo_hub(emu, request, new_hub):
    """A hub with one partition that only this test has used, for tests that read a time range or "everything" (the shared hubs
    of an Azure run hold the events of earlier tests). On Azure it is made for the test and deleted when the test ends."""
    if emu.azure:
        name = ehutil.uniq("azemu-solo")
        azure_eh.ensure_hub(1, name=name)
        request.addfinalizer(lambda: azure_eh.delete_hub(name))
        return name
    return new_hub(1)


@pytest.fixture
def solo_hub2(emu, request, new_hub):
    """Like `solo_hub` with two partitions."""
    if emu.azure:
        name = ehutil.uniq("azemu-solo2")
        azure_eh.ensure_hub(2, name=name)
        request.addfinalizer(lambda: azure_eh.delete_hub(name))
        return name
    return new_hub(2)


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
