"""Time to live. Only these three tests sleep."""

import time

import pytest
from azure.cosmos import PartitionKey
from azure.cosmos.exceptions import CosmosHttpResponseError, CosmosResourceNotFoundError

from cosmosutil import uid


def test_ttl_values_are_validated(make_container):
    # Microsoft test_ttl: 0, null and negative per-item ttl are rejected, -1 is "never expire"
    c = make_container("/id", default_ttl=10)
    for bad in (0, None, -10):
        with pytest.raises(CosmosHttpResponseError) as e:
            c.create_item({"id": uid("d"), "ttl": bad})
        assert e.value.status_code == 400
    c.create_item({"id": uid("d"), "ttl": -1})


def test_item_ttl_is_ignored_when_container_has_no_default_ttl(make_container):
    c = make_container("/id")
    c.create_item({"id": "a", "ttl": 1})
    time.sleep(3)
    assert c.read_item("a", "a")["ttl"] == 1


def test_container_default_ttl_expires_items(make_container):
    c = make_container("/id", default_ttl=2)
    c.create_item({"id": "a"})
    c.create_item({"id": "forever", "ttl": -1})
    deadline = time.time() + 15
    gone = False
    while time.time() < deadline:
        time.sleep(1)
        try:
            c.read_item("a", "a")
        except CosmosResourceNotFoundError:
            gone = True
            break
    assert gone, "item with default ttl 2s was still readable after 15s"
    assert c.read_item("forever", "forever")["id"] == "forever"


def test_item_ttl_overrides_container_default(make_container):
    c = make_container("/id", default_ttl=3600)
    c.create_item({"id": "short", "ttl": 1})
    c.create_item({"id": "long"})
    deadline = time.time() + 15
    gone = False
    while time.time() < deadline:
        time.sleep(1)
        try:
            c.read_item("short", "short")
        except CosmosResourceNotFoundError:
            gone = True
            break
    assert gone, "item with ttl 1s was still readable after 15s"
    assert c.read_item("long", "long")["id"] == "long"
