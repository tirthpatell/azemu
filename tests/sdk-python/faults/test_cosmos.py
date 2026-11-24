"""Cosmos DB: the client waits for `x-ms-retry-after-ms` and retries a 429; with its retries used up the 429 is raised."""

import pytest
from azure.cosmos import CosmosClient, PartitionKey
from azure.cosmos.exceptions import CosmosHttpResponseError

from cosmosutil import KEY, uid
from faultutil import Stopwatch


def kwargs(emu, **extra):
    from cosmosutil import client_kwargs

    kw = client_kwargs(emu.ca_file, **extra)
    # the header is only read when no fixed interval is set
    kw.pop("retry_fixed_interval", None)
    return kw


@pytest.fixture
def item(emu):
    """A database, container and item made before any rule exists. Returns (endpoint, db, container)."""
    endpoint = emu.https_url("documents", uid("acct"))
    db, cont = uid("db"), uid("c")
    with CosmosClient(endpoint, KEY, **kwargs(emu)) as c:
        d = c.create_database(db)
        k = d.create_container(cont, partition_key=PartitionKey(path="/pk"))
        k.create_item({"id": "1", "pk": "a", "v": 1})
    return endpoint, db, cont


def test_cosmos_waits_for_the_retry_header_and_then_succeeds(emu, faults, item):
    endpoint, db, cont = item
    with CosmosClient(endpoint, KEY, **kwargs(emu, retry_total=5, retry_throttle_total=5)) as c:
        k = c.get_database_client(db).get_container_client(cont)
        # the rule names the item's own path, so the client's reads of account and container metadata are not throttled
        rule = faults.add(service="cosmos", method="GET", path=f"/dbs/{db}/colls/{cont}/docs/*", times=2, fault={"kind": "throttle", "retryAfterMs": 600})
        with Stopwatch() as t:
            doc = k.read_item("1", partition_key="a")
    assert doc["v"] == 1
    assert faults.fired(rule["id"]) == 2
    assert 1.1 <= t.seconds < 8, f"two waits of 0.6 s: {t.seconds:.2f}"


def test_cosmos_raises_the_429_when_the_retries_are_used_up(emu, faults, item):
    endpoint, db, cont = item
    faults.add(service="cosmos", method="GET", path=f"/dbs/{db}/colls/{cont}/docs/*", fault={"kind": "throttle", "retryAfterMs": 50})
    with CosmosClient(endpoint, KEY, **kwargs(emu, retry_total=1, retry_throttle_total=1)) as c:
        k = c.get_database_client(db).get_container_client(cont)
        with pytest.raises(CosmosHttpResponseError) as e:
            k.read_item("1", partition_key="a")
    assert e.value.status_code == 429
    assert e.value.sub_status == 3200
    assert e.value.headers["x-ms-retry-after-ms"] == "50"
    assert "Request rate is large" in str(e.value)
    assert faults.fired() == 2, "the first try and the one retry"


def test_cosmos_ru_budget_is_waited_out_by_the_client(emu, faults, item):
    """A budget of 1 RU a second and reads that cost about 1 RU each: the client is throttled now and then, waits for the
    next second as the header says, and every read still succeeds."""
    endpoint, db, cont = item
    rule = faults.add(service="cosmos", path=f"/dbs/{db}/colls/{cont}/docs/*", fault={"kind": "ruBudget", "ruPerSecond": 1})
    with CosmosClient(endpoint, KEY, **kwargs(emu, retry_total=5, retry_throttle_total=5)) as c:
        k = c.get_database_client(db).get_container_client(cont)
        with Stopwatch() as t:
            for _ in range(5):
                assert k.read_item("1", partition_key="a")["id"] == "1"
    assert faults.fired(rule["id"]) >= 1, "at least one read was throttled"
    # five reads at about one a second: it cannot have been faster than two seconds
    assert t.seconds >= 2.0


def test_cosmos_connection_drop_is_retried_for_a_read(emu, faults, item):
    endpoint, db, cont = item
    faults.add(service="cosmos", method="GET", path=f"/dbs/{db}/colls/{cont}/docs/*", times=1, fault={"kind": "drop"})
    with CosmosClient(endpoint, KEY, **kwargs(emu, retry_total=3)) as c:
        k = c.get_database_client(db).get_container_client(cont)
        assert k.read_item("1", partition_key="a")["id"] == "1"
    assert faults.fired() == 1
