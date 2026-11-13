"""Queries: JOIN over arrays, EXISTS and ARRAY() subqueries."""

from cosmosdata import FAMILIES, PRODUCTS
from cosmosutil import ids, run


def test_join_over_array_yields_one_row_per_element(products):
    got = run(products, "SELECT p.id, t FROM p JOIN t IN p.tags")
    assert len(got) == sum(len(d["tags"]) for d in PRODUCTS)
    assert sorted((r["id"], r["t"]) for r in got) == sorted((d["id"], t) for d in PRODUCTS for t in d["tags"])


def test_join_rows_follow_array_order_within_a_document(products):
    got = run(products, "SELECT VALUE t FROM p JOIN t IN p.tags WHERE p.id = 'p06'")
    assert got == ["programming", "databases", "sql"]


def test_join_skips_documents_with_empty_or_missing_array(products):
    got = run(products, "SELECT VALUE p.id FROM p JOIN r IN p.reviews")
    assert set(got) == {d["id"] for d in PRODUCTS if d.get("reviews")}
    assert len(got) == sum(len(d.get("reviews", [])) for d in PRODUCTS)


def test_join_with_filter_on_element(products):
    got = run(products, "SELECT VALUE p.id FROM p JOIN r IN p.reviews WHERE r.stars = 5")
    expected = [d["id"] for d in PRODUCTS for r in d.get("reviews", []) if r["stars"] == 5]
    assert sorted(got) == sorted(expected)


def test_join_projects_element_properties(products):
    got = run(products, "SELECT p.id, r.user, r.stars FROM p JOIN r IN p.reviews WHERE p.id = 'p01'")
    assert got == [{"id": "p01", "user": "ann", "stars": 5}, {"id": "p01", "user": "bob", "stars": 4}]


def test_double_join_is_a_cross_product_per_document(products):
    got = run(products, "SELECT p.id, r.user, t FROM p JOIN r IN p.reviews JOIN t IN p.tags WHERE p.id = 'p01'")
    assert len(got) == 4
    assert {(r["user"], r["t"]) for r in got} == {
        (u, t) for u in ("ann", "bob") for t in ("programming", "rust")
    }


def test_join_count_with_aggregate(products):
    got = run(products, "SELECT VALUE COUNT(1) FROM p JOIN r IN p.reviews")
    assert got == [sum(len(d.get("reviews", [])) for d in PRODUCTS)]


def test_join_with_average_in_single_partition(products):
    got = run(products, "SELECT VALUE AVG(r.stars) FROM p JOIN r IN p.reviews WHERE p.category = 'books'", partition_key="books")
    stars = [r["stars"] for d in PRODUCTS if d["category"] == "books" for r in d.get("reviews", [])]
    assert got == [sum(stars) / len(stars)]


def test_join_over_nested_arrays_in_families(families):
    got = run(families, "SELECT VALUE p.givenName FROM f JOIN c IN f.children JOIN p IN c.pets")
    assert sorted(got) == ["Fluffy", "Goofy", "Rex", "Shadow", "Sven"]


def test_join_children_of_families(families):
    got = run(families, "SELECT f.id AS family, c.gender FROM f JOIN c IN f.children WHERE c.grade > 4")
    expected = [
        {"family": f["id"], "gender": c["gender"]} for f in FAMILIES for c in f["children"] if c["grade"] > 4
    ]
    key = lambda r: (r["family"], r["gender"])
    assert sorted(got, key=key) == sorted(expected, key=key)


def test_self_join_in_single_partition(families):
    got = run(families, "SELECT VALUE c.firstName FROM f JOIN c IN f.children WHERE IS_DEFINED(c.firstName)", partition_key="Smith")
    assert sorted(got) == ["Anna", "Ben", "Zed"]


def test_exists_subquery(products):
    q = "SELECT c.id FROM c WHERE EXISTS(SELECT VALUE r FROM r IN c.reviews WHERE r.stars = 2)"
    assert ids(run(products, q)) == ["p04"]


def test_not_exists_subquery(products):
    q = "SELECT c.id FROM c WHERE NOT EXISTS(SELECT VALUE r FROM r IN c.reviews WHERE r.stars >= 4)"
    expected = sorted(d["id"] for d in PRODUCTS if not any(r["stars"] >= 4 for r in d.get("reviews", [])))
    assert ids(run(products, q)) == expected


def test_exists_over_nested_arrays(families):
    q = "SELECT f.id FROM f WHERE EXISTS(SELECT VALUE c FROM c IN f.children WHERE EXISTS(SELECT VALUE p FROM p IN c.pets WHERE p.givenName = 'Rex'))"
    assert ids(run(families, q)) == ["SmithFamily"]


def test_array_subquery_in_projection(products):
    got = run(products, "SELECT c.id, ARRAY(SELECT VALUE r.user FROM r IN c.reviews) AS users FROM c WHERE c.id = 'p01'")
    assert got == [{"id": "p01", "users": ["ann", "bob"]}]


def test_array_subquery_with_filter(products):
    got = run(products, "SELECT VALUE ARRAY(SELECT VALUE r.user FROM r IN c.reviews WHERE r.stars > 4) FROM c WHERE c.id = 'p07'")
    assert got == [["ann", "bob"]]


def test_array_subquery_over_missing_array_is_empty_array(products):
    got = run(products, "SELECT VALUE ARRAY(SELECT VALUE r.user FROM r IN c.reviews) FROM c WHERE c.id = 'p05'")
    assert got == [[]]


def test_array_subquery_pets_in_families(families):
    q = "SELECT VALUE ARRAY(SELECT VALUE p.givenName FROM c IN f.children JOIN p IN c.pets) FROM f WHERE f.id = 'WakefieldFamily'"
    assert run(families, q) == [["Goofy", "Shadow"]]


def test_scalar_subquery_count(products):
    got = run(products, "SELECT VALUE (SELECT VALUE COUNT(1) FROM t IN c.tags) FROM c WHERE c.id = 'p06'")
    assert got == [3]


def test_subquery_in_from_clause(products):
    got = run(products, "SELECT VALUE s.id FROM (SELECT c.id, c.price FROM c WHERE c.price > 1000) s")
    assert got == ["p07"]
