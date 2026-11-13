//! Results over shared documents (`Query::execute_shared`) and the streaming stages: the same answers as the
//! documented semantics, whatever shape the query has.

use azemu_cosmos_sql::{parse, EvalOptions, Item, Params};
use serde_json::{json, Value};
use std::sync::Arc;

fn docs() -> Vec<Arc<Value>> {
    vec![
        json!({"id": "a", "n": 3, "g": "x", "tags": ["t1", "t2"], "o": {"k": [10, 20, 30]}}),
        json!({"id": "b", "n": 1, "g": "y", "tags": [], "o": {"k": [5]}}),
        json!({"id": "c", "n": 2, "g": "x", "tags": ["t2", "t3", "t4"]}),
        json!({"id": "d", "n": 2, "g": "y", "tags": ["t1"], "o": {"k": []}}),
        json!({"id": "e", "g": "x", "tags": ["t5"]}),
    ]
    .into_iter()
    .map(Arc::new)
    .collect()
}

fn run(sql: &str) -> Vec<Value> {
    let q = parse(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    q.execute_shared(&docs(), &Params::new(), &EvalOptions::default())
        .unwrap_or_else(|e| panic!("{sql}: {}", e.message))
        .into_iter()
        .map(Item::into_value)
        .collect()
}

fn ids(sql: &str) -> Vec<String> {
    run(sql).iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

#[test]
fn star_and_value_c_return_the_documents_as_they_are() {
    let d = docs();
    let q = parse("SELECT * FROM c WHERE c.n = 2").unwrap();
    let r = q.execute_shared(&d, &Params::new(), &EvalOptions::default()).unwrap();
    assert_eq!(r.len(), 2);
    for (item, want) in r.iter().zip([&d[2], &d[3]]) {
        assert!(matches!(item, Item::Shared(a) if Arc::ptr_eq(a, want)));
    }
    let q = parse("SELECT VALUE c FROM c WHERE c.g = 'x' ORDER BY c.id DESC").unwrap();
    let r = q.execute_shared(&d, &Params::new(), &EvalOptions::default()).unwrap();
    assert!(matches!(&r[0], Item::Shared(a) if Arc::ptr_eq(a, &d[4])));
    // FROM root AS c, and through a subquery in FROM
    assert_eq!(run("SELECT VALUE t.id FROM (SELECT * FROM c WHERE c.n = 3) t"), vec![json!("a")]);
    assert_eq!(run("SELECT VALUE t FROM (SELECT * FROM c WHERE c.n = 3) t")[0]["id"], json!("a"));
    assert_eq!(run("SELECT VALUE COUNT(1) FROM (SELECT VALUE c FROM c WHERE c.g = 'y') t"), vec![json!(2)]);
}

#[test]
fn a_bare_alias_is_a_full_value_where_an_expression_needs_one() {
    assert_eq!(run("SELECT VALUE c.id FROM c WHERE c = c AND IS_OBJECT(c) AND c.id = 'a'"), vec![json!("a")]);
    assert_eq!(run("SELECT VALUE c FROM c WHERE c.id = 'b'")[0]["o"], json!({"k": [5]}));
    assert_eq!(run("SELECT c AS doc FROM c WHERE c.id = 'b'")[0]["doc"]["tags"], json!([]));
    assert_eq!(run("SELECT VALUE [c.id, c.n] FROM c WHERE c.id = 'a'"), vec![json!(["a", 3])]);
    assert_eq!(run("SELECT VALUE ARRAY(SELECT VALUE c)  FROM c WHERE c.id = 'b'")[0][0]["id"], json!("b"));
    assert_eq!(run("SELECT VALUE (SELECT VALUE c.id)  FROM c WHERE c.id = 'b'"), vec![json!("b")]);
    assert_eq!(run("SELECT VALUE c.o.k[1] FROM c WHERE c.id = 'a'"), vec![json!(20)]);
    assert_eq!(run("SELECT VALUE c['o']['k'][0] FROM c WHERE c.id = 'a'"), vec![json!(10)]);
    // undefined stays undefined: no row for a missing path, null for a null
    assert_eq!(run("SELECT VALUE c.o.k[7] FROM c WHERE c.id = 'a'"), Vec::<Value>::new());
    assert_eq!(run("SELECT VALUE c.nope.deeper[0] FROM c"), Vec::<Value>::new());
    assert_eq!(run("SELECT VALUE c.id.x FROM c"), Vec::<Value>::new());
    assert_eq!(run("SELECT VALUE c.tags[0].x FROM c"), Vec::<Value>::new());
    assert_eq!(run("SELECT VALUE c.id FROM c WHERE c.o.k[0] = 5"), vec![json!("b")]);
}

#[test]
fn joins_and_from_paths_read_the_document_in_place() {
    assert_eq!(ids("SELECT VALUE c.id FROM c JOIN t IN c.tags WHERE t = 't2'"), vec!["a", "c"]);
    assert_eq!(run("SELECT VALUE COUNT(1) FROM c JOIN t IN c.tags"), vec![json!(7)]);
    assert_eq!(
        run("SELECT VALUE t FROM c.tags t"),
        vec![json!(["t1", "t2"]), json!([]), json!(["t2", "t3", "t4"]), json!(["t1"]), json!(["t5"])]
    );
    assert_eq!(run("SELECT VALUE k FROM k IN c.o.k WHERE k > 5"), vec![json!(10), json!(20), json!(30)]);
    assert_eq!(run("SELECT VALUE [t, u] FROM c JOIN t IN c.tags JOIN u IN c.tags WHERE c.id = 'a' AND t < u"), vec![json!(["t1", "t2"])]);
    assert_eq!(
        run("SELECT c.id, ARRAY(SELECT VALUE t FROM t IN c.tags WHERE t != 't2') AS rest FROM c WHERE c.id = 'c'"),
        vec![json!({"id": "c", "rest": ["t3", "t4"]})]
    );
    assert_eq!(run("SELECT VALUE COUNT(1) FROM c WHERE EXISTS(SELECT VALUE t FROM t IN c.tags WHERE t = 't1')"), vec![json!(2)]);
    assert_eq!(run("SELECT VALUE COUNT(1) FROM c WHERE NOT EXISTS(SELECT VALUE t FROM t IN c.tags)"), vec![json!(1)]);
}

#[test]
fn top_and_limit_stop_early_and_keep_the_order() {
    assert_eq!(ids("SELECT TOP 2 VALUE c.id FROM c"), vec!["a", "b"]);
    assert_eq!(ids("SELECT VALUE c.id FROM c OFFSET 1 LIMIT 2"), vec!["b", "c"]);
    assert_eq!(ids("SELECT VALUE c.id FROM c OFFSET 4 LIMIT 10"), vec!["e"]);
    assert_eq!(ids("SELECT VALUE c.id FROM c OFFSET 9 LIMIT 10"), Vec::<String>::new());
    assert_eq!(ids("SELECT TOP 0 VALUE c.id FROM c"), Vec::<String>::new());
    assert_eq!(ids("SELECT TOP 2 VALUE c.id FROM c WHERE c.g = 'x'"), vec!["a", "c"]);
    // ORDER BY with a count: ties keep the order they arrived in, also when the list is cut while it is built
    assert_eq!(ids("SELECT TOP 3 VALUE c.id FROM c ORDER BY c.n DESC"), vec!["a", "c", "d"]);
    assert_eq!(ids("SELECT VALUE c.id FROM c ORDER BY c.n OFFSET 1 LIMIT 2"), vec!["c", "d"]);
    // DISTINCT before TOP
    assert_eq!(run("SELECT DISTINCT TOP 2 VALUE c.g FROM c"), vec![json!("x"), json!("y")]);
    assert_eq!(run("SELECT DISTINCT VALUE c.n FROM c ORDER BY c.n DESC"), vec![json!(3), json!(2), json!(1)]);
    // a long ORDER BY ... TOP: the list is cut several times on the way
    let many: Vec<Arc<Value>> = (0..5000).map(|i| Arc::new(json!({"id": i, "k": (i * 7919) % 1000}))).collect();
    let q = parse("SELECT TOP 5 VALUE c.id FROM c ORDER BY c.k, c.id").unwrap();
    let got: Vec<Value> =
        q.execute_shared(&many, &Params::new(), &EvalOptions::default()).unwrap().into_iter().map(Item::into_value).collect();
    let mut want: Vec<(i64, i64)> = (0..5000i64).map(|i| ((i * 7919) % 1000, i)).collect();
    want.sort();
    assert_eq!(got, want.iter().take(5).map(|(_, i)| json!(i)).collect::<Vec<_>>());
    let q = parse("SELECT TOP 5 VALUE c.id FROM c ORDER BY c.k DESC").unwrap();
    let got: Vec<Value> =
        q.execute_shared(&many, &Params::new(), &EvalOptions::default()).unwrap().into_iter().map(Item::into_value).collect();
    let mut want: Vec<(i64, i64)> = (0..5000i64).map(|i| ((i * 7919) % 1000, i)).collect();
    want.sort_by_key(|a| std::cmp::Reverse(a.0)); // stable: equal keys stay in id order
    assert_eq!(got, want.iter().take(5).map(|(_, i)| json!(i)).collect::<Vec<_>>());
}

#[test]
fn groups_and_aggregates_keep_running_totals() {
    assert_eq!(run("SELECT VALUE COUNT(1) FROM c"), vec![json!(5)]);
    assert_eq!(run("SELECT VALUE COUNT(c.n) FROM c"), vec![json!(4)]);
    assert_eq!(run("SELECT VALUE SUM(c.n) FROM c"), vec![json!(8)]);
    assert_eq!(run("SELECT VALUE AVG(c.n) FROM c"), vec![json!(2)]);
    assert_eq!(run("SELECT VALUE MIN(c.n) FROM c"), vec![json!(1)]);
    assert_eq!(run("SELECT VALUE MAX(c.id) FROM c"), vec![json!("e")]);
    assert_eq!(run("SELECT VALUE SUM(c.id) FROM c"), Vec::<Value>::new());
    // no rows: one group with count 0, nothing for SUM
    assert_eq!(run("SELECT VALUE COUNT(1) FROM c WHERE c.n > 99"), vec![json!(0)]);
    assert_eq!(run("SELECT VALUE SUM(c.n) FROM c WHERE c.n > 99"), Vec::<Value>::new());
    assert_eq!(run("SELECT COUNT(1) AS n, MAX(c.n) AS m FROM c WHERE c.n > 99"), vec![json!({"n": 0})]);
    assert_eq!(
        run("SELECT c.g, COUNT(1) AS n, SUM(c.n) AS s FROM c GROUP BY c.g"),
        vec![json!({"g": "x", "n": 3, "s": 5}), json!({"g": "y", "n": 2, "s": 3})]
    );
    assert_eq!(run("SELECT c.g, COUNT(1) AS n FROM c WHERE c.n > 99 GROUP BY c.g"), Vec::<Value>::new());
    assert_eq!(run("SELECT VALUE COUNT(1) FROM c JOIN t IN c.tags GROUP BY c.g"), vec![json!(6), json!(1)]);
    assert_eq!(run("SELECT c.g, COUNT(1) AS n FROM c GROUP BY c.g ORDER BY COUNT(1) DESC")[0]["g"], json!("x"));
    assert_eq!(run("SELECT VALUE COUNT(1) + MAX(c.n) * 10 FROM c"), vec![json!(35)]);
    assert_eq!(run("SELECT VALUE (SELECT VALUE COUNT(1) FROM t IN c.tags) FROM c WHERE c.id = 'c'"), vec![json!(3)]);
}
