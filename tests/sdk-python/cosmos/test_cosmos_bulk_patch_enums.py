"""Raw REST: the bulk (non-atomic) batch, a patch with a bare array, enum strings in any case.

What the JavaScript and Go SDKs send; not recorded (see docs/services/cosmos.md).
"""

import pytest

from cosmosutil import RawCosmos, uid

PK = "x-ms-documentdb-partitionkey"


@pytest.fixture
def raw(endpoint, ca_file):
    return RawCosmos(endpoint, ca_file)


@pytest.fixture
def rcoll(raw):
    db, coll = uid("db"), uid("c")
    assert raw.post("dbs", body={"id": db}).status_code == 201
    r = raw.post(f"dbs/{db}/colls", body={"id": coll, "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2}})
    assert r.status_code == 201, r.text
    return db, coll


def test_bulk_batch_each_operation_has_its_own_partition_key(raw, rcoll):
    db, coll = rcoll
    path = f"dbs/{db}/colls/{coll}/docs"
    ops = [
        {"operationType": "Create", "partitionKey": '["p1"]', "resourceBody": {"id": "a", "pk": "p1", "v": 1}},
        {"operationType": "Create", "partitionKey": '["p2"]', "resourceBody": {"id": "a", "pk": "p2", "v": 2}},
        {"operationType": "Create", "partitionKey": '["p1"]', "resourceBody": {"id": "a", "pk": "p1"}},
        {"operationType": "Read", "id": "a", "partitionKey": '["p2"]'},
        {"operationType": "Delete", "id": "a", "partitionKey": '["p1"]'},
    ]
    h = {
        "x-ms-cosmos-is-batch-request": "True",
        "x-ms-cosmos-batch-atomic": "False",
        "x-ms-cosmos-batch-continue-on-error": "True",
        "x-ms-documentdb-partitionkeyrangeid": "0",
    }
    r = raw.post(path, body=ops, headers=h)
    assert r.status_code == 207, r.text
    got = r.json()
    assert [x["statusCode"] for x in got] == [201, 201, 409, 200, 204]
    assert got[3]["resourceBody"]["v"] == 2 and got[0]["eTag"]
    assert raw.get(f"{path}/a", headers={PK: '["p2"]'}).status_code == 200
    assert raw.get(f"{path}/a", headers={PK: '["p1"]'}).status_code == 404
    # nothing failed: 200
    ok = [{"operationType": "Create", "partitionKey": '["q"]', "resourceBody": {"id": "z", "pk": "q"}}]
    assert raw.post(path, body=ok, headers=h).status_code == 200


def test_patch_body_can_be_the_bare_array(raw, rcoll):
    db, coll = rcoll
    path = f"dbs/{db}/colls/{coll}/docs"
    assert raw.post(path, body={"id": "d", "pk": "k", "n": 1}, headers={PK: '["k"]'}).status_code == 201
    r = raw.request("PATCH", f"{path}/d", body=[{"op": "incr", "path": "/n", "value": 4}], headers={PK: '["k"]'})
    assert r.status_code == 200, r.text
    assert r.json()["n"] == 5


def test_enum_strings_are_accepted_in_any_case(raw):
    db = uid("db")
    assert raw.post("dbs", body={"id": db}).status_code == 201
    body = {
        "id": uid("c"),
        "partitionKey": {"paths": ["/pk"], "kind": "hash", "version": 2},
        "indexingPolicy": {"indexingMode": "Consistent"},
    }
    r = raw.post(f"dbs/{db}/colls", body=body)
    assert r.status_code == 201, r.text
    assert r.json()["indexingPolicy"]["indexingMode"] == "consistent"
    body = {"id": uid("c"), "partitionKey": {"paths": ["/pk"]}, "indexingPolicy": {"indexingMode": "None"}}
    assert raw.post(f"dbs/{db}/colls", body=body).status_code == 201


# --- the query plan answers in the form the asking SDK reads ---------------------------------------------------

JS_PLAN = {
    "content-type": "application/query+json",
    "x-ms-cosmos-is-query-plan-request": "True",
    "x-ms-cosmos-query-version": "1.4",
    "x-ms-cosmos-supported-query-features": "NonValueAggregate,Aggregate,Distinct,MultipleOrderBy,OffsetAndLimit,OrderBy,Top,CompositeAggregate,GroupBy,MultipleAggregates",
}
PY_PLAN = {
    "content-type": "application/query+json",
    "x-ms-cosmos-is-query-plan-request": "True",
    "x-ms-cosmos-query-version": "1.0",
    "x-ms-cosmos-supported-query-features": "Aggregate,CompositeAggregate,Distinct,MultipleOrderBy,OffsetAndLimit,OrderBy,Top,GroupBy",
}


def _plan(raw, db, coll, sql, headers):
    r = raw.post(f"dbs/{db}/colls/{coll}/docs", body={"query": sql}, headers=headers)
    assert r.status_code == 200, r.text
    return r.json()["queryInfo"]


def _run(raw, db, coll, sql):
    h = {
        "content-type": "application/query+json",
        "x-ms-documentdb-isquery": "True",
        "x-ms-documentdb-query-enablecrosspartition": "True",
        "x-ms-documentdb-partitionkeyrangeid": "0",
    }
    r = raw.post(f"dbs/{db}/colls/{coll}/docs", body={"query": sql}, headers=h)
    assert r.status_code == 200, r.text
    return r.json()["Documents"]


def test_min_max_plan_is_composite_for_a_client_that_reads_it_and_bare_for_the_others(raw, rcoll):
    db, coll = rcoll
    path = f"dbs/{db}/colls/{coll}/docs"
    for i, n in enumerate([5, 3, 9]):
        assert raw.post(path, body={"id": f"d{i}", "pk": f"k{i}", "n": n}, headers={PK: f'["k{i}"]'}).status_code == 201
    sql = "SELECT VALUE MIN(c.n) FROM c"
    js = _plan(raw, db, coll, sql, JS_PLAN)
    assert js["aggregates"] == ["Min"] and js["hasSelectValue"] is True
    assert '"min": MIN(c.n)' in js["rewrittenQuery"] and '"count": COUNT(c.n)' in js["rewrittenQuery"]
    assert _run(raw, db, coll, js["rewrittenQuery"]) == [[{"item": {"min": 3, "count": 3}}]]
    # a partition range with no matching value: count 0 and no min, which the JavaScript aggregator skips
    none = _plan(raw, db, coll, "SELECT VALUE MAX(c.n) FROM c WHERE c.n > 100", JS_PLAN)
    assert _run(raw, db, coll, none["rewrittenQuery"]) == [[{"item": {"count": 0}}]]
    # the Python SDK (query version 1.0) and a client that sends nothing get the bare value
    py = _plan(raw, db, coll, sql, PY_PLAN)
    assert _run(raw, db, coll, py["rewrittenQuery"]) == [[{"item": 3}]]
    assert _plan(raw, db, coll, sql, {"content-type": "application/query+json", "x-ms-cosmos-is-query-plan-request": "True"})["rewrittenQuery"] == py["rewrittenQuery"]
    # COUNT, SUM and AVG are the same for both
    for agg, want in [("COUNT(1)", [[{"item": 3}]]), ("SUM(c.n)", [[{"item": 17}]]), ("AVG(c.n)", [[{"item": {"sum": 17, "count": 3}}]])]:
        sql = f"SELECT VALUE {agg} FROM c"
        assert _plan(raw, db, coll, sql, JS_PLAN)["rewrittenQuery"] == _plan(raw, db, coll, sql, PY_PLAN)["rewrittenQuery"]
        assert _run(raw, db, coll, _plan(raw, db, coll, sql, JS_PLAN)["rewrittenQuery"]) == want
