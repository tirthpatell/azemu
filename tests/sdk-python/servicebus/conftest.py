"""Fixtures for the Service Bus SDK conformance tests (azure-servicebus 7.15.0).

Every client is made through sbutil (one connection string for AMQP and HTTP, see
docs/sdk-notes.md). Entities are created through the admin client with unique names, so no test
depends on auto-creation (except the few marked `autocreate`, which run in the runner's second pass
with AZEMU_SERVICEBUS_AUTOCREATE=1; see ../autocreate_gate.py). State is reset before
every test by the autouse fixture in ../conftest.py.
"""

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
