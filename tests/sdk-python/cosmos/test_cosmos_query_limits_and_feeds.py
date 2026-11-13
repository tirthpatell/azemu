"""Query limits and feeds, seen through the Python SDK and raw HTTP:
the memory budget of a query, headers of every feed answer, the size of a change feed page, and kept query results.
"""

import json

import pytest
from azure.cosmos.exceptions import CosmosHttpResponseError

from cosmosutil import RawCosmos


def big_array_doc(n):
    return {"id": "big", "pk": "p", "arr": list(range(n))}


def test_a_self_join_that_is_counted_needs_no_memory_and_answers(container):
    container.create_item(big_array_doc(700))
    rows = list(container.query_items(
        "SELECT VALUE COUNT(1) FROM c JOIN a IN c.arr JOIN b IN c.arr", partition_key="p"))
    assert rows == [700 * 700]


def test_a_self_join_with_top_stops_after_the_rows_it_needs(container):
    container.create_item(big_array_doc(2000))
    rows = list(container.query_items(
        "SELECT TOP 5 VALUE a * 10000 + b FROM c JOIN a IN c.arr JOIN b IN c.arr", partition_key="p"))
    assert rows == [0, 1, 2, 3, 4]


def test_a_result_far_over_the_memory_budget_is_a_400_that_says_so(container):
    # 4 million rows would have to be sorted in memory
    container.create_item(big_array_doc(2000))
    with pytest.raises(CosmosHttpResponseError) as e:
        list(container.query_items(
            "SELECT a, b FROM c JOIN a IN c.arr JOIN b IN c.arr ORDER BY a DESC", partition_key="p"))
    assert e.value.status_code == 400
    assert "memory" in str(e.value)
    # the service is still there
    assert container.read_item("big", "p")["id"] == "big"


def test_a_function_that_would_build_a_huge_string_is_refused(container):
    container.create_item({"id": "s", "pk": "p", "s": "y" * 1_500_000})
    with pytest.raises(CosmosHttpResponseError) as e:
        list(container.query_items("SELECT VALUE REPLACE(c.s, 'y', c.s) FROM c", partition_key="p"))
    assert e.value.status_code == 400
    # the same value is fine when it is not multiplied
    assert list(container.query_items("SELECT VALUE LENGTH(c.s) FROM c", partition_key="p")) == [1_500_000]


def test_documents_come_back_as_they_were_stored(container):
    docs = [{"id": f"d{i}", "pk": "p", "n": i, "tags": ["a", "b"], "o": {"k": [i]}} for i in range(5)]
    for d in docs:
        container.create_item(d)
    got = list(container.query_items("SELECT * FROM c WHERE c.n >= 3 ORDER BY c.n", partition_key="p"))
    assert [g["id"] for g in got] == ["d3", "d4"]
    assert got[0]["o"] == {"k": [3]} and got[0]["tags"] == ["a", "b"]
    assert all(k in got[0] for k in ("_rid", "_etag", "_ts", "_self"))
    assert list(container.query_items("SELECT VALUE c.o.k[0] FROM c WHERE c.n = 2", partition_key="p")) == [2]


def test_every_feed_answer_has_an_item_count_even_a_304(endpoint, ca_file, db, container):
    raw = RawCosmos(endpoint, ca_file)
    docs = f"dbs/{db.id}/colls/{container.id}/docs"
    container.create_item({"id": "a", "pk": "p"})
    r = raw.get(docs, headers={"A-IM": "Incremental Feed"})
    assert r.status_code == 200 and r.headers["x-ms-item-count"] == "1"
    etag = r.headers["etag"]
    r = raw.get(docs, headers={"A-IM": "Incremental Feed", "If-None-Match": etag})
    assert r.status_code == 304
    assert r.headers["x-ms-item-count"] == "0"
    assert r.headers["etag"] == etag
    for h in ("x-ms-request-charge", "x-ms-activity-id", "x-ms-session-token", "lsn"):
        assert h in r.headers, h
    r = raw.get(f"dbs/{db.id}/colls/{container.id}/pkranges")
    assert r.status_code == 200 and "x-ms-item-count" in r.headers
    r = raw.get(f"dbs/{db.id}/colls/{container.id}/pkranges", headers={"If-None-Match": r.headers["etag"]})
    assert r.status_code == 304 and r.headers["x-ms-item-count"] == "0"


def test_a_change_feed_page_is_at_most_4_mb_and_the_pages_add_up(endpoint, ca_file, db, container):
    pad = "x" * 600_000
    for i in range(10):
        container.create_item({"id": f"d{i}", "pk": "p", "pad": pad})
    raw = RawCosmos(endpoint, ca_file)
    docs = f"dbs/{db.id}/colls/{container.id}/docs"
    seen, token, pages = [], None, 0
    while True:
        h = {"A-IM": "Incremental Feed", "x-ms-max-item-count": "100000"}
        if token:
            h["If-None-Match"] = token
        r = raw.get(docs, headers=h)
        if r.status_code == 304:
            break
        assert r.status_code == 200
        items = json.loads(r.text)["Documents"]
        assert 0 < len(items) <= 7
        seen += [d["id"] for d in items]
        token = r.headers["etag"]
        pages += 1
        assert pages < 10
    assert seen == [f"d{i}" for i in range(10)] and pages >= 2


def test_change_feed_refuses_a_date_it_cannot_read_and_a_position_it_has_not_reached(endpoint, ca_file, db, container):
    raw = RawCosmos(endpoint, ca_file)
    docs = f"dbs/{db.id}/colls/{container.id}/docs"
    container.create_item({"id": "a", "pk": "p"})
    r = raw.get(docs, headers={"A-IM": "Incremental Feed", "If-Modified-Since": "not a date"})
    assert r.status_code == 400
    r = raw.get(docs, headers={"A-IM": "Incremental Feed", "If-None-Match": '"99999"'})
    assert r.status_code == 400 and "ahead" in r.text


def test_a_continuation_of_a_deleted_container_does_not_serve_its_rows_to_a_new_one(db):
    from azure.cosmos import PartitionKey
    name = "recreated"
    c = db.create_container(name, PartitionKey(path="/pk"))
    for i in range(6):
        c.create_item({"id": f"old{i}", "pk": "p"})
    it = c.query_items("SELECT VALUE c.id FROM c", max_item_count=2, enable_cross_partition_query=True).by_page()
    first = list(next(it))
    token = it.continuation_token
    assert first == ["old0", "old1"] and token
    db.delete_container(name)
    c = db.create_container(name, PartitionKey(path="/pk"))
    for i in range(6):
        c.create_item({"id": f"new{i}", "pk": "p"})
    it = c.query_items("SELECT VALUE c.id FROM c", max_item_count=2, enable_cross_partition_query=True).by_page(token)
    assert list(next(it)) == ["new2", "new3"]
