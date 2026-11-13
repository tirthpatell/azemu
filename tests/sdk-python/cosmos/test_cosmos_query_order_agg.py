"""Queries: ORDER BY, TOP, OFFSET LIMIT, DISTINCT, aggregates, GROUP BY."""

import pytest
from azure.cosmos.exceptions import CosmosHttpResponseError

from cosmosdata import CATEGORIES, PRODUCTS
from cosmosutil import idlist, ids, run

BY_PRICE = sorted(PRODUCTS, key=lambda d: d["price"])
PRICES = [d["price"] for d in PRODUCTS]


# ---- ORDER BY --------------------------------------------------------------
def test_order_by_number_ascending_across_partitions(products):
    assert idlist(run(products, "SELECT * FROM c ORDER BY c.price")) == [d["id"] for d in BY_PRICE]


def test_order_by_number_descending_across_partitions(products):
    got = idlist(run(products, "SELECT * FROM c ORDER BY c.price DESC"))
    assert got == [d["id"] for d in reversed(BY_PRICE)]


def test_order_by_explicit_asc(products):
    assert idlist(run(products, "SELECT c.id FROM c ORDER BY c.price ASC")) == [d["id"] for d in BY_PRICE]


def test_order_by_string(products):
    got = run(products, "SELECT VALUE c.name FROM c ORDER BY c.name")
    assert got == sorted(d["name"] for d in PRODUCTS)


def test_order_by_integer_property(products):
    got = run(products, "SELECT VALUE c.stock FROM c ORDER BY c.stock DESC")
    assert got == sorted((d["stock"] for d in PRODUCTS), reverse=True)


def test_order_by_nested_property(products):
    got = run(products, "SELECT VALUE c.specs.ram_gb FROM c WHERE IS_DEFINED(c.specs.ram_gb) ORDER BY c.specs.ram_gb")
    assert got == [1, 8, 16]


def test_order_by_within_single_partition(products):
    got = idlist(run(products, "SELECT c.id FROM c ORDER BY c.price DESC", partition_key="toys"))
    assert got == [d["id"] for d in reversed(BY_PRICE) if d["category"] == "toys"]


def test_order_by_with_where(products):
    got = idlist(run(products, "SELECT c.id FROM c WHERE c.stock > 40 ORDER BY c.stock"))
    assert got == [d["id"] for d in sorted(PRODUCTS, key=lambda d: d["stock"]) if d["stock"] > 40]


def test_order_by_system_timestamp_runs(products):
    got = run(products, "SELECT c._ts FROM c ORDER BY c._ts")
    assert [r["_ts"] for r in got] == sorted(r["_ts"] for r in got)
    assert len(got) == 24


def test_order_by_multiple_fields_with_composite_index(make_container):
    c = make_container(
        "/category",
        indexing_policy={
            "compositeIndexes": [
                [{"path": "/category", "order": "ascending"}, {"path": "/price", "order": "descending"}]
            ]
        },
    )
    for d in PRODUCTS:
        c.create_item(d)
    got = idlist(run(c, "SELECT c.id FROM c ORDER BY c.category ASC, c.price DESC"))
    expected = [d["id"] for d in sorted(PRODUCTS, key=lambda d: (d["category"], -d["price"]))]
    assert got == expected


@pytest.mark.not_on_vnext(reason="vnext does not enforce the composite index requirement")
def test_order_by_multiple_fields_without_composite_index_is_400(products):
    with pytest.raises(CosmosHttpResponseError) as e:
        run(products, "SELECT c.id FROM c ORDER BY c.category, c.price")
    assert e.value.status_code == 400


# ---- TOP and OFFSET LIMIT --------------------------------------------------
def test_top_with_order_by(products):
    got = idlist(run(products, "SELECT TOP 3 c.id FROM c ORDER BY c.price DESC"))
    assert got == [d["id"] for d in reversed(BY_PRICE)][:3]


def test_offset_limit_with_order_by(products):
    got = idlist(run(products, "SELECT c.id FROM c ORDER BY c.price OFFSET 2 LIMIT 3"))
    assert got == [d["id"] for d in BY_PRICE][2:5]


def test_offset_beyond_end_is_empty(products):
    assert run(products, "SELECT c.id FROM c ORDER BY c.price OFFSET 100 LIMIT 5") == []


def test_limit_larger_than_remaining(products):
    got = idlist(run(products, "SELECT c.id FROM c ORDER BY c.price OFFSET 20 LIMIT 50"))
    assert got == [d["id"] for d in BY_PRICE][20:]


def test_offset_limit_in_single_partition(products):
    got = idlist(run(products, "SELECT c.id FROM c ORDER BY c.price OFFSET 1 LIMIT 2", partition_key="books"))
    assert got == [d["id"] for d in BY_PRICE if d["category"] == "books"][1:3]


def test_offset_limit_without_order_by_returns_requested_count(products):
    got = run(products, "SELECT c.id FROM c OFFSET 5 LIMIT 4")
    assert len(got) == 4 and len(set(idlist(got))) == 4


# ---- DISTINCT --------------------------------------------------------------
def test_distinct_value(products):
    got = run(products, "SELECT DISTINCT VALUE c.category FROM c")
    assert sorted(got) == CATEGORIES


def test_distinct_object(products):
    got = run(products, "SELECT DISTINCT c.category FROM c")
    assert sorted(r["category"] for r in got) == CATEGORIES
    assert len(got) == 4


def test_distinct_skips_missing_values(products):
    got = run(products, "SELECT DISTINCT VALUE c.brand FROM c")
    assert sorted(got) == sorted({d["brand"] for d in PRODUCTS if "brand" in d})


def test_distinct_two_properties(products):
    got = run(products, "SELECT DISTINCT c.category, c.discontinued FROM c")
    assert {(r["category"], r["discontinued"]) for r in got} == {(d["category"], d["discontinued"]) for d in PRODUCTS}
    assert len(got) == len({(d["category"], d["discontinued"]) for d in PRODUCTS})


def test_distinct_with_order_by(products):
    got = run(products, "SELECT DISTINCT VALUE c.category FROM c ORDER BY c.category DESC")
    assert got == sorted(CATEGORIES, reverse=True)


def test_distinct_in_single_partition(products):
    got = run(products, "SELECT DISTINCT VALUE c.brand FROM c", partition_key="garden")
    assert sorted(got) == sorted({d["brand"] for d in PRODUCTS if d["category"] == "garden" and "brand" in d})


def test_distinct_with_top(products):
    got = run(products, "SELECT DISTINCT TOP 2 VALUE c.category FROM c")
    assert len(got) == 2 and set(got) <= set(CATEGORIES)


# ---- aggregates ------------------------------------------------------------
def test_count_star_value(products):
    assert run(products, "SELECT VALUE COUNT(1) FROM c") == [24]


def test_count_with_filter(products):
    got = run(products, "SELECT VALUE COUNT(1) FROM c WHERE c.price > 100")
    assert got == [sum(1 for p in PRICES if p > 100)]


def test_count_of_nothing_is_zero(products):
    assert run(products, "SELECT VALUE COUNT(1) FROM c WHERE c.price > 1000000") == [0]


def test_count_on_empty_container(container):
    assert run(container, "SELECT VALUE COUNT(1) FROM c") == [0]


def test_sum_of_integers_is_exact(products):
    assert run(products, "SELECT VALUE SUM(c.stock) FROM c") == [sum(d["stock"] for d in PRODUCTS)]


def test_sum_of_floats(products):
    [got] = run(products, "SELECT VALUE SUM(c.price) FROM c")
    assert got == pytest.approx(sum(PRICES))


def test_avg(products):
    [got] = run(products, "SELECT VALUE AVG(c.price) FROM c")
    assert got == pytest.approx(sum(PRICES) / len(PRICES))


def test_min_and_max_numbers(products):
    assert run(products, "SELECT VALUE MIN(c.price) FROM c") == [min(PRICES)]
    assert run(products, "SELECT VALUE MAX(c.price) FROM c") == [max(PRICES)]


def test_min_and_max_strings(products):
    assert run(products, "SELECT VALUE MIN(c.name) FROM c") == [min(d["name"] for d in PRODUCTS)]
    assert run(products, "SELECT VALUE MAX(c.name) FROM c") == [max(d["name"] for d in PRODUCTS)]


def test_sum_over_numbers_only(products):
    got = run(products, "SELECT VALUE SUM(c.rating) FROM c WHERE IS_NUMBER(c.rating)")
    assert got == [pytest.approx(sum(d["rating"] for d in PRODUCTS if isinstance(d.get("rating"), (int, float))))]


@pytest.mark.not_on_vnext(reason="vnext runs the aggregate itself and returns no row; real Cosmos goes through the SDK's merge, which yields one None")
def test_sum_with_a_null_value_is_none_across_partitions(products):
    # SUM/AVG over values that include non-numbers (null, strings) are undefined.
    # Across partitions the SDK merges the partial results itself and returns a single
    # None (Microsoft test_aggregate expects ['SUM', None] for this case).
    assert run(products, "SELECT VALUE SUM(c.rating) FROM c") == [None]
    assert run(products, "SELECT VALUE AVG(c.rating) FROM c") == [None]


def test_aggregate_in_single_partition(products):
    got = run(products, "SELECT VALUE SUM(c.stock) FROM c", partition_key="books")
    assert got == [sum(d["stock"] for d in PRODUCTS if d["category"] == "books")]


def test_single_partition_aggregate_without_value_returns_object(products):
    got = run(products, "SELECT COUNT(1) AS n, MAX(c.price) AS hi FROM c", partition_key="books")
    assert got == [{"n": 6, "hi": 55}]


def test_single_partition_unaliased_aggregate_is_named_dollar_1(products):
    assert run(products, "SELECT COUNT(1) FROM c", partition_key="books") == [{"$1": 6}]


@pytest.mark.not_on_vnext(reason="real service refuses non-VALUE aggregates across partitions; vnext answers")
def test_cross_partition_aggregate_without_value_is_400(products):
    # Microsoft test_unsupported_queries
    with pytest.raises(CosmosHttpResponseError) as e:
        run(products, "SELECT COUNT(1) FROM c")
    assert e.value.status_code == 400


def test_aggregate_with_top_clause(products):
    assert run(products, "SELECT TOP 1 VALUE COUNT(1) FROM c") == [24]


# ---- GROUP BY (single partition: the Python SDK has no client-side GROUP BY merge) ----
@pytest.fixture
def grouped(make_container):
    c = make_container("/pk")
    rows = [
        ("a", "x", 1), ("a", "x", 2), ("a", "y", 3), ("a", "y", 4), ("a", "y", 5), ("a", "z", 6),
    ]
    for i, (g, h, n) in enumerate(rows):
        c.create_item({"id": f"g{i}", "pk": "k", "g": g, "h": h, "n": n})
    return c


def test_group_by_single_key_count(grouped):
    got = run(grouped, "SELECT c.h, COUNT(1) AS cnt FROM c GROUP BY c.h", partition_key="k")
    assert sorted((r["h"], r["cnt"]) for r in got) == [("x", 2), ("y", 3), ("z", 1)]


def test_group_by_with_several_aggregates(grouped):
    got = run(
        grouped,
        "SELECT c.h, COUNT(1) AS cnt, SUM(c.n) AS total, MIN(c.n) AS lo, MAX(c.n) AS hi, AVG(c.n) AS mean "
        "FROM c GROUP BY c.h",
        partition_key="k",
    )
    by = {r["h"]: r for r in got}
    assert by["x"] == {"h": "x", "cnt": 2, "total": 3, "lo": 1, "hi": 2, "mean": 1.5}
    assert by["y"] == {"h": "y", "cnt": 3, "total": 12, "lo": 3, "hi": 5, "mean": 4}
    assert by["z"]["cnt"] == 1


def test_group_by_multiple_keys(grouped):
    got = run(grouped, "SELECT c.g, c.h, COUNT(1) AS cnt FROM c GROUP BY c.g, c.h", partition_key="k")
    assert sorted((r["g"], r["h"], r["cnt"]) for r in got) == [("a", "x", 2), ("a", "y", 3), ("a", "z", 1)]


def test_group_by_with_where(grouped):
    got = run(grouped, "SELECT c.h, COUNT(1) AS cnt FROM c WHERE c.n > 2 GROUP BY c.h", partition_key="k")
    assert sorted((r["h"], r["cnt"]) for r in got) == [("y", 3), ("z", 1)]


def test_group_by_select_value_aggregate(grouped):
    got = run(grouped, "SELECT VALUE COUNT(1) FROM c GROUP BY c.h", partition_key="k")
    assert sorted(got) == [1, 2, 3]


def test_group_by_on_products_category_single_partition(products):
    got = run(products, "SELECT c.brand, COUNT(1) AS cnt FROM c GROUP BY c.brand", partition_key="books")
    expected = {}
    for d in PRODUCTS:
        if d["category"] == "books":
            expected[d.get("brand")] = expected.get(d.get("brand"), 0) + 1
    assert {r.get("brand"): r["cnt"] for r in got} == expected
