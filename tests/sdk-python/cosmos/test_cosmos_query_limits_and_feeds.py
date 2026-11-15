"""Query limits and feeds, seen through the Python SDK and raw HTTP:
the memory budget of a query, headers of every feed answer, the size of a change feed page, and kept query results.
"""

import json
import time

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


@pytest.mark.emulator_only
def test_a_result_far_over_the_memory_budget_is_a_400_that_says_so(container):
    # 4 million rows would have to be sorted in memory (the emulator's own limit; Azure has others)
    container.create_item(big_array_doc(2000))
    with pytest.raises(CosmosHttpResponseError) as e:
        list(container.query_items(
            "SELECT a, b FROM c JOIN a IN c.arr JOIN b IN c.arr ORDER BY c.id DESC", partition_key="p"))
    assert e.value.status_code == 400
    assert "memory" in str(e.value)
    # the service is still there
    assert container.read_item("big", "p")["id"] == "big"


def test_order_by_over_a_join_alias_is_a_400(container):
    # checked against a real account in November 2026
    container.create_item({"id": "x", "pk": "p", "arr": [3, 1, 2], "t": "a"})
    for sql in ("SELECT c.id, a FROM c JOIN a IN c.arr ORDER BY a",
                "SELECT c.id FROM c JOIN a IN c.arr ORDER BY a.a"):
        with pytest.raises(CosmosHttpResponseError) as e:
            list(container.query_items(sql, partition_key="p"))
        assert e.value.status_code == 400
        assert "Order-by over correlated collections is not supported." in e.value.message
    # a property of the document is fine with a JOIN in the query
    rows = list(container.query_items("SELECT c.id FROM c JOIN a IN c.arr ORDER BY c.t", partition_key="p"))
    assert len(rows) == 3


# Azure answered 503 to this call in November 2026 (the assertion below is what it said), but the call is slow and heavy on a paid
# account (the second try ran into the time limit of the test), so it is not repeated there.
@pytest.mark.emulator_only
def test_a_function_that_would_build_a_huge_string_is_refused(db, container, make_client):
    container.create_item({"id": "s", "pk": "p", "s": "y" * 1_500_000})
    # a client that does not retry the 503
    quick = make_client(retry_total=1, retry_throttle_total=1, retry_backoff_max=1, retry_fixed_interval=1)
    quick = quick.get_database_client(db.id).get_container_client(container.id)
    with pytest.raises(CosmosHttpResponseError) as e:
        list(quick.query_items("SELECT VALUE REPLACE(c.s, 'y', c.s) FROM c", partition_key="p"))
    # Azure answers 503 ServiceUnavailable (checked in November 2026, tried once)
    assert e.value.status_code == 503
    assert "Service is currently unavailable" in str(e.value)
    # the same value is fine when it is not multiplied (a real account answered 503 for a short time after the failed call)
    for attempt in range(6):
        try:
            got = list(container.query_items("SELECT VALUE LENGTH(c.s) FROM c", partition_key="p"))
            break
        except CosmosHttpResponseError as err:
            if err.status_code != 503 or attempt == 5:
                raise
            time.sleep(5)
    assert got == [1_500_000]


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


def test_partition_key_ranges_answer_has_an_item_count_and_no_etag_and_no_request_charge(endpoint, ca_file, db, container):
    # Azure (checked in November 2026): no `etag`, no `x-ms-request-charge`; `If-None-Match` is ignored (a plain 200)
    raw = RawCosmos(endpoint, ca_file)
    r = raw.get(f"dbs/{db.id}/colls/{container.id}/pkranges")
    assert r.status_code == 200 and r.headers["x-ms-item-count"] == "1"
    assert "etag" not in r.headers and "x-ms-request-charge" not in r.headers
    body = r.json()
    assert body["_count"] == 1 and body["_rid"]
    rng = body["PartitionKeyRanges"][0]
    for key in ("id", "minInclusive", "maxExclusive", "ridPrefix", "throughputFraction", "status", "parents", "_etag", "_rid", "_self", "_ts", "lsn"):
        assert key in rng, key
    r = raw.get(f"dbs/{db.id}/colls/{container.id}/pkranges", headers={"If-None-Match": rng["_etag"]})
    assert r.status_code == 200 and len(r.json()["PartitionKeyRanges"]) == 1


def test_partition_key_ranges_read_as_a_change_feed_have_an_etag_and_end_in_a_304(endpoint, ca_file, db, container):
    # The Python SDK reads the ranges with `A-IM: Incremental feed` and asks again with the etag until the answer is a 304
    # (checked against Azure in November 2026: the feed has an etag and no request charge; zero starts over; text is a 400)
    raw = RawCosmos(endpoint, ca_file)
    path = f"dbs/{db.id}/colls/{container.id}/pkranges"
    feed = {"A-IM": "Incremental Feed"}
    r = raw.get(path, headers=feed)
    assert r.status_code == 200 and r.headers["x-ms-item-count"] == "1" and "x-ms-request-charge" not in r.headers
    etag = r.headers["etag"]
    assert etag.startswith('"') and etag.strip('"').isdigit()
    r = raw.get(path, headers={**feed, "If-None-Match": etag})
    assert r.status_code == 304 and r.text == ""
    assert r.headers["x-ms-item-count"] == "0" and r.headers["etag"] == etag and "x-ms-request-charge" not in r.headers
    assert raw.get(path, headers={**feed, "If-None-Match": '"0"'}).status_code == 200
    r = raw.get(path, headers={**feed, "If-None-Match": '"abc"'})
    assert r.status_code == 400 and "Change feed token format is invalid" in r.text


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


def test_change_feed_ignores_a_date_it_cannot_read_and_a_position_it_has_not_reached_is_a_304(endpoint, ca_file, db, container):
    # Azure (checked in November 2026): an If-Modified-Since that is not a date is ignored (the feed starts at the
    # beginning); a position that is ahead of the container is a 304 with no body that gives the position back; a position
    # that is not a number is a 400
    raw = RawCosmos(endpoint, ca_file)
    docs = f"dbs/{db.id}/colls/{container.id}/docs"
    container.create_item({"id": "a", "pk": "p"})
    r = raw.get(docs, headers={"A-IM": "Incremental Feed", "If-Modified-Since": "not a date"})
    assert r.status_code == 200 and [d["id"] for d in r.json()["Documents"]] == ["a"]
    r = raw.get(docs, headers={"A-IM": "Incremental Feed", "If-None-Match": '"99999"'})
    assert r.status_code == 304 and r.text == ""
    assert r.headers["etag"] == '"99999"' and r.headers["x-ms-item-count"] == "0"
    r = raw.get(docs, headers={"A-IM": "Incremental Feed", "If-None-Match": '"abc"'})
    assert r.status_code == 400 and "Change feed token format is invalid" in r.text, r.text
    for start in ('"0"', '"-1"'):
        r = raw.get(docs, headers={"A-IM": "Incremental Feed", "If-None-Match": start})
        assert r.status_code == 200 and len(r.json()["Documents"]) == 1, start


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
    # Azure answered 400 "the continuation token ... is malformed" once (the SDK catches it and ends with an empty page) and
    # started from the beginning another time; either way the rows of the new container are not served from the old position
    it = c.query_items("SELECT VALUE c.id FROM c", max_item_count=2, enable_cross_partition_query=True).by_page(token)
    try:
        got = list(next(it))
    except (StopIteration, CosmosHttpResponseError):
        got = []
    assert got in ([], ["new0", "new1"]), got
