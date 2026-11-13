"""Queries: SELECT forms, WHERE, operators, parameters, undefined semantics."""

import pytest
from azure.cosmos.exceptions import CosmosHttpResponseError, CosmosResourceNotFoundError

from cosmosdata import PRODUCTS
from cosmosutil import ids, run, uid


def want(pred):
    return sorted(d["id"] for d in PRODUCTS if pred(d))


def test_select_star_returns_every_document(products):
    assert ids(run(products, "SELECT * FROM c")) == sorted(d["id"] for d in PRODUCTS)


def test_select_star_results_carry_system_properties(products):
    [doc] = run(products, "SELECT * FROM c WHERE c.id = 'p01'")
    assert doc["name"] == "Rust in Action"
    for key in ("_rid", "_etag", "_ts"):
        assert key in doc


def test_select_star_returns_stored_values_unchanged(products):
    [doc] = run(products, "SELECT * FROM c WHERE c.id = 'p07'")
    src = next(d for d in PRODUCTS if d["id"] == "p07")
    for k, v in src.items():
        assert doc[k] == v


def test_projection_with_alias(products):
    got = run(products, "SELECT c.id, c.name AS title FROM c WHERE c.id = 'p01'")
    assert got == [{"id": "p01", "title": "Rust in Action"}]


def test_projection_of_missing_property_is_omitted(products):
    # p12 has no brand: the property is simply absent from the projected object
    got = run(products, "SELECT c.id, c.brand FROM c WHERE c.id = 'p12'")
    assert got == [{"id": "p12"}]


def test_projection_of_null_property_is_kept_as_null(products):
    got = run(products, "SELECT c.id, c.rating FROM c WHERE c.id = 'p03'")
    assert got == [{"id": "p03", "rating": None}]


def test_projection_only_missing_property_gives_empty_objects(products):
    got = run(products, "SELECT c.brand FROM c WHERE c.id IN ('p12', 'p18')")
    assert got == [{}, {}]


def test_select_value_scalar(products):
    assert run(products, "SELECT VALUE c.name FROM c WHERE c.id = 'p05'") == ["Poetry Collection"]


def test_select_value_expression(products):
    assert run(products, "SELECT VALUE c.stock * 2 FROM c WHERE c.id = 'p01'") == [24]


def test_select_value_missing_property_yields_no_rows(products):
    got = run(products, "SELECT VALUE c.brand FROM c")
    assert sorted(got) == sorted(d["brand"] for d in PRODUCTS if "brand" in d)


def test_select_value_object_and_array_literals(products):
    got = run(products, "SELECT VALUE {'n': c.name, 'p': c.price} FROM c WHERE c.id = 'p05'")
    assert got == [{"n": "Poetry Collection", "p": 9.5}]
    got = run(products, "SELECT VALUE [c.id, c.stock] FROM c WHERE c.id = 'p05'")
    assert got == [["p05", 100]]


def test_select_object_literal_with_alias(products):
    got = run(products, "SELECT {'n': c.name} AS info FROM c WHERE c.id = 'p05'")
    assert got == [{"info": {"n": "Poetry Collection"}}]


def test_select_unaliased_expression_gets_dollar_name(products):
    got = run(products, "SELECT c.stock + 1 FROM c WHERE c.id = 'p05'")
    assert got == [{"$1": 101}]


def test_from_alias_can_be_any_identifier(products):
    for src in ("FROM root r", "FROM products p", "FROM c"):
        alias = src.split()[-1]
        got = run(products, f"SELECT VALUE {alias}.id {src} WHERE {alias}.id = 'p01'")
        assert got == ["p01"]


def test_keywords_are_case_insensitive(products):
    got = run(products, "select value c.id from c where c.id = 'p01' and c.stock > 1")
    assert got == ["p01"]
    got = run(products, "SeLeCt VaLuE c.id FrOm c WhErE c.id = 'p01'")
    assert got == ["p01"]


def test_property_names_are_case_sensitive(products):
    assert run(products, "SELECT VALUE c.id FROM c WHERE c.NAME = 'Shovel'") == []


def test_bracket_property_access(products):
    assert run(products, "SELECT VALUE c['name'] FROM c WHERE c['id'] = 'p14'") == ["Shovel"]


def test_double_quoted_string_literals(products):
    assert run(products, 'SELECT VALUE c.id FROM c WHERE c.name = "Shovel"') == ["p14"]


def test_nested_property_access(products):
    assert run(products, "SELECT VALUE c.specs.dims.w FROM c WHERE c.id = 'p07'") == [30]
    assert run(products, "SELECT VALUE c['specs']['ram_gb'] FROM c WHERE c.id = 'p08'") == [8]


def test_array_index_access(products):
    assert run(products, "SELECT VALUE c.tags[0] FROM c WHERE c.id = 'p01'") == ["programming"]
    assert run(products, "SELECT VALUE c.reviews[1].user FROM c WHERE c.id = 'p01'") == ["bob"]


def test_array_index_out_of_range_is_undefined(products):
    assert run(products, "SELECT VALUE c.tags[5] FROM c WHERE c.id = 'p01'") == []


@pytest.mark.parametrize(
    "where,pred",
    [
        ("c.price = 45", lambda d: d["price"] == 45),
        ("c.price = 45.0", lambda d: d["price"] == 45),
        ("c.price != 45", lambda d: d["price"] != 45),
        ("c.price <> 45", lambda d: d["price"] != 45),
        ("c.price < 20", lambda d: d["price"] < 20),
        ("c.price <= 19.99", lambda d: d["price"] <= 19.99),
        ("c.price > 200", lambda d: d["price"] > 200),
        ("c.price >= 249", lambda d: d["price"] >= 249),
        ("c.stock = 0", lambda d: d["stock"] == 0),
        ("c.name > 'S'", lambda d: d["name"] > "S"),
        ("c.name <= 'Shovel'", lambda d: d["name"] <= "Shovel"),
    ],
)
def test_comparison_operators(products, where, pred):
    assert ids(run(products, f"SELECT c.id FROM c WHERE {where}")) == want(pred)


def test_logical_and_or_not(products):
    assert ids(run(products, "SELECT c.id FROM c WHERE c.stock = 0 AND c.category = 'books'")) == ["p02"]
    assert ids(run(products, "SELECT c.id FROM c WHERE c.stock = 0 OR c.discontinued = true")) == want(
        lambda d: d["stock"] == 0 or d["discontinued"]
    )
    assert ids(run(products, "SELECT c.id FROM c WHERE NOT (c.price < 100)")) == want(lambda d: d["price"] >= 100)


def test_and_binds_tighter_than_or(products):
    q = "SELECT c.id FROM c WHERE c.category = 'toys' OR c.category = 'books' AND c.stock = 0"
    assert ids(run(products, q)) == want(
        lambda d: d["category"] == "toys" or (d["category"] == "books" and d["stock"] == 0)
    )


def test_boolean_property_as_predicate(products):
    assert ids(run(products, "SELECT c.id FROM c WHERE c.discontinued")) == want(lambda d: d["discontinued"])
    assert ids(run(products, "SELECT c.id FROM c WHERE NOT c.discontinued")) == want(lambda d: not d["discontinued"])


def test_arithmetic_operators(products):
    q = "SELECT VALUE [c.stock + 3, c.stock - 3, c.stock * 3, c.stock / 4, c.stock % 5, -c.stock] FROM c WHERE c.id = 'p01'"
    assert run(products, q) == [[15, 9, 36, 3, 2, -12]]


def test_arithmetic_in_where(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE c.price * c.stock > 10000"))
    assert got == want(lambda d: d["price"] * d["stock"] > 10000)


def test_string_concatenation_operator(products):
    q = "SELECT VALUE c.category || '/' || c.id FROM c WHERE c.id = 'p01'"
    assert run(products, q) == ["books/p01"]


def test_in_operator(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE c.category IN ('books', 'toys')"))
    assert got == want(lambda d: d["category"] in ("books", "toys"))


def test_not_in_operator(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE c.category NOT IN ('books', 'toys')"))
    assert got == want(lambda d: d["category"] not in ("books", "toys"))


def test_in_with_numbers(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE c.stock IN (0, 100, 500)"))
    assert got == want(lambda d: d["stock"] in (0, 100, 500))


def test_between_is_inclusive(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE c.price BETWEEN 12 AND 18"))
    assert got == want(lambda d: 12 <= d["price"] <= 18)
    assert "p17" in got and "p14" in got  # 12 and 18 are included


def test_not_between(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE c.price NOT BETWEEN 10 AND 100"))
    assert got == want(lambda d: not (10 <= d["price"] <= 100))


@pytest.mark.parametrize(
    "pattern,pred",
    [
        ("Rust%", lambda n: n.startswith("Rust")),
        ("%Cook%", lambda n: "Cook" in n),
        ("%er", lambda n: n.endswith("er")),
        ("Sh_vel", lambda n: n == "Shovel"),
        ("Kit_", lambda n: n == "Kite"),
    ],
)
def test_like(products, pattern, pred):
    got = ids(run(products, f"SELECT c.id FROM c WHERE c.name LIKE '{pattern}'"))
    assert got == want(lambda d: pred(d["name"]))


def test_not_like(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE c.name NOT LIKE '%o%'"))
    assert got == want(lambda d: "o" not in d["name"])


def test_ternary_operator(products):
    q = "SELECT VALUE (c.price > 50 ? 'high' : 'low') FROM c WHERE c.id IN ('p01', 'p06')"
    assert sorted(run(products, q)) == ["high", "low"]


def test_coalesce_operator_on_missing_property(products):
    got = run(products, "SELECT VALUE (c.brand ?? 'none') FROM c WHERE c.id IN ('p12', 'p01')")
    assert sorted(got) == ["Manning", "none"]


def test_iif_function(products):
    got = run(products, "SELECT VALUE IIF(c.stock > 0, 'in', 'out') FROM c WHERE c.id IN ('p01', 'p02')")
    assert sorted(got) == ["in", "out"]


def test_comparison_with_missing_property_is_false(products):
    # p12 and p18 have no brand: neither = nor != selects them
    eq = ids(run(products, "SELECT c.id FROM c WHERE c.brand = 'Acme'"))
    ne = ids(run(products, "SELECT c.id FROM c WHERE c.brand != 'Acme'"))
    assert eq == want(lambda d: d.get("brand") == "Acme")
    assert ne == want(lambda d: "brand" in d and d["brand"] != "Acme")
    assert "p12" not in eq + ne and "p18" not in eq + ne


def test_comparison_with_null_property(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE c.rating = null"))
    assert got == want(lambda d: "rating" in d and d["rating"] is None)


def test_where_on_nonexistent_property_matches_nothing(products):
    assert run(products, "SELECT * FROM c WHERE c.no_such_property = 1") == []


def test_parameters_of_every_json_type(products):
    cases = [
        ("c.category = @v", "books", want(lambda d: d["category"] == "books")),
        ("c.stock = @v", 0, want(lambda d: d["stock"] == 0)),
        ("c.price < @v", 10.0, want(lambda d: d["price"] < 10)),
        ("c.discontinued = @v", True, want(lambda d: d["discontinued"])),
        ("c.rating = @v", None, want(lambda d: "rating" in d and d["rating"] is None)),
    ]
    for where, value, expected in cases:
        got = run(products, f"SELECT c.id FROM c WHERE {where}", parameters=[{"name": "@v", "value": value}])
        assert ids(got) == expected, where


def test_multiple_parameters(products):
    got = run(
        products,
        "SELECT c.id FROM c WHERE c.category = @cat AND c.price > @min",
        parameters=[{"name": "@cat", "value": "garden"}, {"name": "@min", "value": 20}],
    )
    assert ids(got) == want(lambda d: d["category"] == "garden" and d["price"] > 20)


def test_array_parameter_in_in_clause(products):
    got = run(
        products,
        "SELECT c.id FROM c WHERE ARRAY_CONTAINS(@ids, c.id)",
        parameters=[{"name": "@ids", "value": ["p01", "p02", "nope"]}],
    )
    assert ids(got) == ["p01", "p02"]


def test_object_parameter_value(products):
    got = run(
        products,
        "SELECT VALUE @o.a FROM c WHERE c.id = 'p01'",
        parameters=[{"name": "@o", "value": {"a": 5}}],
    )
    assert got == [5]


def test_top_limits_rows(products):
    assert len(run(products, "SELECT TOP 5 * FROM c")) == 5
    assert len(run(products, "SELECT TOP 100 * FROM c")) == 24
    assert run(products, "SELECT TOP 0 * FROM c") == []


def test_system_timestamp_comparison(products):
    assert len(run(products, "SELECT c.id FROM c WHERE c._ts > 0")) == 24
    assert run(products, "SELECT c.id FROM c WHERE c._ts > 99999999999") == []


def test_lookup_by_id(products):
    assert run(products, "SELECT VALUE c.name FROM c WHERE c.id = 'p21'") == ["Robot Kit"]
    assert run(products, "SELECT VALUE c.name FROM c WHERE c.id = 'no-such-id'") == []


def test_single_partition_query_with_partition_key(products):
    got = run(products, "SELECT c.id FROM c", partition_key="garden")
    assert ids(got) == want(lambda d: d["category"] == "garden")


def test_single_partition_query_with_filter(products):
    got = run(products, "SELECT c.id FROM c WHERE c.price > 20", partition_key="toys")
    assert ids(got) == want(lambda d: d["category"] == "toys" and d["price"] > 20)


def test_single_partition_query_other_partition_key_is_empty(products):
    assert run(products, "SELECT * FROM c", partition_key="no-such-category") == []


@pytest.mark.not_on_vnext(reason="vnext does not enforce enable_cross_partition_query")
def test_cross_partition_query_without_flag_is_400(products):
    # No partition key and enable_cross_partition_query left unset: the service refuses
    # (Microsoft's test_partition_key expects a 400 for this).
    with pytest.raises(CosmosHttpResponseError) as e:
        list(products.query_items("SELECT * FROM c"))
    assert e.value.status_code == 400


def test_invalid_query_is_400(products):
    with pytest.raises(CosmosHttpResponseError) as e:
        run(products, "SELEKT * FROM c")
    assert e.value.status_code == 400


def test_query_with_unknown_function_is_400(products):
    with pytest.raises(CosmosHttpResponseError) as e:
        run(products, "SELECT NO_SUCH_FUNCTION(c.id) FROM c")
    assert e.value.status_code == 400


def test_query_missing_container_is_404(db):
    ghost = db.get_container_client(uid("ghost"))
    with pytest.raises(CosmosResourceNotFoundError) as e:
        list(ghost.query_items("SELECT * FROM c", enable_cross_partition_query=True))
    assert e.value.status_code == 404


def test_query_empty_container_returns_nothing(container):
    assert run(container, "SELECT * FROM c") == []
