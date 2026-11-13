//! Aggregates, GROUP BY, ORDER BY (cross-type order), DISTINCT, TOP, OFFSET/LIMIT.

mod common;
use azemu_cosmos_sql::EvalOptions;
use common::*;
use serde_json::{json, Value};

fn people() -> Vec<Value> {
    vec![
        json!({"id": 1, "city": "Seattle", "age": 30, "score": 10}),
        json!({"id": 2, "city": "Seattle", "age": 25, "score": 20.5}),
        json!({"id": 3, "city": "NY", "age": 40, "score": null}),
        json!({"id": 4, "city": "NY", "age": "x", "score": 5}),
        json!({"id": 5, "age": 35, "score": 7}),
    ]
}

#[test]
fn aggregate_functions() {
    let p = people();
    check_table(
        &p,
        &[
            ("SELECT VALUE COUNT(1) FROM c", "[5]"),
            ("SELECT VALUE COUNT(c.city) FROM c", "[4]"),
            ("SELECT VALUE COUNT(c.score) FROM c", "[5]"), // null counts, undefined does not
            ("SELECT VALUE COUNT(c.nothing) FROM c", "[0]"),
            ("SELECT VALUE COUNT(1) FROM c WHERE false", "[0]"),
            ("SELECT VALUE COUNT(2 + 3) FROM c", "[5]"),
            ("SELECT VALUE {countScalar: COUNT(1), countExpression: COUNT(2 + 3)}", r#"[{"countScalar":1,"countExpression":1}]"#),
            ("SELECT COUNT(1) AS n FROM c", r#"[{"n":5}]"#),
            // SUM / AVG: a string, boolean or null among the values makes the result undefined
            ("SELECT VALUE SUM(c.age) FROM c", "[]"),
            ("SELECT SUM(c.age) AS s FROM c", "[{}]"),
            ("SELECT VALUE SUM(c.score) FROM c", "[]"),
            ("SELECT VALUE AVG(c.age) FROM c", "[]"),
            ("SELECT VALUE SUM(c.age) FROM c WHERE c.id IN (1, 2, 5)", "[90]"),
            ("SELECT VALUE AVG(c.age) FROM c WHERE c.id IN (1, 2, 5)", "[30]"),
            ("SELECT VALUE SUM(c.score) FROM c WHERE c.id IN (1, 2, 5)", "[37.5]"),
            ("SELECT VALUE AVG(c.score) FROM c WHERE c.id IN (1, 2, 5)", "[12.5]"),
            ("SELECT VALUE SUM(c.id) FROM c", "[15]"),
            ("SELECT VALUE AVG(c.id) FROM c", "[3]"),
            ("SELECT VALUE SUM(c.city) FROM c", "[]"),
            ("SELECT VALUE SUM(c.nothing) FROM c", "[]"),
            ("SELECT VALUE AVG(c.nothing) FROM c", "[]"),
            ("SELECT VALUE SUM(c.id) FROM c WHERE false", "[]"),
            ("SELECT VALUE SUM(c.city = 'NY' ? 1 : 0) FROM c", "[2]"), // the row without city yields undefined and is skipped
            // MIN / MAX use the cross-type order null < boolean < number < string
            ("SELECT VALUE MIN(c.age) FROM c", "[25]"),
            ("SELECT VALUE MAX(c.age) FROM c", r#"["x"]"#),
            ("SELECT VALUE MIN(c.score) FROM c", "[null]"),
            ("SELECT VALUE MAX(c.score) FROM c", "[20.5]"),
            ("SELECT VALUE MIN(c.city) FROM c", r#"["NY"]"#),
            ("SELECT VALUE MAX(c.city) FROM c", r#"["Seattle"]"#),
            ("SELECT VALUE MIN(c.nothing) FROM c", "[]"),
            ("SELECT VALUE MAX(c.id) FROM c WHERE c.id < 3", "[2]"),
            // several aggregates, aggregates inside expressions
            ("SELECT COUNT(1) AS n, SUM(c.id) AS total, MAX(c.id) AS hi FROM c", r#"[{"n":5,"total":15,"hi":5}]"#),
            ("SELECT VALUE COUNT(1) + 1 FROM c", "[6]"),
            ("SELECT VALUE [COUNT(1), SUM(c.id)] FROM c", "[[5,15]]"),
            (r#"SELECT VALUE {"item": COUNT(1)} FROM c"#, r#"[{"item":5}]"#),
            ("SELECT VALUE SUM(c.id) / COUNT(1) FROM c", "[3]"),
            ("SELECT VALUE SUM(c.id * 2) FROM c", "[30]"),
            ("SELECT VALUE COUNT(1) FROM c WHERE c.age > 26", "[3]"),
            ("SELECT VALUE MAX(LENGTH(c.city)) FROM c", "[7]"),
        ],
    );
    // sum / avg / min / max pages
    let prod = vec![json!({"name": "Diannis Watch", "price": 98}), json!({"name": "Confira Watch", "price": 105})];
    check_table(
        &prod,
        &[
            ("SELECT AVG(p.price) AS averagePrice FROM products p", r#"[{"averagePrice":101.5}]"#),
            ("SELECT MAX(p.price) AS maxPrice FROM p", r#"[{"maxPrice":105}]"#),
            ("SELECT MIN(p.price) AS minPrice FROM p", r#"[{"minPrice":98}]"#),
        ],
    );
    let q =
        vec![json!({"quantity": 0}), json!({"quantity": 230}), json!({"quantity": 14}), json!({"quantity": 232}), json!({"quantity": 141})];
    check_table(&q, &[("SELECT VALUE SUM(p.quantity) FROM products p", "[617]")]);
}

#[test]
fn aggregate_over_mixed_values() {
    // booleans and nulls poison SUM/AVG, but not MIN/MAX/COUNT
    let d = vec![
        json!({"v": 1}),
        json!({"v": true}),
        json!({"v": "s"}),
        json!({"v": null}),
        json!({"v": [1]}),
        json!({"v": {"a": 1}}),
        json!({}),
    ];
    check_table(
        &d,
        &[
            ("SELECT VALUE COUNT(c.v) FROM c", "[6]"),
            ("SELECT VALUE SUM(c.v) FROM c", "[]"),
            ("SELECT VALUE MIN(c.v) FROM c", "[null]"),
            ("SELECT VALUE MAX(c.v) FROM c", r#"[{"a":1}]"#),
            ("SELECT VALUE MAX(c.v) FROM c WHERE IS_PRIMITIVE(c.v)", r#"["s"]"#),
            ("SELECT VALUE MIN(c.v) FROM c WHERE IS_BOOL(c.v) OR IS_NUMBER(c.v)", "[true]"),
            ("SELECT VALUE SUM(c.v) FROM c WHERE IS_NUMBER(c.v)", "[1]"),
        ],
    );
    let floats = vec![json!({"v": 0.1}), json!({"v": 0.2}), json!({"v": 0.3})];
    check_table(&floats, &[("SELECT VALUE SUM(c.v) FROM c", "[0.6000000000000001]"), ("SELECT VALUE MAX(c.v) FROM c", "[0.3]")]);
    // integer overflow falls back to floating point
    let big = vec![json!({"v": 9223372036854775807i64}), json!({"v": 1})];
    check_table(&big, &[("SELECT VALUE SUM(c.v) FROM c", "[9.223372036854776e18]")]);
}

#[test]
fn group_by() {
    let p = people();
    check_table(
        &p,
        &[
            ("SELECT c.city, COUNT(1) AS n FROM c GROUP BY c.city", r#"[{"city":"Seattle","n":2},{"city":"NY","n":2},{"n":1}]"#),
            ("SELECT VALUE COUNT(1) FROM c GROUP BY c.city", "[2,2,1]"),
            ("SELECT c.city, AVG(c.age) AS a FROM c GROUP BY c.city", r#"[{"city":"Seattle","a":27.5},{"city":"NY"},{"a":35}]"#),
            (
                "SELECT c.city, MIN(c.id) AS lo, MAX(c.id) AS hi, SUM(c.id) AS s FROM c GROUP BY c.city",
                r#"[{"city":"Seattle","lo":1,"hi":2,"s":3},{"city":"NY","lo":3,"hi":4,"s":7},{"lo":5,"hi":5,"s":5}]"#,
            ),
            (
                "SELECT c.age > 30 AS older, COUNT(1) AS n FROM c GROUP BY c.age > 30",
                r#"[{"older":false,"n":2},{"older":true,"n":2},{"n":1}]"#,
            ),
            (
                "SELECT c.city, c.age, COUNT(1) AS n FROM c WHERE c.id < 3 GROUP BY c.city, c.age",
                r#"[{"city":"Seattle","age":30,"n":1},{"city":"Seattle","age":25,"n":1}]"#,
            ),
            ("SELECT c.city FROM c GROUP BY c.city", r#"[{"city":"Seattle"},{"city":"NY"},{}]"#),
            ("SELECT VALUE c.city FROM c GROUP BY c.city", r#"["Seattle","NY"]"#),
            ("SELECT COUNT(1) AS n FROM c WHERE false GROUP BY c.city", "[]"),
            (
                "SELECT VALUE {city: c.city, n: COUNT(1)} FROM c GROUP BY c.city",
                r#"[{"city":"Seattle","n":2},{"city":"NY","n":2},{"n":1}]"#,
            ),
            ("SELECT c.city, COUNT(1) AS n FROM c GROUP BY c.city ORDER BY c.city", r#"[{"city":"NY","n":2},{"city":"Seattle","n":2}]"#),
            ("SELECT c.city, COUNT(1) AS n FROM c GROUP BY c.city OFFSET 1 LIMIT 1", r#"[{"city":"NY","n":2}]"#),
            ("SELECT TOP 1 c.city, COUNT(1) AS n FROM c GROUP BY c.city", r#"[{"city":"Seattle","n":2}]"#),
            ("SELECT DISTINCT VALUE COUNT(1) FROM c GROUP BY c.city", "[2,1]"),
        ],
    );
    // group-by page: the group of items without the field is the empty object
    let emp = vec![
        json!({"team": "Cloud software engineering", "capabilities": {}}),
        json!({"team": "Cloud software engineering", "capabilities": {"softwareDevelopment": "c-sharp"}}),
        json!({"team": "Cloud software engineering", "capabilities": {"softwareDevelopment": "javascript"}}),
        json!({"team": "Cloud software engineering", "capabilities": {"softwareDevelopment": "python"}}),
        json!({"team": "Cloud software engineering", "capabilities": {"softwareDevelopment": "python"}}),
        json!({"team": "Other", "capabilities": {"softwareDevelopment": "rust"}}),
    ];
    check_table(
        &emp,
        &[(
            r#"SELECT e.capabilities.softwareDevelopment AS developmentLang FROM employees e WHERE e.team = "Cloud software engineering" GROUP BY e.capabilities.softwareDevelopment"#,
            r#"[{},{"developmentLang":"c-sharp"},{"developmentLang":"javascript"},{"developmentLang":"python"}]"#,
        )],
    );
    // number keys 1 and 1.0 are one group; "1" is another
    let k = vec![
        json!({"k": 1}),
        json!({"k": 1.0}),
        json!({"k": "1"}),
        json!({"k": [1]}),
        json!({"k": [1.0]}),
        json!({"k": {"a": 1, "b": 2}}),
        json!({"k": {"b": 2, "a": 1}}),
    ];
    check_table(&k, &[("SELECT VALUE COUNT(1) FROM c GROUP BY c.k", "[2,1,2,2]")]);
}

#[test]
fn order_by_cross_type() {
    // order: undefined < null < boolean < number < string < array < object
    let d = vec![
        json!({"id": "obj", "v": {"a": 1}}),
        json!({"id": "str", "v": "a"}),
        json!({"id": "num", "v": 3}),
        json!({"id": "true", "v": true}),
        json!({"id": "null", "v": null}),
        json!({"id": "arr", "v": [1]}),
        json!({"id": "false", "v": false}),
        json!({"id": "undef"}),
        json!({"id": "num2", "v": 1.5}),
        json!({"id": "str2", "v": "B"}),
    ];
    check_table(
        &d,
        &[
            ("SELECT VALUE c.id FROM c ORDER BY c.v", r#"["null","false","true","num2","num","str2","str","arr","obj"]"#),
            ("SELECT VALUE c.id FROM c ORDER BY c.v DESC", r#"["obj","arr","str","str2","num","num2","true","false","null"]"#),
            ("SELECT VALUE c.id FROM c ORDER BY c.v ASC", r#"["null","false","true","num2","num","str2","str","arr","obj"]"#),
        ],
    );
    // undefined values: excluded by default (Cosmos behaviour with the default index) ...
    let mut opts = fixed_clock_opts();
    let q = azemu_cosmos_sql::parse("SELECT VALUE c.id FROM c ORDER BY c.v").unwrap();
    let got = q.execute(&d, &Default::default(), &opts).unwrap();
    assert!(!got.contains(&json!("undef")));
    // ... and sorted first when the switch is off
    opts.order_by_excludes_undefined = false;
    let got = q.execute(&d, &Default::default(), &opts).unwrap();
    assert_eq!(got[0], json!("undef"));
    assert_eq!(got.len(), 10);
    let q = azemu_cosmos_sql::parse("SELECT VALUE c.id FROM c ORDER BY c.v DESC").unwrap();
    let got = q.execute(&d, &Default::default(), &opts).unwrap();
    assert_eq!(got.last().unwrap(), &json!("undef"));
    let _ = EvalOptions::default();
}

#[test]
fn order_by_forms() {
    let p = people();
    check_table(
        &p,
        &[
            ("SELECT VALUE c.id FROM c ORDER BY c.id DESC", "[5,4,3,2,1]"),
            ("SELECT VALUE c.id FROM c ORDER BY c.age", "[2,1,5,3,4]"), // 25, 30, 35, 40, "x"
            ("SELECT VALUE c.id FROM c ORDER BY c.age DESC", "[4,3,5,1,2]"),
            ("SELECT VALUE c.id FROM c ORDER BY c.city, c.age DESC", "[4,3,1,2]"), // id 5 has no city: excluded; "x" sorts after 40
            ("SELECT VALUE c.id FROM c ORDER BY c.city DESC, c.id", "[1,2,3,4]"),
            ("SELECT VALUE c.id FROM c ORDER BY c.score", "[3,4,5,1,2]"), // null, 5, 7, 10, 20.5
            ("SELECT VALUE c.id FROM c ORDER BY c.age * -1", "[3,5,1,2]"), // "x" * -1 is undefined: that row is excluded
            ("SELECT VALUE c.id FROM c WHERE IS_NUMBER(c.age) ORDER BY c.age * -1", "[3,5,1,2]"),
            ("SELECT VALUE c.id FROM c WHERE c.id < 4 ORDER BY LENGTH(c.city) DESC, c.id DESC", "[2,1,3]"),
            ("SELECT TOP 2 VALUE c.id FROM c ORDER BY c.id DESC", "[5,4]"),
            ("SELECT VALUE c.id FROM c ORDER BY c.id OFFSET 1 LIMIT 2", "[2,3]"),
            ("SELECT c.id, c.city FROM c WHERE c.id < 3 ORDER BY c.city ASC", r#"[{"id":1,"city":"Seattle"},{"id":2,"city":"Seattle"}]"#),
            ("SELECT DISTINCT VALUE c.city FROM c ORDER BY c.city DESC", r#"["Seattle","NY"]"#),
            (
                "SELECT * FROM c WHERE c.id < 3 ORDER BY c.age",
                r#"[{"id":2,"city":"Seattle","age":25,"score":20.5},{"id":1,"city":"Seattle","age":30,"score":10}]"#,
            ),
            ("SELECT VALUE c.id FROM c ORDER BY c.id ASC, c.age DESC", "[1,2,3,4,5]"),
            ("SELECT VALUE c.id FROM c ORDER BY ABS(c.id - 3), c.id", "[3,2,4,1,5]"),
            ("select value c.id from c order by c.id desc", "[5,4,3,2,1]"),
        ],
    );
    // stability: ties keep input order
    let ties = vec![json!({"i": 1, "k": 1}), json!({"i": 2, "k": 0}), json!({"i": 3, "k": 1}), json!({"i": 4, "k": 0})];
    check_table(
        &ties,
        &[("SELECT VALUE c.i FROM c ORDER BY c.k", "[2,4,1,3]"), ("SELECT VALUE c.i FROM c ORDER BY c.k DESC", "[1,3,2,4]")],
    );
    // numbers: integers and floats compare by value; strings by code point
    let n = vec![json!({"v": 10}), json!({"v": 2}), json!({"v": 1.5}), json!({"v": -3}), json!({"v": 2.0}), json!({"v": 1e20})];
    check_table(&n, &[("SELECT VALUE c.v FROM c ORDER BY c.v", "[-3,1.5,2,2.0,10,1e20]")]);
    let s = vec![json!({"v": "b"}), json!({"v": "B"}), json!({"v": "a"}), json!({"v": ""}), json!({"v": "é"}), json!({"v": "ab"})];
    check_table(&s, &[("SELECT VALUE c.v FROM c ORDER BY c.v", r#"["","B","a","ab","b","é"]"#)]);
    // ORDER BY on joined values
    let j = vec![json!({"id": 1, "t": [3, 1]}), json!({"id": 2, "t": [2]})];
    check_table(&j, &[("SELECT c.id, x FROM c JOIN x IN c.t ORDER BY x", r#"[{"id":1,"x":1},{"id":2,"x":2},{"id":1,"x":3}]"#)]);
}

#[test]
fn distinct_top_offset_limit() {
    let d = vec![
        json!({"a": 1, "b": "x"}),
        json!({"a": 1, "b": "x"}),
        json!({"a": 2, "b": "x"}),
        json!({"a": 1, "b": "y"}),
        json!({"b": "x"}),
        json!({"a": 1.0, "b": "x"}),
    ];
    check_table(
        &d,
        &[
            ("SELECT DISTINCT VALUE c.a FROM c", "[1,2]"),
            ("SELECT DISTINCT c.a FROM c", r#"[{"a":1},{"a":2},{}]"#),
            ("SELECT DISTINCT c.a, c.b FROM c", r#"[{"a":1,"b":"x"},{"a":2,"b":"x"},{"a":1,"b":"y"},{"b":"x"}]"#),
            ("SELECT DISTINCT VALUE c FROM c", r#"[{"a":1,"b":"x"},{"a":2,"b":"x"},{"a":1,"b":"y"},{"b":"x"}]"#),
            ("SELECT DISTINCT VALUE [c.a, c.b] FROM c", r#"[[1,"x"],[2,"x"],[1,"y"],["x"]]"#),
            ("SELECT DISTINCT TOP 2 VALUE c.b FROM c", r#"["x","y"]"#),
            ("SELECT DISTINCT VALUE {\"b\": c.b, \"a\": c.a} FROM c", r#"[{"b":"x","a":1},{"b":"x","a":2},{"b":"y","a":1},{"b":"x"}]"#),
            ("SELECT TOP 3 VALUE c.a FROM c", "[1,1,2]"),
            ("SELECT TOP 100 VALUE c.a FROM c WHERE c.a = 2", "[2]"),
            ("SELECT TOP (2) VALUE c.a FROM c", "[1,1]"),
            ("SELECT VALUE c.a FROM c OFFSET 2 LIMIT 2", "[2,1]"),
            ("SELECT VALUE c.a FROM c OFFSET 0 LIMIT 0", "[]"),
            ("SELECT VALUE c.a FROM c OFFSET 100 LIMIT 3", "[]"),
            ("SELECT VALUE c.a FROM c WHERE c.a = 1 OFFSET 1 LIMIT 1", "[1]"),
            ("SELECT DISTINCT VALUE c.a FROM c OFFSET 1 LIMIT 5", "[2]"),
            ("SELECT VALUE c.b FROM c ORDER BY c.b OFFSET 3 LIMIT 3", r#"["x","x","y"]"#),
        ],
    );
}
