"""Fixtures for the Event Grid SDK tests (azure-eventgrid 4.22.1).

Hosts: `http://<name>.eventgrid.localhost:<port>`. `<name>` is a custom topic, a domain
or a namespace. Every test uses fresh names, and state is reset before each test anyway.

Tests marked ``emulator_only`` check something the emulator adds that real Azure does not
have: auto-creation of topics and subscriptions, the admin API under
``/_azemu/eventgrid``, the delivery log, shortened lock durations and waits.
"""

import os
import uuid

import pytest
from azure.core.credentials import AzureKeyCredential
from azure.eventgrid import EventGridConsumerClient, EventGridPublisherClient

import autocreate_gate
from egutil import Admin, EmuProc, Hook

# Retries of webhook deliveries are shortened for the default (session) emulator.
os.environ.setdefault("AZEMU_EVENTGRID_RETRY_BASE_MS", "50")
os.environ.setdefault("AZEMU_EVENTGRID_RETRY_MAX_MS", "200")


def pytest_configure(config):
    config.addinivalue_line(
        "markers",
        "emulator_only: checks something the emulator adds that real Azure Event Grid does not "
        "have (auto-creation, the admin API, the delivery log, shortened timings)",
    )
    autocreate_gate.add_markers(config)


def pytest_collection_modifyitems(config, items):
    autocreate_gate.gate(items, "AZEMU_EVENTGRID_AUTOCREATE")


@pytest.fixture(scope="session")
def azemu_service():
    return "eventgrid"


@pytest.fixture
def key():
    return AzureKeyCredential("c2VjcmV0")


@pytest.fixture
def name():
    return "t" + uuid.uuid4().hex[:10]


@pytest.fixture
def topic(admin, name):
    """Name of a custom topic that exists (made through the admin API; any input schema is accepted). The emulator does not
    create topics by being named, as Azure does not. `name` is a fresh name that nothing created."""
    admin.put_topic(name, expect=(201,))
    return name


@pytest.fixture
def topic_endpoint(emu, topic):
    """Full publish URL of a custom topic that exists."""
    return emu.url("eventgrid", topic) + "/api/events"


@pytest.fixture
def publisher(topic_endpoint, key):
    with EventGridPublisherClient(topic_endpoint, key, retry_total=0) as c:
        yield c


@pytest.fixture
def admin(emu):
    return Admin(emu)


@pytest.fixture
def ns(emu):
    """Name of a fresh namespace; `ns_endpoint` is its URL."""
    return "ns" + uuid.uuid4().hex[:10]


@pytest.fixture
def ns_endpoint(emu, ns):
    return emu.url("eventgrid", ns)


@pytest.fixture
def ns_topic(admin, ns):
    """A namespace topic with one pull subscription named `sub`, created through the admin API."""
    admin.put_topic("jobs", namespace=ns)
    admin.put_sub("jobs", "sub", {}, namespace=ns)
    return "jobs"


def make_consumer(endpoint, key, topic, sub, **kw):
    return EventGridConsumerClient(endpoint, key, namespace_topic=topic, subscription=sub, retry_total=0, **kw)


def make_producer(endpoint, key, topic, **kw):
    return EventGridPublisherClient(endpoint, key, namespace_topic=topic, retry_total=0, **kw)


@pytest.fixture
def consumer(ns_endpoint, key, ns_topic):
    with make_consumer(ns_endpoint, key, ns_topic, "sub") as c:
        yield c


@pytest.fixture
def producer(ns_endpoint, key, ns_topic):
    with make_producer(ns_endpoint, key, ns_topic) as c:
        yield c


@pytest.fixture
def hook():
    h = Hook()
    try:
        yield h
    finally:
        h.close()


@pytest.fixture(scope="module")
def strict_emu():
    """A separate emulator with auto-creation switched off (AZEMU_EVENTGRID_AUTOCREATE=0)."""
    p = EmuProc({"AZEMU_EVENTGRID_AUTOCREATE": "0"})
    try:
        yield p
    finally:
        p.close()


@pytest.fixture(autouse=True)
def _reset_strict(request):
    if "strict_emu" in request.fixturenames:
        request.getfixturevalue("strict_emu").reset()
    yield
