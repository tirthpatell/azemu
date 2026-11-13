"""Request input that must be refused instead of crashing or exhausting the service.

Limits (Microsoft Learn, "Service quotas and default limits"): nesting of embedded objects and
arrays 128 levels, request size 2 MB, SQL query length 512 KB; patch accepts 10 operations.
After every refusal the service must still answer.
"""

import json

import pytest
from azure.cosmos.exceptions import CosmosHttpResponseError

from cosmosutil import RawCosmos


def nested(levels):
    v = 1
    for _ in range(levels):
        v = {"a": v}
    return v


def alive(container):
    container.upsert_item({"id": "alive", "pk": "p"})
    assert container.read_item("alive", "p")["id"] == "alive"


def path_of(levels):
    return "/x" + "/a" * levels


def test_a_document_nested_100000_deep_is_a_400(endpoint, ca_file, db, container):
    raw = RawCosmos(endpoint, ca_file)
    body = '{"id":"deep","pk":"p","x":' + '{"a":' * 100_000 + "1" + "}" * 100_000 + "}"
    r = raw.post(
        f"dbs/{db.id}/colls/{container.id}/docs",
        body=body,
        headers={"x-ms-documentdb-partitionkey": '["p"]', "content-type": "application/json"},
    )
    assert r.status_code == 400, r.text[:200]
    alive(container)


def test_the_deepest_allowed_document_round_trips(container):
    container.create_item({"id": "ok", "pk": "p", "x": nested(120)})
    assert container.read_item("ok", "p")["x"]["a"]["a"]
    with pytest.raises(CosmosHttpResponseError) as e:
        container.create_item({"id": "too", "pk": "p", "x": nested(130)})
    assert e.value.status_code == 400


def test_patch_with_ten_operations_works_and_eleven_is_a_400(container):
    container.create_item({"id": "a", "pk": "p"})
    ten = [{"op": "add", "path": f"/f{i}", "value": i} for i in range(10)]
    assert container.patch_item("a", "p", ten)["f9"] == 9
    with pytest.raises(CosmosHttpResponseError) as e:
        container.patch_item("a", "p", ten + [{"op": "add", "path": "/f10", "value": 10}])
    assert e.value.status_code == 400
    assert "f10" not in container.read_item("a", "p")


def test_repeated_patches_cannot_build_a_document_deeper_than_the_limit(container):
    container.create_item({"id": "a", "pk": "p", "x": {}})
    path = "/x"
    accepted = 0
    for _ in range(50):
        try:
            container.patch_item("a", "p", [{"op": "add", "path": path + "/a", "value": nested(100)}])
        except CosmosHttpResponseError as e:
            assert e.status_code == 400
            break
        accepted += 1
        path += "/a" * 100
    assert accepted == 1
    doc = container.read_item("a", "p")  # still readable
    assert doc["x"]["a"]
    alive(container)


def test_patch_path_with_100000_segments_is_a_400(container):
    container.create_item({"id": "a", "pk": "p"})
    for op in ("add", "set", "replace", "remove", "incr"):
        with pytest.raises(CosmosHttpResponseError) as e:
            container.patch_item("a", "p", [{"op": op, "path": "/a" * 100_000, "value": 1}])
        assert e.value.status_code == 400
    alive(container)


@pytest.mark.parametrize(
    "make",
    [
        lambda n: "SELECT VALUE " + "(" * n + "1" + ")" * n + " FROM c",
        lambda n: "SELECT VALUE " + "ARRAY(SELECT VALUE " * n + "1" + ")" * n + " FROM c",
        lambda n: "SELECT VALUE c.x IN (" + "[" * n + "1" + "]" * n + ") FROM c",
        lambda n: "SELECT VALUE " + "c.q ?? " * n + "1 FROM c",
        lambda n: "SELECT VALUE " + "-" * n + "1 FROM c",
    ],
    ids=["parentheses", "array-subqueries", "nested-in-lists", "coalesce-chain", "unary-minus"],
)
def test_a_query_nested_10000_deep_is_a_400(container, make):
    with pytest.raises(CosmosHttpResponseError) as e:
        list(container.query_items(make(10_000), partition_key="p"))
    assert e.value.status_code == 400
    alive(container)


def test_a_query_over_512_kb_is_a_400(container):
    q = "SELECT * FROM c WHERE c.x = 1" + " OR c.x = 2" * 60_000
    assert len(q) > 512 * 1024
    with pytest.raises(CosmosHttpResponseError) as e:
        list(container.query_items(q, partition_key="p"))
    assert e.value.status_code == 400


def test_a_request_over_two_megabytes_is_a_413(container):
    container.create_item({"id": "a", "pk": "p"})
    with pytest.raises(CosmosHttpResponseError) as e:
        container.patch_item("a", "p", [{"op": "add", "path": "/big", "value": "x" * 2_300_000}])
    assert e.value.status_code == 413
    with pytest.raises(CosmosHttpResponseError) as e:
        list(container.query_items("SELECT * FROM c WHERE c.s = @s", parameters=[{"name": "@s", "value": "x" * 2_300_000}], partition_key="p"))
    assert e.value.status_code == 413
    alive(container)


def test_a_query_that_is_deep_but_allowed_runs(container):
    container.create_item({"id": "a", "pk": "p", "n": 5})
    q = "SELECT VALUE " + "(" * 15 + "c.n" + ")" * 15 + " FROM c"
    assert list(container.query_items(q, partition_key="p")) == [5]
