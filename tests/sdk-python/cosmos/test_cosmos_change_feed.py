"""Change feed (latest version mode). Scenarios follow Microsoft's test_change_feed."""

import pytest

from cosmosutil import uid


def fids(it):
    return [i["id"] for i in it]


def etag(c):
    return c.client_connection.last_response_headers["etag"]


def test_empty_container_change_feed_is_empty_with_continuation(container):
    assert list(container.query_items_change_feed(is_start_from_beginning=True)) == []
    assert etag(container)


def test_read_feed_ranges_returns_one_range_for_new_container(container):
    assert len(list(container.read_feed_ranges())) == 1


def test_from_beginning_returns_all_changes_in_write_order(container):
    for i in ("doc1", "doc2", "doc3"):
        container.create_item({"id": i, "pk": "pk"})
    assert fids(container.query_items_change_feed(is_start_from_beginning=True)) == ["doc1", "doc2", "doc3"]


@pytest.mark.not_on_vnext(reason="vnext change feed ignores start-from-now")
def test_from_now_returns_nothing_for_existing_documents(container):
    container.create_item({"id": "old", "pk": "pk"})
    assert list(container.query_items_change_feed()) == []
    assert etag(container)


def test_continuation_returns_only_newer_changes(container):
    container.create_item({"id": "doc1", "pk": "pk"})
    list(container.query_items_change_feed(is_start_from_beginning=True))
    cont = etag(container)
    container.create_item({"id": "doc2", "pk": "pk"})
    container.create_item({"id": "doc3", "pk": "pk3"})
    assert fids(container.query_items_change_feed(continuation=cont)) == ["doc2", "doc3"]


def test_continuation_changes_after_new_writes(container):
    list(container.query_items_change_feed(is_start_from_beginning=True))
    c1 = etag(container)
    container.create_item({"id": "doc1", "pk": "pk"})
    list(container.query_items_change_feed(is_start_from_beginning=True))
    assert etag(container) != c1


def test_continuation_after_everything_read_is_empty(container):
    container.create_item({"id": "doc1", "pk": "pk"})
    list(container.query_items_change_feed(is_start_from_beginning=True))
    cont = etag(container)
    assert list(container.query_items_change_feed(continuation=cont, is_start_from_beginning=True)) == []


@pytest.mark.parametrize("page_size", [1, 100])
def test_change_feed_pages_respect_max_item_count(container, page_size):
    container.create_item({"id": "doc1", "pk": "pk"})
    list(container.query_items_change_feed(is_start_from_beginning=True))
    cont = etag(container)
    container.create_item({"id": "doc2", "pk": "pk"})
    container.create_item({"id": "doc3", "pk": "pk"})
    pages = [list(p) for p in container.query_items_change_feed(continuation=cont, max_item_count=page_size).by_page()]
    assert all(len(p) <= page_size for p in pages)
    assert [i["id"] for p in pages for i in p] == ["doc2", "doc3"]


@pytest.mark.not_on_vnext(reason="vnext change feed ignores the partition key filter")
def test_change_feed_for_one_partition_key(container):
    container.create_item({"id": "doc1", "pk": "pk"})
    container.create_item({"id": "doc2", "pk": "pk2"})
    container.create_item({"id": "doc3", "pk": "pk"})
    got = fids(container.query_items_change_feed(is_start_from_beginning=True, partition_key="pk"))
    assert got == ["doc1", "doc3"]


def test_change_feed_for_feed_range(container):
    container.create_item({"id": "doc1", "pk": "pk"})
    container.create_item({"id": "doc2", "pk": "pk2"})
    [rng] = list(container.read_feed_ranges())
    got = fids(container.query_items_change_feed(is_start_from_beginning=True, feed_range=rng))
    assert got == ["doc1", "doc2"]


def test_change_feed_by_partition_key_range_id(container):
    container.create_item({"id": "doc1", "pk": "pk"})
    got = fids(container.query_items_change_feed(is_start_from_beginning=True, partition_key_range_id="0"))
    assert got == ["doc1"]


def test_updates_appear_as_new_changes_with_latest_version(container):
    container.create_item({"id": "a", "pk": "pk", "v": 1})
    list(container.query_items_change_feed(is_start_from_beginning=True))
    cont = etag(container)
    container.replace_item("a", {"id": "a", "pk": "pk", "v": 2})
    got = list(container.query_items_change_feed(continuation=cont))
    assert [(i["id"], i["v"]) for i in got] == [("a", 2)]


def test_change_feed_items_have_system_properties(container):
    container.create_item({"id": "a", "pk": "pk"})
    [item] = list(container.query_items_change_feed(is_start_from_beginning=True))
    assert item["_etag"] and item["_ts"] and item["_rid"]


def test_deletes_are_not_in_latest_version_change_feed(container):
    container.create_item({"id": "a", "pk": "pk"})
    list(container.query_items_change_feed(is_start_from_beginning=True))
    cont = etag(container)
    container.delete_item("a", "pk")
    assert list(container.query_items_change_feed(continuation=cont)) == []


def test_change_feed_across_partitions_from_beginning(container):
    for i in range(4):
        container.create_item({"id": f"doc{i}", "pk": f"pk{i}"})
    assert fids(container.query_items_change_feed(start_time="Beginning")) == ["doc0", "doc1", "doc2", "doc3"]


def test_change_feed_with_future_start_time_is_empty(container):
    from datetime import datetime, timedelta, timezone

    container.create_item({"id": "a", "pk": "pk"})
    future = datetime.now(timezone.utc) + timedelta(hours=1)
    assert list(container.query_items_change_feed(start_time=future)) == []
