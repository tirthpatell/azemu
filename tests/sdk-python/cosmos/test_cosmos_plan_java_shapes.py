"""Query plans for the Java SDK and GROUP BY with VALUE.

The Java SDK (`azure-cosmos` 4.83) sends query version 1.0 and the features `NonValueAggregate, CompositeAggregate,
MultipleAggregates, ...`. Its pipeline (`AggregateDocumentQueryExecutionContext`, `SingleGroupAggregator`,
`GroupByDocumentQueryExecutionContext`) reads:
* a select list of aggregates without GROUP BY: each partition answers one row `{"payload": {alias: {"item": ..}}}` and the plan
  names the aliases (`groupByAliases`) and their operators (`groupByAliasToAggregateType`); `hasSelectValue` is false;
* `SELECT VALUE <aggregate> ... GROUP BY`: the operator in `aggregates`, the partial in a one element array under `payload`
  (the JavaScript SDK reads `aggregates[0]` and `payload[0]` too).
Not recorded; taken from those SDKs' sources (see docs/sdk-notes.md).
"""

import pytest

from cosmosutil import RawCosmos, uid

PK = "x-ms-documentdb-partitionkey"

JAVA_PLAN = {
    "content-type": "application/query+json",
    "x-ms-cosmos-is-query-plan-request": "True",
    "x-ms-cosmos-query-version": "1.0",
    "x-ms-cosmos-supported-query-features": "Aggregate, CompositeAggregate, MultipleOrderBy, MultipleAggregates, OrderBy, "
    "OffsetAndLimit, Distinct, GroupBy, Top, DCount, NonValueAggregate, NonStreamingOrderBy",
}
PY_PLAN = {
    "content-type": "application/query+json",
    "x-ms-cosmos-is-query-plan-request": "True",
    "x-ms-cosmos-query-version": "1.0",
    "x-ms-cosmos-supported-query-features": "Aggregate,CompositeAggregate,Distinct,MultipleOrderBy,OffsetAndLimit,OrderBy,Top,GroupBy",
}


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


def _fill(raw, db, coll):
    for i, (pk, n) in enumerate([("a", 5), ("a", 3), ("b", 9)]):
        r = raw.post(f"dbs/{db}/colls/{coll}/docs", body={"id": f"d{i}", "pk": pk, "n": n}, headers={PK: f'["{pk}"]'})
        assert r.status_code == 201


def test_select_list_of_aggregates_is_planned_as_one_group_for_a_client_with_nonvalueaggregate(raw, rcoll):
    db, coll = rcoll
    _fill(raw, db, coll)
    sql = "SELECT COUNT(1) AS n, MIN(c.n) AS lo, MAX(c.n) AS hi, SUM(c.n) AS total FROM c"
    info = _plan(raw, db, coll, sql, JAVA_PLAN)
    assert info["hasSelectValue"] is False
    assert info["groupByExpressions"] == []
    assert info["groupByAliases"] == ["n", "lo", "hi", "total"]
    assert info["groupByAliasToAggregateType"] == {"n": "Count", "lo": "Min", "hi": "Max", "total": "Sum"}
    assert info["aggregates"] == ["Count", "Min", "Max", "Sum"]
    # one row per partition with a payload; MIN and MAX in the composite form only for a client that takes it (version 1.4)
    assert _run(raw, db, coll, info["rewrittenQuery"]) == [
        {"payload": {"n": {"item": 3}, "lo": {"item": 3}, "hi": {"item": 9}, "total": {"item": 17}}}
    ]
    # an empty partition still answers one row: COUNT is 0, the others have no item
    empty = _plan(raw, db, coll, sql + " WHERE c.n > 100", JAVA_PLAN)
    assert _run(raw, db, coll, empty["rewrittenQuery"]) == [{"payload": {"n": {"item": 0}, "lo": {}, "hi": {}, "total": {}}}]


def test_unnamed_aggregate_in_a_select_list_is_named_dollar_one(raw, rcoll):
    db, coll = rcoll
    _fill(raw, db, coll)
    info = _plan(raw, db, coll, "SELECT COUNT(1) FROM c", JAVA_PLAN)
    assert info["groupByAliases"] == ["$1"] and info["groupByAliasToAggregateType"] == {"$1": "Count"}
    assert _run(raw, db, coll, info["rewrittenQuery"]) == [{"payload": {"$1": {"item": 3}}}]


def test_select_list_of_aggregates_keeps_the_old_form_for_a_client_without_the_feature(raw, rcoll):
    db, coll = rcoll
    _fill(raw, db, coll)
    info = _plan(raw, db, coll, "SELECT COUNT(1) AS n, SUM(c.n) AS s FROM c", PY_PLAN)
    assert info["groupByAliases"] == [] and info["groupByAliasToAggregateType"] == {}
    assert _run(raw, db, coll, info["rewrittenQuery"]) == [[{"item": 3}, {"item": 17}]]


@pytest.mark.parametrize(
    "agg,partial",
    [
        ("COUNT(1)", {"item": 2}),
        ("SUM(c.n)", {"item": 8}),
        ("AVG(c.n)", {"item": {"sum": 8, "count": 2}}),
        ("MAX(c.n)", {"item": 5}),
    ],
)
def test_value_aggregate_with_group_by_names_its_operator_and_wraps_each_partial_in_an_array(raw, rcoll, agg, partial):
    db, coll = rcoll
    _fill(raw, db, coll)
    info = _plan(raw, db, coll, f"SELECT VALUE {agg} FROM c GROUP BY c.pk", JAVA_PLAN)
    assert info["hasSelectValue"] is True
    assert len(info["aggregates"]) == 1
    assert info["groupByAliases"] == ["$1"]
    rows = _run(raw, db, coll, info["rewrittenQuery"])
    mine = [r for r in rows if r["groupByItems"] == [{"item": "a"}]]
    assert mine == [{"groupByItems": [{"item": "a"}], "payload": [partial]}]


def test_value_without_aggregate_with_group_by_stays_bare(raw, rcoll):
    db, coll = rcoll
    _fill(raw, db, coll)
    info = _plan(raw, db, coll, "SELECT VALUE c.pk FROM c GROUP BY c.pk", JAVA_PLAN)
    assert info["aggregates"] == []
    rows = _run(raw, db, coll, info["rewrittenQuery"])
    assert sorted(r["payload"] for r in rows) == ["a", "b"]


def test_ordered_distinct_rows_carry_the_rid_unless_top_or_offset_limit_needs_the_partition_to_remove_repeats(raw, rcoll):
    # The Java SDK builds the continuation token of an ORDER BY page from the `_rid` of the last row ("rid must not be null").
    db, coll = rcoll
    _fill(raw, db, coll)
    info = _plan(raw, db, coll, "SELECT DISTINCT VALUE c.pk FROM c ORDER BY c.pk", JAVA_PLAN)
    assert info["distinctType"] == "Ordered"
    rows = _run(raw, db, coll, info["rewrittenQuery"].replace("{documentdb-formattableorderbyquery-filter}", "true"))
    assert len(rows) == 3 and all(r["_rid"] and r["payload"] in ("a", "b") for r in rows)
    top = _plan(raw, db, coll, "SELECT DISTINCT TOP 2 VALUE c.pk FROM c ORDER BY c.pk", JAVA_PLAN)
    rows = _run(raw, db, coll, top["rewrittenQuery"].replace("{documentdb-formattableorderbyquery-filter}", "true"))
    assert [r["payload"] for r in rows] == ["a", "b"]


def test_is_boolean_is_accepted_as_the_java_sdk_writes_it_in_order_by_resume_filters(raw, rcoll):
    # Documented name: IS_BOOL. The Java SDK sends IS_BOOLEAN when it resumes a descending ORDER BY from a continuation token.
    db, coll = rcoll
    _fill(raw, db, coll)
    rows = _run(raw, db, coll, "SELECT VALUE c.id FROM c WHERE IS_BOOLEAN(c.n) OR IS_NUMBER(c.n)")
    assert sorted(rows) == ["d0", "d1", "d2"]
