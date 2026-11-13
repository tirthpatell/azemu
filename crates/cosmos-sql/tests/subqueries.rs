// Some code samples and query examples in this file are reproduced from Microsoft Learn and from Microsoft's SDKs (MIT licence, Copyright Microsoft); see NOTICE and LICENSES/MIT-Microsoft.txt.
//! Subqueries: JOIN on subqueries, scalar, EXISTS, ARRAY. Examples from
//! learn.microsoft.com/azure/cosmos-db/nosql/query/subquery (data sets completed by us).

mod common;
use common::*;
use serde_json::{json, Value};

fn catalog() -> Vec<Value> {
    vec![
        json!({"id": "00000000-0000-0000-0000-000000004041", "name": "Remdriel Shoes", "price": 15,
               "tags": [{"key": "fabric", "value": "leather", "description": "Leather"}, {"key": "volume", "value": "68-gal", "description": "6.8 Gal"}],
               "sizes": [{"key": "5", "description": "Small", "order": 1}, {"key": "6", "description": "Medium", "order": 3}, {"key": "9", "description": "Large", "order": 4}],
               "colors": ["gray", "darkgray", "blue"]}),
        json!({"id": "00000000-0000-0000-0000-000000004322", "name": "Tirevy trunks", "price": 10,
               "tags": [{"key": "material", "value": "polyester", "description": "Polyester"}],
               "sizes": [{"key": "7", "description": "Small", "order": 0}],
               "colors": ["cobalt", "turquoise"]}),
        json!({"id": "00000000-0000-0000-0000-000000004055", "name": "Pricey Parka", "price": 20,
               "tags": [], "sizes": [{"key": "8", "description": "Large", "order": 9}], "colors": []}),
    ]
}

#[test]
fn join_with_subqueries() {
    let c = catalog();
    check_table(&c, &[
        // plain joins
        (r#"SELECT VALUE COUNT(1) FROM products p JOIN t in p.tags JOIN s in p.sizes JOIN c in p.colors WHERE t.key IN ("fabric", "material") AND s["order"] >= 3 AND c LIKE "%gray%""#, "[4]"),
        // the same with multi-value subqueries (no alias: generated)
        (r#"SELECT VALUE COUNT(1) FROM products p JOIN (SELECT VALUE t FROM t IN p.tags WHERE t.key IN ("fabric", "material")) JOIN (SELECT VALUE s FROM s IN p.sizes WHERE s["order"] >= 3) JOIN (SELECT VALUE c FROM c in p.colors WHERE c LIKE "%gray%")"#, "[4]"),
        // evaluate once, reference many times
        ("SELECT VALUE {subtotal: p.price, total: (p.price * 1.25)} FROM products p WHERE (p.price * 1.25) < 22.25", r#"[{"subtotal":15,"total":18.75},{"subtotal":10,"total":12.5}]"#),
        ("SELECT VALUE {subtotal: p.price, total: totalPrice} FROM products p JOIN (SELECT VALUE p.price * 1.25) totalPrice WHERE totalPrice < 22.25", r#"[{"subtotal":15,"total":18.75},{"subtotal":10,"total":12.5}]"#),
        ("SELECT p.id, totalPrice FROM products p JOIN (SELECT VALUE p.price * 1.25) AS totalPrice WHERE p.price = 20", r#"[{"id":"00000000-0000-0000-0000-000000004055","totalPrice":25}]"#),
        // a subquery whose value is undefined produces no row
        ("SELECT VALUE p.name FROM products p JOIN (SELECT VALUE p.nothing) x", "[]"),
        ("SELECT VALUE p.name FROM products p JOIN (SELECT p.nothing) x WHERE p.price = 10", r#"["Tirevy trunks"]"#),
        // mimic a relational join with reference data
        ("SELECT p.name, s.description AS size, m.length, m.unit FROM products p JOIN s IN p.sizes JOIN m IN (SELECT VALUE [{size: '5', length: 63.5, unit: 'cm'}, {size: '6', length: 64.5, unit: 'cm'}, {size: '7', length: 66, unit: 'cm'}, {size: '9', length: 67.5, unit: 'cm'}]) WHERE s.key = m.size",
         r#"[{"name":"Remdriel Shoes","size":"Small","length":63.5,"unit":"cm"},{"name":"Remdriel Shoes","size":"Medium","length":64.5,"unit":"cm"},{"name":"Remdriel Shoes","size":"Large","length":67.5,"unit":"cm"},{"name":"Tirevy trunks","size":"Small","length":66,"unit":"cm"}]"#),
        // aggregate subqueries joined in
        ("SELECT p.name, colorCount, smallSizesCount, largeSizesCount FROM products p JOIN (SELECT VALUE COUNT(1) FROM c IN p.colors) AS colorCount JOIN (SELECT VALUE COUNT(1) FROM s in p.sizes WHERE s.description LIKE \"%Small\") AS smallSizesCount JOIN (SELECT VALUE COUNT(1) FROM s in p.sizes WHERE s.description LIKE \"%Large\") AS largeSizesCount WHERE colorCount >= 2 AND largeSizesCount > 0 AND smallSizesCount > 0",
         r#"[{"name":"Remdriel Shoes","colorCount":3,"smallSizesCount":1,"largeSizesCount":1}]"#),
        // join on a subquery over an array expression
        ("SELECT VALUE x FROM products p JOIN x IN (SELECT VALUE p.colors) WHERE p.price = 10", r#"["cobalt","turquoise"]"#),
    ]);
}

#[test]
fn scalar_subqueries() {
    let c = catalog();
    check_table(&c, &[
        ("SELECT 1 AS a, 2 AS b", r#"[{"a":1,"b":2}]"#),
        ("SELECT (SELECT VALUE 1) AS a, (SELECT VALUE 2) AS b", r#"[{"a":1,"b":2}]"#),
        ("SELECT (SELECT VALUE CONCAT('ID-', p.id)) AS internalId FROM products p WHERE p.price = 10", r#"[{"internalId":"ID-00000000-0000-0000-0000-000000004322"}]"#),
        (r#"SELECT p.id, (SELECT p.name WHERE CONTAINS(p.name, "Shoes")).name FROM products p"#,
         r#"[{"id":"00000000-0000-0000-0000-000000004041","name":"Remdriel Shoes"},{"id":"00000000-0000-0000-0000-000000004322"},{"id":"00000000-0000-0000-0000-000000004055"}]"#),
        // aggregate scalar subqueries
        ("SELECT p.name, (SELECT VALUE COUNT(1) FROM c IN p.colors) AS colorsCount FROM products p WHERE p.price = 15", r#"[{"name":"Remdriel Shoes","colorsCount":3}]"#),
        ("SELECT p.name, (SELECT VALUE COUNT(1) FROM c IN p.colors) AS colorsCount, (SELECT VALUE COUNT(1) FROM c IN p.colors WHERE c LIKE \"%t\") AS colorsEndsWithTCount FROM products p WHERE p.price = 10",
         r#"[{"name":"Tirevy trunks","colorsCount":2,"colorsEndsWithTCount":1}]"#),
        ("SELECT p.name, (SELECT VALUE COUNT(1) FROM c IN p.colors) AS colorsCount, (SELECT VALUE COUNT(1) FROM s in p.sizes) AS sizesCount, (SELECT VALUE COUNT(1) FROM t IN p.tags) AS tagsCount FROM products p WHERE p.price = 15",
         r#"[{"name":"Remdriel Shoes","colorsCount":3,"sizesCount":3,"tagsCount":2}]"#),
        ("SELECT VALUE p.name FROM products p WHERE (SELECT VALUE COUNT(1) FROM c IN p.colors) >= 3", r#"["Remdriel Shoes"]"#),
        ("SELECT VALUE p.name FROM products p WHERE (SELECT VALUE COUNT(1) FROM c IN p.colors) = 0", r#"["Pricey Parka"]"#),
        // scalar subquery without rows is undefined; with several rows the first one is used
        ("SELECT VALUE (SELECT VALUE c FROM c IN p.colors WHERE c = 'nope') FROM products p", "[]"),
        ("SELECT VALUE (SELECT VALUE c FROM c IN p.colors) FROM products p", r#"["gray","cobalt"]"#),
        ("SELECT VALUE (SELECT VALUE MAX(s.order) FROM s IN p.sizes) FROM products p", "[4,0,9]"),
        ("SELECT VALUE (SELECT p.name, p.price).price FROM products p WHERE p.price < 15", "[10]"),
        // nested and correlated
        ("SELECT VALUE (SELECT VALUE (SELECT VALUE p.price + 1)) FROM products p WHERE p.price = 15", "[16]"),
        ("SELECT VALUE (SELECT VALUE COUNT(1) FROM t IN p.tags WHERE (SELECT VALUE COUNT(1) FROM s IN p.sizes WHERE s.order > 2) > 1) FROM products p", "[2,0,0]"),
    ]);
}

#[test]
fn exists_and_array() {
    let c = catalog();
    check_table(&c, &[
        ("SELECT VALUE EXISTS (SELECT VALUE undefined)", "[false]"),
        ("SELECT VALUE EXISTS (SELECT undefined)", "[true]"),
        ("SELECT VALUE EXISTS (SELECT VALUE 1)", "[true]"),
        ("SELECT VALUE EXISTS (SELECT VALUE null)", "[true]"),
        ("SELECT VALUE EXISTS (SELECT VALUE 1 WHERE false)", "[false]"),
        (r#"SELECT p.name, p.colors FROM products p WHERE ARRAY_CONTAINS(p.colors, "cobalt")"#, r#"[{"name":"Tirevy trunks","colors":["cobalt","turquoise"]}]"#),
        (r#"SELECT p.name, p.colors FROM products p WHERE EXISTS (SELECT VALUE c FROM c IN p.colors WHERE c = "cobalt")"#, r#"[{"name":"Tirevy trunks","colors":["cobalt","turquoise"]}]"#),
        (r#"SELECT VALUE p.name FROM products p WHERE EXISTS (SELECT VALUE t FROM t IN p.tags WHERE t.key = "fabric" AND t["value"] = "leather")"#, r#"["Remdriel Shoes"]"#),
        (r#"SELECT p.name, EXISTS (SELECT VALUE t FROM t IN p.tags WHERE t.key = "fabric" AND t["value"] = "leather") AS containsFabricLeatherTag FROM products p"#,
         r#"[{"name":"Remdriel Shoes","containsFabricLeatherTag":true},{"name":"Tirevy trunks","containsFabricLeatherTag":false},{"name":"Pricey Parka","containsFabricLeatherTag":false}]"#),
        (r#"SELECT p.name, t.description AS tag FROM products p JOIN t in p.tags WHERE t.key = "fabric" AND t["value"] = "leather""#, r#"[{"name":"Remdriel Shoes","tag":"Leather"}]"#),
        ("SELECT VALUE p.name FROM products p WHERE NOT EXISTS (SELECT VALUE c FROM c IN p.colors)", r#"["Pricey Parka"]"#),
        // ARRAY(...)
        ("SELECT p.name, ARRAY (SELECT VALUE s.key FROM s IN p.sizes) AS sizes FROM products p WHERE p.price = 15", r#"[{"name":"Remdriel Shoes","sizes":["5","6","9"]}]"#),
        ("SELECT p.name, ARRAY (SELECT VALUE s.key FROM s IN p.sizes WHERE STRINGTONUMBER(s.key) <= 6) AS smallSizes, ARRAY (SELECT VALUE s.key FROM s IN p.sizes WHERE STRINGTONUMBER(s.key) >= 9) AS largeSizes FROM products p WHERE p.price = 15",
         r#"[{"name":"Remdriel Shoes","smallSizes":["5","6"],"largeSizes":["9"]}]"#),
        ("SELECT p.name, z.s.key AS sizes FROM products p JOIN z IN (SELECT VALUE ARRAY (SELECT s FROM s IN p.sizes WHERE STRINGTONUMBER(s.key) <= 6)) WHERE p.price = 15",
         r#"[{"name":"Remdriel Shoes","sizes":"5"},{"name":"Remdriel Shoes","sizes":"6"}]"#),
        ("SELECT VALUE ARRAY(SELECT VALUE c FROM c IN p.colors) FROM products p", r#"[["gray","darkgray","blue"],["cobalt","turquoise"],[]]"#),
        ("SELECT VALUE ARRAY(SELECT VALUE 1 WHERE false)", "[[]]"),
        ("SELECT VALUE ARRAY(SELECT VALUE undefined)", "[[]]"),
        ("SELECT VALUE ARRAY(SELECT 1 AS a)", r#"[[{"a":1}]]"#),
        ("SELECT VALUE ARRAY_LENGTH(ARRAY(SELECT VALUE c FROM c IN p.colors)) FROM products p", "[3,2,0]"),
    ]);
}

#[test]
fn from_subqueries_are_container_scoped() {
    let d = vec![
        json!({"id": 1, "city": "A", "arr": [1, 2]}),
        json!({"id": 2, "city": "B", "arr": []}),
        json!({"id": 3, "city": "A", "arr": [3]}),
    ];
    check_table(
        &d,
        &[
            ("SELECT VALUE COUNT(1) FROM (SELECT DISTINCT VALUE c.city FROM c)", "[2]"),
            ("SELECT VALUE x FROM (SELECT VALUE c.id FROM c) AS x", "[1,2,3]"),
            ("SELECT t.n FROM (SELECT c.id AS n FROM c WHERE c.id > 1) t", r#"[{"n":2},{"n":3}]"#),
            ("SELECT VALUE t FROM (SELECT VALUE c.city FROM c ORDER BY c.id DESC) t", r#"["A","B","A"]"#),
            ("SELECT VALUE SUM(t.n) FROM (SELECT c.id AS n FROM c) t", "[6]"),
            ("SELECT VALUE e FROM (SELECT c.arr FROM c) t JOIN e IN t.arr", "[1,2,3]"),
            ("SELECT VALUE t.city FROM (SELECT DISTINCT c.city FROM c) t", r#"["A","B"]"#),
            ("SELECT VALUE COUNT(1) FROM (SELECT VALUE COUNT(1) FROM c)", "[1]"),
            ("SELECT VALUE n FROM (SELECT VALUE COUNT(1) FROM c) n", "[3]"),
            ("SELECT VALUE y FROM (SELECT VALUE x FROM (SELECT VALUE c.id FROM c) x WHERE x < 3) y", "[1,2]"),
            ("SELECT TOP 1 VALUE t.id FROM (SELECT * FROM c ORDER BY c.id DESC) t", "[3]"),
        ],
    );
}

#[test]
fn identifier_resolution() {
    // aliases shadow outer aliases; inner queries see outer aliases; unknown names fail
    let d = vec![json!({"id": 1, "x": "outer", "arr": [{"x": "inner"}]})];
    check_table(
        &d,
        &[
            ("SELECT VALUE (SELECT VALUE x.x FROM x IN c.arr) FROM c", r#"["inner"]"#),
            ("SELECT VALUE (SELECT VALUE c.x) FROM c", r#"["outer"]"#),
            ("SELECT VALUE ARRAY(SELECT VALUE c.x FROM c IN c.arr) FROM c", r#"[["inner"]]"#),
        ],
    );
    for q in [
        "SELECT x FROM c",
        "SELECT VALUE c.id FROM c WHERE d.id = 1",
        "SELECT VALUE (SELECT VALUE z FROM z IN w.arr) FROM c",
        "SELECT VALUE c FROM c JOIN d IN e.arr",
        "SELECT * FROM c JOIN d IN c.arr",
        "SELECT VALUE 1 FROM c JOIN c IN c.arr",
    ] {
        let e = err(q);
        assert!(["SC2001", "SC2002", "SC2103"].contains(&e.code.as_str()), "{q}: {e}");
    }
    assert_eq!(err("SELECT x FROM c").message, "Identifier 'x' could not be resolved.");
}
