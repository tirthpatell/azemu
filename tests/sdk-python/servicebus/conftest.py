"""Fixtures for the Service Bus SDK conformance tests (azure-servicebus 7.15.0).

Every client is made through sbutil (one connection string for AMQP and HTTP, see
docs/sdk-notes.md). Entities are created through the admin client with unique names, so no test
depends on auto-creation (except the few marked `autocreate`, which run in the runner's second pass
with AZEMU_SERVICEBUS_AUTOCREATE=1; see ../autocreate_gate.py). State is reset before
every test by the autouse fixture in ../conftest.py.
"""

import os

import pytest
import pytest_asyncio

import autocreate_gate
import sbutil


@pytest.fixture(scope="session")
def azemu_service():
    return "servicebus"


def pytest_configure(config):
    config.addinivalue_line("markers", "slow: needs a lock/TTL/schedule wait of several seconds (still under 10 s)")
    config.addinivalue_line(
        "markers", "emulator_only: behaviour of the emulator itself (not of real Service Bus), e.g. auto-creation"
    )
    autocreate_gate.add_markers(config)


def pytest_collection_modifyitems(config, items):
    autocreate_gate.gate(items, "AZEMU_SERVICEBUS_AUTOCREATE")


# -- real Azure (AZEMU_AZURE=1) -----------------------------------------------------------------------
def sweep_namespace(emu) -> int:
    """Delete every queue and topic (with its subscriptions and rules) of the real namespace. The namespace
    belongs to this suite alone, so the tests leave it empty. Returns how many entities were deleted.
    Raw requests, not the administration client: after a topic was updated with the Python SDK, Azure lists it in a
    form the SDK can not parse."""
    n = 0
    for kind in ("queues", "topics"):
        for _ in range(20):
            names = sbutil.azure_entity_names(kind)
            if not names:
                break
            for name in names:
                status, body = sbutil.azure_http("DELETE", sbutil.quote(name, safe="/"))
                if status not in (200, 404):
                    raise RuntimeError(f"deleting {kind[:-1]} answered {status}: {body[:200]}")
                n += 1
    return n


@pytest.fixture(autouse=True)
def _azure_clean_namespace(request):
    """Against Azure nothing resets between tests: every test ends by deleting what is in the namespace, so the
    next one starts on an empty namespace, as it does on a freshly reset emulator."""
    azure_emu = request.getfixturevalue("emu") if os.environ.get("AZEMU_AZURE") == "1" else None
    yield
    if azure_emu is not None:
        sweep_namespace(azure_emu)


# -- connection ---------------------------------------------------------------
@pytest.fixture
def conn_str(emu):
    return sbutil.conn_str(emu)


@pytest.fixture
def make_client(emu):
    """Factory: make_client(namespace="default", **kwargs) -> ServiceBusClient (closed at teardown)."""
    made = []

    def factory(namespace="default", **kwargs):
        c = sbutil.make_client(emu, namespace, **kwargs)
        made.append(c)
        return c

    yield factory
    for c in made:
        try:
            c.close()
        except Exception:  # noqa: BLE001
            pass


@pytest.fixture
def sb(make_client):
    """The ServiceBusClient for namespace `default`."""
    return make_client()


@pytest.fixture
def admin(emu):
    client = sbutil.make_admin(emu)
    yield client
    client.close()


# -- entities -----------------------------------------------------------------
@pytest.fixture
def new_queue(admin):
    """Factory: new_queue(**create_queue kwargs) -> queue name (unique, created through the admin client)."""

    def factory(prefix="q", **kwargs):
        name = sbutil.uniq(prefix)
        admin.create_queue(name, **kwargs)
        return name

    return factory


@pytest.fixture
def new_topic(admin):
    def factory(prefix="t", **kwargs):
        name = sbutil.uniq(prefix)
        admin.create_topic(name, **kwargs)
        return name

    return factory


@pytest.fixture
def new_sub(admin):
    """Factory: new_sub(topic, **create_subscription kwargs) -> subscription name. Removes `$Default`
    when remove_default=True is passed (so the subscription starts with no rule)."""

    def factory(topic, prefix="s", remove_default=False, **kwargs):
        name = sbutil.uniq(prefix)
        admin.create_subscription(topic, name, **kwargs)
        if remove_default:
            admin.delete_rule(topic, name, "$Default")
        return name

    return factory


@pytest.fixture
def queue(new_queue):
    return new_queue()


@pytest.fixture
def session_queue(new_queue):
    return new_queue("sq", requires_session=True)


@pytest.fixture
def topic(new_topic):
    return new_topic()


@pytest.fixture
def topic_sub(topic, new_sub):
    """(topic name, subscription name) with the default rule."""
    return topic, new_sub(topic)


# -- async --------------------------------------------------------------------
@pytest_asyncio.fixture
async def async_sb(emu):
    client = sbutil.make_async_client(emu)
    yield client
    await client.close()


@pytest_asyncio.fixture
async def async_admin(emu):
    client = sbutil.make_async_admin(emu)
    yield client
    await client.close()
