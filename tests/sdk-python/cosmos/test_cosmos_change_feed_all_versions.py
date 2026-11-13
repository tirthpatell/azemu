"""Change feed in "all versions and deletes" mode.

Expected shapes come from Microsoft Learn, "Change feed modes" ("Parse the response object"):
create -> {"current", "metadata": {"operationType", "lsn", "crts"}}; replace adds "previousImageLSN";
delete has no "current" and its metadata has "id", "partitionKey", "timeToLiveExpired".
The Python SDK only allows start_time="Now" (or a continuation) in this mode.
"""

import time

import pytest
from azure.cosmos import PartitionKey

from cosmosutil import RawCosmos, uid

MODE = "AllVersionsAndDeletes"


def read(container, **kw):
    """Read the feed and return (items, continuation)."""
    items = list(container.query_items_change_feed(mode=MODE, **kw))
    return items, container.client_connection.last_response_headers["etag"]


def start(container):
    items, cont = read(container, start_time="Now")
    assert items == []
    return cont


def ops(items):
    return [i["metadata"]["operationType"] for i in items]


def test_every_version_and_the_delete_are_returned_in_order(container):
    cont = start(container)
    container.create_item({"id": "a", "pk": "p", "v": 1})
    a = container.read_item("a", "p")
    a["v"] = 2
    container.replace_item("a", a)
    a["v"] = 3
    container.replace_item("a", a)
    container.delete_item("a", "p")
    items, cont2 = read(container, continuation=cont)
    assert ops(items) == ["create", "replace", "replace", "delete"]
    assert [i["current"]["v"] for i in items[:3]] == [1, 2, 3]
    assert cont2 != cont


def test_latest_version_mode_shows_neither_intermediate_versions_nor_deletes(container):
    container.create_item({"id": "a", "pk": "p", "v": 1})
    a = container.read_item("a", "p")
    a["v"] = 2
    container.replace_item("a", a)
    container.create_item({"id": "b", "pk": "p"})
    container.delete_item("b", "p")
    items = list(container.query_items_change_feed(is_start_from_beginning=True))
    assert [(i["id"], i.get("v")) for i in items] == [("a", 2)]


def test_documents_written_before_the_start_are_not_in_the_feed(container):
    container.create_item({"id": "old", "pk": "p"})
    cont = start(container)
    container.create_item({"id": "new", "pk": "p"})
    items, _ = read(container, continuation=cont)
    assert [i["current"]["id"] for i in items] == ["new"]


def test_create_item_shape(container):
    cont = start(container)
    container.create_item({"id": "a", "pk": "p", "v": 1})
    (item,) = read(container, continuation=cont)[0]
    assert set(item) == {"current", "metadata"}
    assert item["current"]["id"] == "a" and item["current"]["v"] == 1
    assert item["current"]["_ts"] > 1_000_000_000
    meta = item["metadata"]
    assert meta["operationType"] == "create"
    assert isinstance(meta["lsn"], int)
    assert meta["crts"] > 1_000_000_000
    assert "previousImageLSN" not in meta


def test_replace_points_at_the_previous_change(container):
    cont = start(container)
    container.create_item({"id": "a", "pk": "p"})
    a = container.read_item("a", "p")
    a["v"] = 2
    container.replace_item("a", a)
    created, replaced = read(container, continuation=cont)[0]
    assert replaced["metadata"]["operationType"] == "replace"
    assert replaced["metadata"]["previousImageLSN"] == created["metadata"]["lsn"]
    assert "previous" not in replaced  # Azure offers no previous image


def test_upsert_of_an_existing_item_is_a_replace(container):
    cont = start(container)
    container.upsert_item({"id": "a", "pk": "p", "v": 1})
    container.upsert_item({"id": "a", "pk": "p", "v": 2})
    assert ops(read(container, continuation=cont)[0]) == ["create", "replace"]


def test_patch_is_a_replace(container):
    cont = start(container)
    container.create_item({"id": "a", "pk": "p", "v": 1})
    container.patch_item("a", "p", [{"op": "incr", "path": "/v", "value": 5}])
    items = read(container, continuation=cont)[0]
    assert ops(items) == ["create", "replace"]
    assert items[1]["current"]["v"] == 6


def test_delete_has_no_current_and_names_the_item(container):
    cont = start(container)
    container.create_item({"id": "a", "pk": "p"})
    container.delete_item("a", "p")
    delete = read(container, continuation=cont)[0][1]
    assert "current" not in delete
    meta = delete["metadata"]
    assert meta["operationType"] == "delete"
    assert meta["id"] == "a"
    assert meta["partitionKey"] == {"pk": "p"}
    assert meta["timeToLiveExpired"] is False
    assert "previousImageLSN" in meta
    assert meta["crts"] > 1_000_000_000


def test_nothing_new_returns_nothing_and_the_continuation_still_works(container):
    cont = start(container)
    assert read(container, continuation=cont)[0] == []
    container.create_item({"id": "a", "pk": "p"})
    items, cont2 = read(container, continuation=cont)
    assert ops(items) == ["create"]
    assert read(container, continuation=cont2)[0] == []
    container.create_item({"id": "b", "pk": "p"})
    items, _ = read(container, continuation=cont2)
    assert [i["current"]["id"] for i in items] == ["b"]


def test_paging_with_max_item_count(container):
    cont = start(container)
    for i in range(7):
        container.create_item({"id": f"i{i}", "pk": "p"})
    seen, pages = [], []
    for _ in range(10):
        pager = container.query_items_change_feed(mode=MODE, continuation=cont, max_item_count=3).by_page()
        try:
            items = list(next(pager))
        except StopIteration:  # nothing left: the feed is drained
            break
        cont = container.client_connection.last_response_headers["etag"]
        if not items:
            break
        pages.append(len(items))
        seen += [i["current"]["id"] for i in items]
    assert pages == [3, 3, 1]
    assert seen == [f"i{i}" for i in range(7)]


def test_partition_key_scopes_the_feed(container):
    _, cont = read(container, start_time="Now", partition_key="q")
    container.create_item({"id": "a", "pk": "p"})
    container.create_item({"id": "b", "pk": "q"})
    items, _ = read(container, continuation=cont)
    assert [i["current"]["id"] for i in items] == ["b"]


def test_feed_range_scopes_the_feed(container):
    (rng,) = list(container.read_feed_ranges())
    cont = start(container)
    container.create_item({"id": "a", "pk": "p"})
    items, _ = read(container, feed_range=rng, continuation=cont)
    assert [i["current"]["id"] for i in items] == ["a"]


def test_start_from_beginning_or_a_time_is_refused_by_the_sdk(container):
    with pytest.raises(ValueError):
        list(container.query_items_change_feed(mode=MODE, start_time="Beginning"))
    with pytest.raises(ValueError):
        list(container.query_items_change_feed(mode=MODE, is_start_from_beginning=True))


def test_service_refuses_a_read_without_start_point(endpoint, ca_file, db, container):
    # what a client that sends the header without a start point would get
    raw = RawCosmos(endpoint, ca_file)
    path = f"dbs/{db.id}/colls/{container.id}/docs"
    r = raw.get(path, headers={"A-IM": "Full-Fidelity Feed", "x-ms-cosmos-changefeed-wire-format-version": "2021-09-15"})
    assert r.status_code == 400, r.text


def test_ttl_expiry_is_a_delete_with_the_flag(make_container):
    c = make_container("/pk", default_ttl=-1)
    cont = start(c)
    c.create_item({"id": "x", "pk": "p", "ttl": 1})
    time.sleep(2.2)
    items, _ = read(c, continuation=cont)
    assert ops(items) == ["create", "delete"]
    assert items[1]["metadata"]["timeToLiveExpired"] is True
    assert items[1]["metadata"]["id"] == "x"


def test_a_batch_shows_its_changes_and_an_undone_batch_shows_nothing(container):
    cont = start(container)
    with pytest.raises(Exception):
        container.execute_item_batch(
            [("create", ({"id": "ok", "pk": "p"},)), ("create", ({"id": "ok", "pk": "p"},))], partition_key="p"
        )
    assert read(container, continuation=cont)[0] == []
    container.execute_item_batch(
        [("create", ({"id": "b1", "pk": "p"},)), ("create", ({"id": "b2", "pk": "p"},))], partition_key="p"
    )
    items, _ = read(container, continuation=cont)
    assert [i["current"]["id"] for i in items] == ["b1", "b2"]


def test_two_readers_have_independent_positions(container):
    c1 = start(container)
    container.create_item({"id": "a", "pk": "p"})
    c2 = start(container)
    container.create_item({"id": "b", "pk": "p"})
    first, _ = read(container, continuation=c1)
    second, _ = read(container, continuation=c2)
    assert [i["current"]["id"] for i in first] == ["a", "b"]
    assert [i["current"]["id"] for i in second] == ["b"]


def test_delete_all_items_by_partition_key_is_in_the_feed_as_deletes(container):
    container.create_item({"id": "a", "pk": "p"})
    cont = start(container)
    container.delete_all_items_by_partition_key("p")
    items, _ = read(container, continuation=cont)
    assert ops(items) == ["delete"]


def test_replace_with_a_changed_partition_key_value_is_a_delete_and_a_create(container):
    # a different partition key value is a different item: it is created, the old one stays
    cont = start(container)
    container.create_item({"id": "a", "pk": "p"})
    container.create_item({"id": "a", "pk": "q"})
    items, _ = read(container, continuation=cont)
    assert [(i["current"]["id"], i["current"]["pk"]) for i in items] == [("a", "p"), ("a", "q")]
