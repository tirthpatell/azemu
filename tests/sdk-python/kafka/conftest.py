"""Fixtures for the Kafka suite: the Kafka endpoint of Event Hubs, through librdkafka (the Python package
`confluent-kafka`).

The suite needs the unified binary (`target/debug/azemu`): the cross-protocol tests read with `azure-eventhub` what Kafka
wrote and the other way round, so AMQP and Kafka must share one process. Expectations come from the Kafka and
librdkafka documentation and from Microsoft Learn ("Use Azure Event Hubs from Apache Kafka applications", "Exchange events
between consumers and producers that use different protocols"), not from the emulator's code.

With AZEMU_AZURE=1 the suite runs against the Kafka endpoint of a real namespace (kutil.azure_conf): every test makes its topics (event hubs)
through Resource Manager or the Kafka admin API and they are deleted when the test ends.
"""

import pytest

import autocreate_gate
import kutil  # first: it puts ../eventhubs on the path
import azure_eh


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
        if not emu.ca_file and not emu.azure:
            pytest.skip("no CA file: the emulator under test was not started by the fixture (set AZEMU_CA_FILE)")


@pytest.fixture(autouse=True)
def _azure_topics_are_removed(emu):
    """Against Azure every test makes its own topics (event hubs) and they go when the test ends: the namespace holds at most 10."""
    yield
    if emu.azure:
        while kutil.CREATED:
            name = kutil.CREATED.pop()
            try:
                azure_eh.delete_hub(name)
            except Exception:  # noqa: BLE001 - already deleted by the test itself
                pass


@pytest.fixture(scope="session", autouse=True)
def _azure_namespace_is_left_as_it_was(emu):
    """What a test made through the Kafka admin API and did not delete (a failed test) goes at the end. Hubs of the Event Hubs suite
    (`azemu-...`) and anything that was there before the session are left alone."""
    before = set(azure_eh.list_hubs()) if emu.azure else set()
    yield
    if emu.azure:
        for name in azure_eh.list_hubs():
            if name not in before and not name.startswith("azemu-"):
                azure_eh.delete_hub(name)


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
