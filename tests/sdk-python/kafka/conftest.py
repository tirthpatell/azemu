"""Fixtures for the Kafka suite: the Kafka endpoint of Event Hubs, through librdkafka (the Python package
`confluent-kafka`).

The suite needs the unified binary (`target/debug/azemu`): the cross-protocol tests read with `azure-eventhub` what Kafka
wrote and the other way round, so AMQP and Kafka must share one process. Expectations come from the Kafka and
librdkafka documentation and from Microsoft Learn ("Use Azure Event Hubs from Apache Kafka applications", "Exchange events
between consumers and producers that use different protocols"), not from the emulator's code.
"""

import pytest

import autocreate_gate
import kutil


@pytest.fixture(scope="session")
def azemu_service():
    return "azemu"  # the unified binary, not azemu-<service>


def pytest_configure(config):
    config.addinivalue_line("markers", "tls: needs the emulator's CA file")
    autocreate_gate.add_markers(config)


def pytest_collection_modifyitems(config, items):
    autocreate_gate.gate(items, "AZEMU_EVENTHUBS_AUTOCREATE")


@pytest.fixture(autouse=True)
def _needs_ca(request):
    if "tls" in request.keywords:
        emu = request.getfixturevalue("emu")
        if not emu.ca_file:
            pytest.skip("no CA file: the emulator under test was not started by the fixture (set AZEMU_CA_FILE)")


@pytest.fixture
def topic(emu):
    """A topic (event hub) with 4 partitions in the namespace `default` (what a plain connection reaches)."""
    name = kutil.uniq("topic")
    kutil.admin_create(emu, name, 4)
    return name


@pytest.fixture
def topic1(emu):
    name = kutil.uniq("topic1")
    kutil.admin_create(emu, name, 1)
    return name


@pytest.fixture
def track():
    """`track(client)` registers a producer, consumer or admin client; consumers are closed at the end of the test."""
    items = []

    def add(c):
        items.append(c)
        return c

    yield add
    for c in reversed(items):
        close = getattr(c, "close", None)
        if close:
            try:
                close()
            except Exception:
                pass
