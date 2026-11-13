"""Queries: paging, continuation tokens, response headers, query metrics."""

import pytest

from cosmosdata import PRODUCTS
from cosmosutil import idlist, ids, run

ALL_IDS = sorted(d["id"] for d in PRODUCTS)


def pages_of(it):
    return [list(p) for p in it.by_page()]


def test_by_page_respects_max_item_count_single_partition(products):
    it = products.query_items("SELECT c.id FROM c", partition_key="books", max_item_count=4)
    pages = pages_of(it)
    assert [len(p) for p in pages] == [4, 2]


def test_by_page_respects_max_item_count_cross_partition(products):
    it = products.query_items("SELECT c.id FROM c", enable_cross_partition_query=True, max_item_count=5)
    pages = pages_of(it)
    assert all(0 < len(p) <= 5 for p in pages)
    assert sorted(i["id"] for p in pages for i in p) == ALL_IDS


@pytest.mark.not_on_vnext(reason="vnext ignores max_item_count for ORDER BY queries")
def test_paging_with_order_by_keeps_order(products):
    it = products.query_items("SELECT c.id FROM c ORDER BY c.price", enable_cross_partition_query=True, max_item_count=7)
    pages = pages_of(it)
    assert all(len(p) <= 7 for p in pages)
    flat = [i["id"] for p in pages for i in p]
    assert flat == [d["id"] for d in sorted(PRODUCTS, key=lambda d: d["price"])]


def test_iterating_items_spans_pages(products):
    it = products.query_items("SELECT c.id FROM c", enable_cross_partition_query=True, max_item_count=3)
    assert ids(list(it)) == ALL_IDS


def test_max_item_count_one_gives_single_item_pages(products):
    it = products.query_items("SELECT c.id FROM c", partition_key="toys", max_item_count=1)
    pages = pages_of(it)
    assert [len(p) for p in pages] == [1] * 6


def test_continuation_token_present_when_more_pages_remain(products):
    it = products.query_items("SELECT c.id FROM c", partition_key="books", max_item_count=2)
    pager = it.by_page()
    next(pager)
    assert pager.continuation_token


def test_continuation_token_none_after_last_page(products):
    it = products.query_items("SELECT c.id FROM c", partition_key="books", max_item_count=10)
    pager = it.by_page()
    list(next(pager))
    assert not pager.continuation_token


@pytest.mark.not_on_vnext(reason="vnext returns no continuation token for ORDER BY queries")
def test_resume_single_partition_query_from_continuation_token(products):
    q = "SELECT c.id FROM c ORDER BY c.price"
    it = products.query_items(q, partition_key="garden", max_item_count=2)
    pager = it.by_page()
    first = list(next(pager))
    token = pager.continuation_token
    assert token
    it2 = products.query_items(q, partition_key="garden", max_item_count=2)
    rest = [i for page in it2.by_page(token) for i in page]
    expected = [d["id"] for d in sorted(PRODUCTS, key=lambda d: d["price"]) if d["category"] == "garden"]
    assert [i["id"] for i in first] + [i["id"] for i in rest] == expected


def test_resume_second_page_equals_page_fetched_in_sequence(products):
    # same shape as Microsoft's test_paging_with_continuation_token
    it = products.query_items("SELECT * FROM c", partition_key="books", max_item_count=1)
    pager = it.by_page()
    next(pager)
    token = pager.continuation_token
    second = list(next(pager))[0]
    again = list(next(products.query_items("SELECT * FROM c", partition_key="books", max_item_count=1).by_page(token)))[0]
    assert second["id"] == again["id"]


def test_resume_cross_partition_query_from_continuation_token(products):
    it = products.query_items("SELECT c.id FROM c", enable_cross_partition_query=True, max_item_count=1)
    pager = it.by_page()
    next(pager)
    token = pager.continuation_token
    second = list(next(pager))[0]
    it2 = products.query_items("SELECT c.id FROM c", enable_cross_partition_query=True, max_item_count=1)
    again = list(next(it2.by_page(token)))[0]
    assert second["id"] == again["id"]


def test_pages_do_not_repeat_or_skip_documents(products):
    it = products.query_items("SELECT c.id FROM c", partition_key="electronics", max_item_count=4)
    flat = [i["id"] for p in pages_of(it) for i in p]
    assert len(flat) == len(set(flat)) == 6


def test_response_hook_receives_headers_and_body(products):
    seen = []
    list(products.query_items("SELECT c.id FROM c", partition_key="books", response_hook=lambda h, b: seen.append((h, b))))
    assert seen
    headers, body = seen[0]
    assert "x-ms-request-charge" in headers
    assert float(headers["x-ms-request-charge"]) >= 0


def test_query_response_headers_have_request_charge_and_item_count(products):
    it = products.query_items("SELECT c.id FROM c", partition_key="books")
    list(it)
    h = products.client_connection.last_response_headers
    assert float(h["x-ms-request-charge"]) >= 0
    assert int(h["x-ms-item-count"]) == 6


def test_query_response_has_activity_id(products):
    list(products.query_items("SELECT c.id FROM c", partition_key="books"))
    assert products.client_connection.last_response_headers["x-ms-activity-id"]


def test_populate_query_metrics_header_is_well_formed(products):
    # Microsoft test_populate_query_metrics: "key1=value1;key2=value2;..."
    list(products.query_items("SELECT * FROM c", partition_key="books", populate_query_metrics=True))
    header = products.client_connection.last_response_headers["x-ms-documentdb-query-metrics"]
    parts = header.split(";")
    assert len(parts) > 1 and all("=" in p for p in parts)


def test_get_response_headers_on_query_iterable(products):
    it = products.query_items("SELECT c.id FROM c", partition_key="books")
    list(it)
    assert "x-ms-request-charge" in it.get_response_headers()


def test_empty_result_yields_no_items(products):
    it = products.query_items("SELECT * FROM c WHERE c.id = 'nope'", partition_key="books")
    assert [i for p in pages_of(it) for i in p] == []


def test_single_partition_query_does_not_see_other_partitions(products):
    for cat in ("books", "toys"):
        got = run(products, "SELECT c.category FROM c", partition_key=cat)
        assert {r["category"] for r in got} == {cat}


def test_cross_partition_query_sees_every_partition(products):
    got = run(products, "SELECT DISTINCT VALUE c.category FROM c")
    assert len(got) == 4


def test_cross_partition_order_by_merges_partitions(products):
    got = run(products, "SELECT VALUE c.category FROM c ORDER BY c.price")
    assert len(got) == 24 and len(set(got)) == 4


def test_cross_partition_top(products):
    assert len(run(products, "SELECT TOP 7 c.id FROM c")) == 7


def test_cross_partition_offset_limit_pages_correctly(products):
    q = "SELECT c.id FROM c ORDER BY c.price OFFSET 3 LIMIT 5"
    it = products.query_items(q, enable_cross_partition_query=True, max_item_count=2)
    flat = [i["id"] for p in pages_of(it) for i in p]
    assert flat == [d["id"] for d in sorted(PRODUCTS, key=lambda d: d["price"])][3:8]


def test_cross_partition_count_with_where(products):
    assert run(products, "SELECT VALUE COUNT(1) FROM c WHERE c.stock = 0") == [3]
