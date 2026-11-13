// Some code samples and query examples in this file are reproduced from Microsoft Learn and from Microsoft's SDKs (MIT licence, Copyright Microsoft); see NOTICE and LICENSES/MIT-Microsoft.txt.
//! SELECT / FROM / JOIN / subqueries. Examples marked with a page name come from
//! learn.microsoft.com/azure/cosmos-db/nosql/query/<page> (data sets completed by us).

mod common;
use common::*;
use serde_json::{json, Value};

fn products() -> Vec<Value> {
    vec![
        json!({"id": "1", "name": "Remdriel Shoes", "sku": "61506", "category": "shoes",
               "metadata": {"link": "https://example.com/remdriel-shoes", "status": "active"},
               "tags": [{"name": "waterproof", "value": "suede-leather-and-mesh"}, {"name": "lightweight", "value": "x"}],
               "sizes": [{"key": "65", "description": "6.5", "order": 0}, {"key": "75", "description": "7.5", "order": 1}],
               "colors": ["blue", "gray"], "price": 15}),
        json!({"id": "2", "name": "Tirevy trunks", "sku": "73402", "category": "swim",
               "metadata": {"link": "https://example.com/tirevy-trunks", "status": "inactive"},
               "tags": [{"name": "quick-dry", "value": "polyester"}],
               "sizes": [{"key": "s", "description": "Small", "order": 3}, {"key": "xl", "description": "Extra Large", "order": 5}],
               "colors": ["gray"], "price": 10}),
        json!({"id": "3", "name": "Plain thing", "sku": "1", "colors": [], "price": 24.5}),
    ]
}

#[test]
fn select_projection_forms() {
    let p = products();
    check_table(
        &p,
        &[
            // select page
            (r#"SELECT "Cosmic", "Works""#, r#"[{"$1":"Cosmic","$2":"Works"}]"#),
            (r#"SELECT VALUE "Cosmic Works""#, r#"["Cosmic Works"]"#),
            (
                "SELECT {identifier: p.name, model: p.sku} FROM products p WHERE p.id = '1'",
                r#"[{"$1":{"identifier":"Remdriel Shoes","model":"61506"}}]"#,
            ),
            (
                r#"SELECT VALUE {name: p.name, link: p.metadata.link, firstTag: p.tags[0]["value"]} FROM products p WHERE p.id IN ('1','2')"#,
                r#"[{"name":"Remdriel Shoes","link":"https://example.com/remdriel-shoes","firstTag":"suede-leather-and-mesh"},
             {"name":"Tirevy trunks","link":"https://example.com/tirevy-trunks","firstTag":"polyester"}]"#,
            ),
            ("SELECT c.id FROM c WHERE c.metadata.status = \"active\"", r#"[{"id":"1"}]"#),
            (r#"SELECT c.id FROM c WHERE ARRAY_CONTAINS(c.tags, {"name": "waterproof"}, true)"#, r#"[{"id":"1"}]"#),
            (r#"SELECT c.id FROM c WHERE ARRAY_CONTAINS(c.tags, {"name": "waterproof"})"#, "[]"),
            (
                r#"SELECT c.id, t AS tag FROM c JOIN t IN c.tags WHERE t.name = "waterproof""#,
                r#"[{"id":"1","tag":{"name":"waterproof","value":"suede-leather-and-mesh"}}]"#,
            ),
            // aliases
            ("SELECT p.name AS n, p.sku code FROM p WHERE p.id = '2'", r#"[{"n":"Tirevy trunks","code":"73402"}]"#),
            ("SELECT p.name AS n FROM products AS p WHERE p.id = '2'", r#"[{"n":"Tirevy trunks"}]"#),
            ("SELECT products.id FROM products WHERE products.id = '2'", r#"[{"id":"2"}]"#),
            // unnamed expressions: $1, $2, ... counted among unnamed items only
            ("SELECT p.price + 1, p.id, p.price * 2 FROM p WHERE p.id = '2'", r#"[{"$1":11,"id":"2","$2":20}]"#),
            ("SELECT p.sizes[0].key FROM p WHERE p.id = '1'", r#"[{"key":"65"}]"#),
            (r#"SELECT p["metadata"]["link"] FROM p WHERE p.id = '1'"#, r#"[{"link":"https://example.com/remdriel-shoes"}]"#),
            // undefined properties are omitted; undefined VALUE yields no row
            ("SELECT p.id, p.metadata FROM p WHERE p.id = '3'", r#"[{"id":"3"}]"#),
            (
                "SELECT VALUE p.metadata FROM p",
                r#"[{"link":"https://example.com/remdriel-shoes","status":"active"},{"link":"https://example.com/tirevy-trunks","status":"inactive"}]"#,
            ),
            ("SELECT undefined", "[{}]"),
            ("SELECT VALUE undefined", "[]"),
            ("SELECT VALUE null", "[null]"),
            ("SELECT VALUE [1, undefined, 2]", "[[1,2]]"),
            (r#"SELECT VALUE {"a": undefined, "b": 1}"#, r#"[{"b":1}]"#),
            // object/array construction
            (r#"SELECT {"a": c.id, "b": [c.price, 1]} AS o FROM c WHERE c.id = '1'"#, r#"[{"o":{"a":"1","b":[15,1]}}]"#),
            (
                "SELECT p.name, [p.category, p.sku] AS metadata FROM p WHERE p.id = '1'",
                r#"[{"name":"Remdriel Shoes","metadata":["shoes","61506"]}]"#,
            ),
            // SELECT * and alias projection
            ("SELECT * FROM p WHERE p.id = '3'", r#"[{"id":"3","name":"Plain thing","sku":"1","colors":[],"price":24.5}]"#),
            ("SELECT p FROM p WHERE p.id = '3'", r#"[{"p":{"id":"3","name":"Plain thing","sku":"1","colors":[],"price":24.5}}]"#),
            ("SELECT VALUE p.id FROM p", r#"["1","2","3"]"#),
            // key order of the output object follows the select list
            ("SELECT p.sku, p.id FROM p WHERE p.id = '1'", r#"[{"sku":"61506","id":"1"}]"#),
            // duplicate output names: the last wins
            ("SELECT p.id AS a, p.sku AS a FROM p WHERE p.id = '1'", r#"[{"a":"61506"}]"#),
            // case-insensitive keywords, case-sensitive properties
            ("select value p.id from p where p.id = '1'", r#"["1"]"#),
            ("SELECT VALUE p.ID FROM p", "[]"),
            // statement without FROM
            ("SELECT 1 + 1 AS two", r#"[{"two":2}]"#),
            ("SELECT VALUE 1 WHERE false", "[]"),
            ("SELECT VALUE 1 WHERE true", "[1]"),
        ],
    );
}

#[test]
fn from_sources() {
    let fam = families();
    check_table(&fam, &[
        ("SELECT VALUE f.id FROM Families f", r#"["AndersenFamily","WakefieldFamily"]"#),
        ("SELECT VALUE r.id FROM root AS r", r#"["AndersenFamily","WakefieldFamily"]"#),
        ("SELECT VALUE c.id FROM c", r#"["AndersenFamily","WakefieldFamily"]"#),
        // sub-root sources keep the value (no iteration)
        ("SELECT VALUE f.address.city FROM Families f", r#"["Seattle","NY"]"#),
        ("SELECT * FROM Families.address", r#"[{"state":"WA","county":"King","city":"Seattle"},{"state":"NY","county":"Manhattan","city":"NY"}]"#),
        ("SELECT VALUE a.city FROM Families.address a", r#"["Seattle","NY"]"#),
        ("SELECT VALUE address.city FROM Families.address", r#"["Seattle","NY"]"#),
        ("SELECT VALUE ch FROM Families.children ch", r#"[[{"firstName":"Henriette Thaulow","gender":"female","grade":5,"pets":[{"givenName":"Fluffy"}]}],
            [{"familyName":"Merriam","givenName":"Jesse","gender":"female","grade":1,"pets":[{"givenName":"Goofy"},{"givenName":"Shadow"}]},{"familyName":"Miller","givenName":"Lisa","gender":"female","grade":8}]]"#),
        ("SELECT VALUE Families.children[0].grade FROM Families", "[5,1]"),
        // documents where the source path is undefined are skipped
        ("SELECT VALUE familyName FROM c.parents[1].familyName", r#"["Miller"]"#),
        // iteration
        ("SELECT VALUE c.givenName FROM c IN Families.children", r#"["Jesse","Lisa"]"#),
        ("SELECT VALUE c.firstName FROM c IN Families.children", r#"["Henriette Thaulow"]"#),
        ("SELECT VALUE COUNT(1) FROM c IN Families.children", "[3]"),
        ("SELECT * FROM c IN Families.children WHERE c.grade > 5", r#"[{"familyName":"Miller","givenName":"Lisa","gender":"female","grade":8}]"#),
        ("SELECT VALUE p.givenName FROM p IN Families.parents", r#"["Robin","Ben"]"#),
        // iterating over something that is not an array yields nothing
        ("SELECT VALUE x FROM x IN Families.address", "[]"),
        ("SELECT VALUE x FROM x IN Families.nothing", "[]"),
        // JOINs: cross product per document
        ("SELECT f.id, c.givenName FROM Families f JOIN c IN f.children", r#"[{"id":"AndersenFamily"},{"id":"WakefieldFamily","givenName":"Jesse"},{"id":"WakefieldFamily","givenName":"Lisa"}]"#),
        ("SELECT VALUE COUNT(1) FROM Families f JOIN c IN f.children JOIN p IN f.parents", "[6]"),
        ("SELECT f.id, c.givenName, p.givenName AS parent FROM Families f JOIN c IN f.children JOIN p IN f.parents WHERE c.grade = 8 AND p.givenName = 'Ben'",
         r#"[{"id":"WakefieldFamily","givenName":"Lisa","parent":"Ben"}]"#),
        ("SELECT VALUE pet.givenName FROM Families f JOIN c IN f.children JOIN pet IN c.pets", r#"["Fluffy","Goofy","Shadow"]"#),
        // JOIN without IN binds the value
        ("SELECT VALUE a.city FROM Families f JOIN f.address a", r#"["Seattle","NY"]"#),
        // JOIN on an undefined path drops the document
        ("SELECT VALUE f.id FROM Families f JOIN x IN f.nothing", "[]"),
    ]);
}

#[test]
fn from_sources_order_agnostic() {
    // Cross-product order is document order, then join order; the doc pages' own listings are
    // sorted differently, so these are compared as sets.
    let d = vec![
        json!({"name": "Gremon Fins", "colors": ["science-blue", "turbo"]}),
        json!({"name": "Elecy Jacket", "colors": ["indigo-shark", "jordy-blue-shark"]}),
        json!({"name": "Tresko Pack", "colors": ["golden-dream"]}),
    ];
    check_table_unordered(
        &d,
        &[
            // join page
            (
                "SELECT p.name, c AS color FROM products p JOIN c in p.colors",
                r#"[{"name":"Gremon Fins","color":"science-blue"},{"name":"Gremon Fins","color":"turbo"},
             {"name":"Elecy Jacket","color":"indigo-shark"},{"name":"Elecy Jacket","color":"jordy-blue-shark"},
             {"name":"Tresko Pack","color":"golden-dream"}]"#,
            ),
            (
                r#"SELECT p.name, c AS color FROM products p JOIN c in p.colors WHERE c LIKE "%blue%""#,
                r#"[{"name":"Elecy Jacket","color":"jordy-blue-shark"},{"name":"Gremon Fins","color":"science-blue"}]"#,
            ),
            (r#"SELECT VALUE p.name FROM products p JOIN c in p.colors WHERE c LIKE "%blue%""#, r#"["Elecy Jacket","Gremon Fins"]"#),
        ],
    );
}

#[test]
fn join_page_sizes() {
    // join page: self-join with a single item
    let d = vec![json!({"name": "Raiot Jacket", "sizes": [
        {"key": "s", "description": "Small"}, {"key": "m", "description": "Medium"},
        {"key": "l", "description": "Large"}, {"key": "xl", "description": "Extra Large"}]})];
    check_table(
        &d,
        &[
            (
                "SELECT p.name, s.key, s.description FROM products p JOIN s in p.sizes",
                r#"[{"name":"Raiot Jacket","key":"s","description":"Small"},{"name":"Raiot Jacket","key":"m","description":"Medium"},
             {"name":"Raiot Jacket","key":"l","description":"Large"},{"name":"Raiot Jacket","key":"xl","description":"Extra Large"}]"#,
            ),
            (
                r#"SELECT p.name, s.key AS size FROM products p JOIN s in p.sizes WHERE s.description LIKE "%Large""#,
                r#"[{"name":"Raiot Jacket","size":"l"},{"name":"Raiot Jacket","size":"xl"}]"#,
            ),
            (
                r#"SELECT * FROM products p WHERE p.sizes[0].description LIKE "%Large" OR p.sizes[1].description LIKE "%Large" OR p.sizes[2].description LIKE "%Large" OR p.sizes[3].description LIKE "%Large""#,
                r#"[{"name":"Raiot Jacket","sizes":[{"key":"s","description":"Small"},{"key":"m","description":"Medium"},{"key":"l","description":"Large"},{"key":"xl","description":"Extra Large"}]}]"#,
            ),
        ],
    );
}

#[test]
fn get_started_json_page() {
    // get-started-json page
    let heatker = json!({"name": "Heatker Women's Jacket", "category": "apparel", "slug": "heatker-women-s-jacket",
        "sizes": [{"key": "s", "description": "Small"}],
        "metadata": {"link": "https://www.adventure-works.com/heatker-women-s-jacket/68719520138.p"}});
    let swim = vec![
        json!({"name": "Pila Swimsuit", "colors": ["regal-blue", "rose-bud-cherry"],
               "sizes": [{"key": "m", "description": "Medium"}, {"key": "l", "description": "Large"}, {"key": "xl", "description": "Extra Large"}]}),
        json!({"name": "Makay Bikini", "colors": ["starship"],
               "sizes": [{"key": "s", "description": "Small"}, {"key": "m", "description": "Medium"}, {"key": "l", "description": "Large"}]}),
    ];
    check_table(
        std::slice::from_ref(&heatker),
        &[
            (
                "SELECT p.name, p.category, p.metadata.link FROM products p WHERE p.name = \"Heatker Women's Jacket\"",
                r#"[{"name":"Heatker Women's Jacket","category":"apparel","link":"https://www.adventure-works.com/heatker-women-s-jacket/68719520138.p"}]"#,
            ),
            (
                "SELECT p.name, p.sizes[0].description AS defaultSize FROM products p WHERE p.name = \"Heatker Women's Jacket\"",
                r#"[{"name":"Heatker Women's Jacket","defaultSize":"Small"}]"#,
            ),
            (
                "SELECT p.name, [p.category, p.slug, p.metadata.link] AS metadata FROM products p",
                r#"[{"name":"Heatker Women's Jacket","metadata":["apparel","heatker-women-s-jacket","https://www.adventure-works.com/heatker-women-s-jacket/68719520138.p"]}]"#,
            ),
            (
                "SELECT p.metadata.link AS a, p[\"metadata\"][\"link\"] AS b, p.metadata[\"link\"] AS c FROM products p",
                r#"[{"a":"https://www.adventure-works.com/heatker-women-s-jacket/68719520138.p","b":"https://www.adventure-works.com/heatker-women-s-jacket/68719520138.p","c":"https://www.adventure-works.com/heatker-women-s-jacket/68719520138.p"}]"#,
            ),
            (
                "SELECT {\"brandName\": p.name, \"department\": p.category} FROM products p",
                r#"[{"$1":{"brandName":"Heatker Women's Jacket","department":"apparel"}}]"#,
            ),
            (
                "SELECT {\"brandName\": p.name, \"department\": p.category} AS product FROM products p",
                r#"[{"product":{"brandName":"Heatker Women's Jacket","department":"apparel"}}]"#,
            ),
            (
                "SELECT VALUE {\"product name\": p.name, \"from\": p.metadata.link, \"detail/category\": p.category} FROM products p",
                r#"[{"product name":"Heatker Women's Jacket","from":"https://www.adventure-works.com/heatker-women-s-jacket/68719520138.p","detail/category":"apparel"}]"#,
            ),
        ],
    );
    check_table(&swim, &[
        ("SELECT * FROM p IN p.colors", r#"["regal-blue","rose-bud-cherry","starship"]"#),
        (r#"SELECT p.key FROM p IN p.sizes WHERE p.description LIKE "%Large""#, r#"[{"key":"l"},{"key":"xl"},{"key":"l"}]"#),
        ("SELECT VALUE COUNT(1) FROM p IN p.sizes", "[6]"),
        (r#"SELECT VALUE p.name FROM products p WHERE EXISTS(SELECT VALUE c FROM c IN p.sizes WHERE c.description LIKE "%Large")"#,
         r#"["Pila Swimsuit","Makay Bikini"]"#),
        ("SELECT IS_DEFINED(p.sku) AS isSkuDefined, IS_NULL(p.sku) AS isSkuDefinedButNull FROM products p WHERE p.name = 'Makay Bikini'",
         r#"[{"isSkuDefined":false,"isSkuDefinedButNull":false}]"#),
    ]);
    // null vs undefined
    check_table(
        &[json!({"name": "Witalica helmet", "sku": null})],
        &[("SELECT IS_DEFINED(p.sku) AS d, IS_NULL(p.sku) AS n FROM products p", r#"[{"d":true,"n":true}]"#)],
    );
}

#[test]
fn where_order_offset_pages() {
    // where / order-by / offset-limit pages
    let emp = vec![
        json!({"name": {"first": "Jordan", "last": "Mitchell"}, "team": "Hospitality"}),
        json!({"name": {"first": "Ashley", "last": "Schroeder"}, "team": "Hospitality"}),
        json!({"name": {"first": "Tomas", "last": "Richardson"}, "team": "Hospitality"}),
        json!({"name": {"first": "Amari", "last": "Rivera"}, "team": "Human resources"}),
        json!({"name": {"first": "Kayla", "last": "Lewis"}, "team": "Human resources"}),
        json!({"name": {"first": "Casey", "last": "Jensen"}, "team": "Human resources"}),
        json!({"name": "Riley Johnson", "team": "Leadership team"}),
        json!({"name": "Jennifer Wilkins", "team": "Leadership team"}),
        json!({"name": "Isaac Talbot", "team": "Leadership team"}),
        json!({"name": "Zed Zimmer", "team": "Leadership team"}),
    ];
    check_table(&emp, &[
        ("SELECT VALUE {employeeName: e.name.first, currentTeam: e.team} FROM employees e WHERE e.team = \"Hospitality\"",
         r#"[{"employeeName":"Jordan","currentTeam":"Hospitality"},{"employeeName":"Ashley","currentTeam":"Hospitality"},{"employeeName":"Tomas","currentTeam":"Hospitality"}]"#),
        ("SELECT VALUE {firstName: e.name.first, lastName: e.name.last} FROM employees e WHERE e.team = \"Human resources\" ORDER BY e.name.last",
         r#"[{"firstName":"Casey","lastName":"Jensen"},{"firstName":"Kayla","lastName":"Lewis"},{"firstName":"Amari","lastName":"Rivera"}]"#),
        ("SELECT VALUE {name: e.name} FROM employees e WHERE e.team = \"Leadership team\" ORDER BY e.name OFFSET 1 LIMIT 3",
         r#"[{"name":"Jennifer Wilkins"},{"name":"Riley Johnson"},{"name":"Zed Zimmer"}]"#),
        ("SELECT VALUE e.name FROM employees e WHERE e.team = \"Leadership team\" ORDER BY e.name DESC OFFSET 0 LIMIT 2", r#"["Zed Zimmer","Riley Johnson"]"#),
        ("SELECT VALUE e.name FROM employees e WHERE e.team = \"Leadership team\" ORDER BY e.name OFFSET 10 LIMIT 2", "[]"),
        ("SELECT VALUE e.name FROM employees e WHERE e.team = \"Leadership team\" ORDER BY e.name OFFSET 3 LIMIT 100", r#"["Zed Zimmer"]"#),
        ("SELECT VALUE e.team FROM employees e OFFSET 8 LIMIT 5", r#"["Leadership team","Leadership team"]"#),
        ("SELECT TOP 2 VALUE e.team FROM employees e", r#"["Hospitality","Hospitality"]"#),
        ("SELECT TOP 0 VALUE e.team FROM employees e", "[]"),
        ("SELECT DISTINCT VALUE e.team FROM employees e", r#"["Hospitality","Human resources","Leadership team"]"#),
        ("SELECT DISTINCT e.team FROM employees e", r#"[{"team":"Hospitality"},{"team":"Human resources"},{"team":"Leadership team"}]"#),
        ("SELECT DISTINCT VALUE e.nothing FROM employees e", "[]"),
        ("SELECT DISTINCT e.nothing FROM employees e", "[{}]"),
    ]);
    // distinct page: missing field gives one empty object
    let prod =
        vec![json!({"category": "Accessories"}), json!({"category": "Tools"}), json!({"category": "Accessories"}), json!({"name": "x"})];
    check_table(
        &prod,
        &[
            ("SELECT DISTINCT VALUE p.category FROM products p", r#"["Accessories","Tools"]"#),
            ("SELECT DISTINCT p.category FROM products p", r#"[{"category":"Accessories"},{"category":"Tools"},{}]"#),
        ],
    );
}

#[test]
fn parameters() {
    let d = vec![
        json!({"id": "a", "price": 50, "onSale": false}),
        json!({"id": "b", "price": 150, "onSale": false}),
        json!({"id": "c", "price": 20, "onSale": true}),
    ];
    // parameterized-queries page
    assert_eq!(
        run_p(
            "SELECT * FROM p WHERE (NOT p.onSale) AND (p.price BETWEEN 0 AND @upperPriceLimit)",
            &d,
            json!([{"name": "@upperPriceLimit", "value": 100}])
        ),
        vec![json!({"id": "a", "price": 50, "onSale": false})]
    );
    assert_eq!(
        run_p("SELECT TOP @pageSize VALUE p.id FROM p", &d, json!([{"name": "@pageSize", "value": 2}])),
        vec![json!("a"), json!("b")]
    );
    assert_eq!(
        run_p("SELECT VALUE p.id FROM p OFFSET @o LIMIT @l", &d, json!([{"name":"@o","value":1},{"name":"@l","value":1}])),
        vec![json!("b")]
    );
    assert_eq!(run_p("SELECT VALUE @x", &[], json!([{"name": "@x", "value": {"a": [1, null]}}])), vec![json!({"a": [1, null]})]);
    assert_eq!(
        run_p("SELECT VALUE p.id FROM p WHERE p.id IN (@a, @b)", &d, json!([{"name":"@a","value":"a"},{"name":"@b","value":"c"}])),
        vec![json!("a"), json!("c")]
    );
    assert_eq!(run_p("SELECT VALUE @n", &[], json!([{"name": "@n", "value": null}])), vec![Value::Null]);
    assert_eq!(
        run_p("SELECT VALUE ARRAY_CONTAINS(@arr, p.id) FROM p", &d, json!([{"name":"@arr","value":["a","c"]}])),
        vec![json!(true), json!(false), json!(true)]
    );
    // missing parameter, negative TOP
    let e = err("SELECT VALUE @missing");
    assert_eq!(e.kind, azemu_cosmos_sql::ErrorKind::Evaluation);
    assert!(e.message.contains("@missing"), "{}", e.message);
    let e = try_run_p_err("SELECT TOP @n VALUE 1", json!([{"name":"@n","value":-1}]));
    assert!(e.message.contains("TOP"), "{}", e.message);
    let e = try_run_p_err("SELECT TOP @n VALUE 1", json!([{"name":"@n","value":"x"}]));
    assert!(e.message.contains("TOP"), "{}", e.message);
}

fn try_run_p_err(sql: &str, params: Value) -> azemu_cosmos_sql::SqlError {
    let p = azemu_cosmos_sql::Params::from_cosmos(&params);
    try_run_p(sql, &[], &p, &fixed_clock_opts()).unwrap_err()
}
