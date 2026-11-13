//! Query plan facts (`info`, `needs_query_plan`), the rewritten queries the SDKs expect, a
//! simulation of the Python SDK's cross-partition pipeline, and partition key extraction.
//! The SDK behaviour is taken from azure/cosmos/_execution_context/*.py (document_producer,
//! endpoint_component, aggregators, execution_dispatcher, query_execution_info).

mod common;
use azemu_cosmos_sql::{parse, AggregateKind, DistinctType, EvalOptions, Params, SortOrder};
use common::*;
use serde_json::{json, Value};
use std::cmp::Ordering;

fn p0() -> Params {
    Params::new()
}

fn people() -> Vec<Value> {
    vec![
        json!({"id": "a", "city": "Seattle", "age": 30, "score": 1.5}),
        json!({"id": "b", "city": "NY", "age": 25, "score": 2}),
        json!({"id": "c", "city": "Seattle", "age": 40}),
        json!({"id": "d", "city": "NY", "age": 25, "score": 4}),
        json!({"id": "e", "age": 35, "score": 5}),
        json!({"id": "f", "city": "LA", "age": "old", "score": 6}),
        json!({"id": "g", "city": "LA", "age": 31, "score": 7}),
    ]
}

#[test]
fn needs_query_plan_table() {
    for (sql, expect) in [
        ("SELECT * FROM c", false),
        ("SELECT c.id FROM c WHERE c.x = 1", false),
        ("SELECT VALUE c.id FROM c JOIN t IN c.tags WHERE t = 1", false),
        ("SELECT VALUE (SELECT VALUE COUNT(1) FROM x IN c.arr) FROM c", false),
        ("SELECT VALUE ARRAY(SELECT VALUE x FROM x IN c.arr ORDER BY x) FROM c", false),
        ("SELECT * FROM c WHERE EXISTS(SELECT VALUE 1 FROM x IN c.a)", false),
        ("SELECT 1", false),
        ("SELECT * FROM c ORDER BY c.id", true),
        ("SELECT VALUE COUNT(1) FROM c", true),
        ("SELECT SUM(c.x) AS s FROM c", true),
        ("SELECT c.city FROM c GROUP BY c.city", true),
        ("SELECT DISTINCT c.city FROM c", true),
        ("SELECT DISTINCT VALUE c.city FROM c", true),
        ("SELECT * FROM c OFFSET 1 LIMIT 2", true),
        ("SELECT TOP 3 * FROM c", true),
        ("SELECT TOP @n * FROM c", true),
        ("SELECT VALUE MAX(c.x) FROM c WHERE c.y = 1", true),
        ("SELECT VALUE COUNT(1) FROM (SELECT DISTINCT VALUE c.x FROM c)", true),
        ("SELECT * FROM (SELECT * FROM c) t", false),
        ("SELECT * FROM (SELECT TOP 1 * FROM c) t", true),
    ] {
        assert_eq!(parse(sql).unwrap().needs_query_plan(), expect, "{sql}");
    }
}

#[test]
fn info_fields() {
    let i = parse("SELECT * FROM c").unwrap().info(&p0());
    assert_eq!(i.distinct_type, DistinctType::None);
    assert_eq!((i.top, i.offset, i.limit), (None, None, None));
    assert!(i.order_by.is_empty() && i.aggregates.is_empty() && i.group_by_expressions.is_empty());
    assert!(!i.has_select_value && !i.has_non_streaming_order_by);
    assert_eq!(i.rewritten_query, "");

    let i = parse("SELECT TOP 5 c.id FROM c").unwrap().info(&p0());
    assert_eq!(i.top, Some(5));
    assert_eq!(i.rewritten_query, "");
    let mut params = Params::new();
    params.insert("@n", json!(7));
    let i = parse("SELECT TOP @n c.id FROM c").unwrap().info(&params);
    assert_eq!(i.top, Some(7));
    let i = parse("SELECT TOP @n c.id FROM c").unwrap().info(&p0());
    assert_eq!(i.top, None);

    let i = parse("SELECT * FROM c OFFSET 5 LIMIT 10").unwrap().info(&p0());
    assert_eq!((i.offset, i.limit, i.top), (Some(5), Some(10), None));
    assert_eq!(i.rewritten_query, "SELECT TOP 15 *\nFROM c");
    let i = parse("select distinct c.a, c.b from c where c.x = 1 offset 2 limit 3").unwrap().info(&p0());
    assert_eq!(i.rewritten_query, "SELECT DISTINCT TOP 5 c.a, c.b\nfrom c\nWHERE c.x = 1");
    assert_eq!(i.distinct_type, DistinctType::Unordered);

    let i = parse("SELECT DISTINCT VALUE c.x FROM c ORDER BY c.x").unwrap().info(&p0());
    assert_eq!(i.distinct_type, DistinctType::Ordered);
    assert!(i.has_select_value);

    let i = parse("SELECT c.id, c.name FROM c WHERE c.age > 3 ORDER BY c.age DESC, c.name").unwrap().info(&p0());
    assert_eq!(i.order_by, vec![SortOrder::Descending, SortOrder::Ascending]);
    assert_eq!(i.order_by_expressions, vec!["c.age", "c.name"]);
    assert_eq!(
        i.rewritten_query,
        "SELECT c._rid, [{\"item\": c.age}, {\"item\": c.name}] AS orderByItems, {\"id\": c.id, \"name\": c.name} AS payload\nFROM c\nWHERE ({documentdb-formattableorderbyquery-filter}) AND (c.age > 3)\nORDER BY c.age DESC, c.name ASC"
    );
    let i = parse("SELECT * FROM c ORDER BY c.age").unwrap().info(&p0());
    assert_eq!(i.rewritten_query, "SELECT c._rid, [{\"item\": c.age}] AS orderByItems, c AS payload\nFROM c\nWHERE ({documentdb-formattableorderbyquery-filter})\nORDER BY c.age ASC");
    let i = parse("SELECT VALUE c.id FROM c ORDER BY c[\"a b\"]").unwrap().info(&p0());
    assert_eq!(i.order_by_expressions, vec!["c[\"a b\"]"]);
    assert!(i.rewritten_query.contains("c.id AS payload"));
    let i = parse("SELECT TOP 3 VALUE c.id FROM c ORDER BY c.a").unwrap().info(&p0());
    assert!(i.rewritten_query.starts_with("SELECT TOP 3 c._rid,"), "{}", i.rewritten_query);
    let i = parse("SELECT VALUE c.id FROM c ORDER BY c.a OFFSET 2 LIMIT 3").unwrap().info(&p0());
    assert!(i.rewritten_query.starts_with("SELECT TOP 5 c._rid,"), "{}", i.rewritten_query);
    assert!(!i.rewritten_query.to_uppercase().contains("OFFSET"));
    let i = parse("SELECT DISTINCT VALUE c.x FROM c ORDER BY c.x").unwrap().info(&p0());
    // ordered DISTINCT without TOP / OFFSET LIMIT: all rows with `_rid` (the SDKs remove the repeats; the Java SDK needs the `_rid`)
    assert_eq!(i.rewritten_query, "SELECT c._rid, [{\"item\": c.x}] AS orderByItems, c.x AS payload\nFROM c\nWHERE ({documentdb-formattableorderbyquery-filter})\nORDER BY c.x ASC");
    // with TOP the partition must remove the repeats before it counts
    let i = parse("SELECT DISTINCT TOP 2 VALUE c.x FROM c ORDER BY c.x").unwrap().info(&p0());
    assert_eq!(i.rewritten_query, "SELECT DISTINCT TOP 2 VALUE {\"orderByItems\": [{\"item\": c.x}], \"payload\": c.x}\nFROM c\nWHERE ({documentdb-formattableorderbyquery-filter})\nORDER BY c.x ASC");

    let i = parse("SELECT VALUE COUNT(1) FROM c").unwrap().info(&p0());
    assert_eq!(i.aggregates, vec![AggregateKind::Count]);
    assert!(i.has_select_value);
    assert_eq!(i.rewritten_query, "SELECT VALUE [{\"item\": COUNT(1)}]\nFROM c");
    let i = parse("SELECT VALUE AVG(c.x) FROM c WHERE c.y = 1").unwrap().info(&p0());
    assert_eq!(i.aggregates, vec![AggregateKind::Average]);
    assert_eq!(i.rewritten_query, "SELECT VALUE [{\"item\": {\"sum\": SUM(c.x), \"count\": COUNT(c.x)}}]\nFROM c\nWHERE c.y = 1");
    for (f, k) in [("SUM", AggregateKind::Sum), ("MIN", AggregateKind::Min), ("MAX", AggregateKind::Max), ("COUNT", AggregateKind::Count)] {
        let i = parse(&format!("SELECT VALUE {f}(c.x) FROM c")).unwrap().info(&p0());
        assert_eq!(i.aggregates, vec![k]);
    }
    let i = parse("SELECT COUNT(1) AS n, SUM(c.x) AS s FROM c").unwrap().info(&p0());
    assert_eq!(i.aggregates, vec![AggregateKind::Count, AggregateKind::Sum]);
    assert!(!i.has_select_value);

    let i = parse("SELECT c.city, COUNT(1) AS n, AVG(c.age) FROM c WHERE c.age > 1 GROUP BY c.city").unwrap().info(&p0());
    assert_eq!(i.group_by_expressions, vec!["c.city"]);
    assert_eq!(i.group_by_aliases, vec!["city", "n", "$1"]);
    assert_eq!(
        i.group_by_alias_to_aggregate_type,
        vec![("city".to_string(), None), ("n".to_string(), Some(AggregateKind::Count)), ("$1".to_string(), Some(AggregateKind::Average))]
    );
    assert!(i.aggregates.is_empty());
    assert_eq!(
        i.rewritten_query,
        "SELECT [{\"item\": c.city}] AS groupByItems, {\"city\": c.city, \"n\": {\"item\": COUNT(1)}, \"$1\": {\"item\": {\"sum\": SUM(c.age), \"count\": COUNT(c.age)}}} AS payload\nFROM c\nWHERE c.age > 1\nGROUP BY c.city"
    );
}

#[test]
fn info_json_shape() {
    let i = parse("SELECT c.city, COUNT(1) AS n FROM c GROUP BY c.city").unwrap().info(&p0());
    let j = i.to_json();
    for key in [
        "distinctType",
        "top",
        "offset",
        "limit",
        "orderBy",
        "orderByExpressions",
        "groupByExpressions",
        "groupByAliases",
        "aggregates",
        "groupByAliasToAggregateType",
        "rewrittenQuery",
        "hasSelectValue",
        "hasNonStreamingOrderBy",
    ] {
        assert!(j.get(key).is_some(), "missing {key}");
    }
    assert_eq!(j["distinctType"], "None");
    assert_eq!(j["top"], Value::Null);
    assert_eq!(j["groupByAliasToAggregateType"], json!({"city": null, "n": "Count"}));
    let i = parse("SELECT DISTINCT VALUE COUNT(1) FROM c ORDER BY c.x DESC").unwrap();
    let j = i.info(&p0()).to_json();
    assert_eq!(j["orderBy"], json!(["Descending"]));
    assert_eq!(j["distinctType"], "Ordered");
    assert_eq!(j["aggregates"], json!(["Count"]));
    let j = parse("SELECT TOP 4 VALUE AVG(c.x) FROM c").unwrap().info(&p0()).to_json();
    assert_eq!(j["aggregates"], json!(["Average"]));
    assert_eq!(j["top"], 4);
    assert_eq!(j["hasSelectValue"], true);
    // the Python SDK only reads these paths; an empty rewritten query makes it use the original text
    assert_eq!(parse("SELECT * FROM c").unwrap().info(&p0()).to_json()["rewrittenQuery"], "");
}

// ---------------------------------------------------------------- SDK simulation

/// The Python SDK's `_OrderByHelper.compare` over `{"item": v}` entries.
fn sdk_compare(a: &Value, b: &Value) -> Ordering {
    fn ord(v: &Value) -> u8 {
        match v.get("item") {
            None => 0,
            Some(Value::Null) => 1,
            Some(Value::Bool(_)) => 2,
            Some(Value::Number(_)) => 4,
            Some(Value::String(_)) => 5,
            Some(other) => panic!("the SDK raises TypeError for {other}"),
        }
    }
    let (oa, ob) = (ord(a), ord(b));
    if oa != ob {
        return oa.cmp(&ob);
    }
    match (a.get("item"), b.get("item")) {
        (Some(Value::Bool(x)), Some(Value::Bool(y))) => x.cmp(y),
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64().partial_cmp(&y.as_f64()).unwrap(),
        (Some(Value::String(x)), Some(Value::String(y))) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// Runs a query like the Python SDK does over `parts` partitions: ask for the plan, send the
/// rewritten query to every partition, merge, then apply aggregate / distinct / offset / top.
fn sdk_run(sql: &str, docs: &[Value], parts: usize) -> Vec<Value> {
    let q = parse(sql).unwrap();
    let params = Params::new();
    let opts = fixed_clock_opts();
    let info = q.info(&params);
    let chunks: Vec<&[Value]> = if docs.is_empty() { vec![docs] } else { docs.chunks(docs.len().div_ceil(parts)).collect() };
    let per_part: Vec<Vec<Value>> = chunks.iter().map(|c| q.execute_rewritten(*c, &params, &opts).unwrap()).collect();

    let mut out: Vec<Value>;
    if !info.order_by.is_empty() {
        // k-way merge by orderByItems (stable by partition order), keep payloads
        let mut all: Vec<(usize, Value)> =
            per_part.into_iter().enumerate().flat_map(|(p, rows)| rows.into_iter().map(move |r| (p, r))).collect();
        all.sort_by(|(pa, a), (pb, b)| {
            let (ia, ib) = (a["orderByItems"].as_array().unwrap(), b["orderByItems"].as_array().unwrap());
            for (i, (x, y)) in ia.iter().zip(ib).enumerate() {
                let o = sdk_compare(x, y);
                let o = if info.order_by[i] == SortOrder::Descending { o.reverse() } else { o };
                if o != Ordering::Equal {
                    return o;
                }
            }
            pa.cmp(pb)
        });
        out = all.into_iter().map(|(_, r)| r.get("payload").cloned().unwrap_or(Value::Null)).collect();
    } else if !info.aggregates.is_empty() {
        // _QueryExecutionAggregateEndpointComponent
        let mut results = Vec::new();
        for (idx, kind) in info.aggregates.iter().enumerate() {
            let mut count = 0i64;
            let mut sum: Option<f64> = None;
            let mut avg: Option<(f64, i64)> = None;
            let mut best: Option<Value> = None;
            for rows in &per_part {
                for res in rows {
                    let item = &res.as_array().unwrap()[idx];
                    if !item.is_object() || item.as_object().unwrap().is_empty() {
                        continue; // `{}`: nothing to aggregate in this partition
                    }
                    let v = &item["item"];
                    if *kind == AggregateKind::Average && v.get("sum").is_none() {
                        continue; // _AverageAggregator ignores items without "sum"
                    }
                    match kind {
                        AggregateKind::Count => count += v.as_i64().unwrap(),
                        AggregateKind::Sum => sum = Some(sum.unwrap_or(0.0) + v.as_f64().unwrap()),
                        AggregateKind::Average => {
                            let (s, c) = avg.unwrap_or((0.0, 0));
                            avg = Some((s + v["sum"].as_f64().unwrap(), c + v["count"].as_i64().unwrap()));
                        }
                        AggregateKind::Min | AggregateKind::Max => {
                            best = Some(match best.take() {
                                None => v.clone(),
                                Some(b) => {
                                    let o = sdk_compare(&json!({"item": v}), &json!({"item": b}));
                                    if (*kind == AggregateKind::Min && o == Ordering::Less)
                                        || (*kind == AggregateKind::Max && o == Ordering::Greater)
                                    {
                                        v.clone()
                                    } else {
                                        b
                                    }
                                }
                            })
                        }
                    }
                }
            }
            results.push(match kind {
                AggregateKind::Count => Some(json!(count)),
                AggregateKind::Sum => sum.map(|s| json!(s)),
                AggregateKind::Average => avg.filter(|(_, c)| *c > 0).map(|(s, c)| json!(s / c as f64)),
                _ => best,
            });
        }
        out = results.into_iter().flatten().collect();
    } else {
        out = per_part.into_iter().flatten().collect();
    }
    if info.distinct_type == DistinctType::Ordered {
        out.dedup();
    } else if info.distinct_type == DistinctType::Unordered {
        let mut seen: Vec<Value> = Vec::new();
        out.retain(|v| {
            if seen.contains(v) {
                false
            } else {
                seen.push(v.clone());
                true
            }
        });
    }
    if let Some(o) = info.offset {
        out = out.into_iter().skip(o as usize).collect();
    }
    if let Some(t) = info.top {
        out.truncate(t as usize);
    }
    if let Some(l) = info.limit {
        out.truncate(l as usize);
    }
    out
}

#[test]
fn sdk_pipeline_matches_single_pass() {
    let docs = people();
    for sql in [
        "SELECT * FROM c ORDER BY c.age",
        "SELECT * FROM c ORDER BY c.age DESC",
        "SELECT c.id, c.age FROM c ORDER BY c.age, c.id DESC",
        "SELECT VALUE c.id FROM c ORDER BY c.score DESC",
        "SELECT VALUE c.id FROM c WHERE c.age > 26 ORDER BY c.city, c.age",
        "SELECT TOP 3 VALUE c.id FROM c ORDER BY c.score",
        "SELECT VALUE c.id FROM c ORDER BY c.score OFFSET 1 LIMIT 3",
        "SELECT DISTINCT VALUE c.age FROM c ORDER BY c.age",
        "SELECT DISTINCT VALUE c.city FROM c ORDER BY c.city DESC",
        "SELECT VALUE COUNT(1) FROM c",
        "SELECT VALUE COUNT(c.score) FROM c",
        "SELECT VALUE SUM(c.score) FROM c",
        "SELECT VALUE SUM(c.score) FROM c WHERE c.age > 26",
        "SELECT VALUE AVG(c.score) FROM c",
        "SELECT VALUE AVG(c.score) FROM c WHERE c.id = 'a'",
        "SELECT VALUE MIN(c.age) FROM c",
        "SELECT VALUE MAX(c.age) FROM c",
        "SELECT VALUE MAX(c.city) FROM c",
        "SELECT VALUE COUNT(1) FROM c WHERE c.age > 1000",
        "SELECT VALUE SUM(c.score) FROM c WHERE c.age > 1000",
        "SELECT VALUE MIN(c.score) FROM c WHERE c.age > 1000",
        "SELECT * FROM c OFFSET 2 LIMIT 3",
        "SELECT DISTINCT VALUE c.city FROM c",
        "SELECT TOP 4 * FROM c",
        "SELECT TOP 4 c.id FROM c WHERE c.age > 26",
    ] {
        let expected = parse(sql).unwrap().execute(&docs, &p0(), &fixed_clock_opts()).unwrap();
        for parts in [1, 2, 3, 7] {
            let got = sdk_run(sql, &docs, parts);
            assert_eq!(got, expected, "{sql} over {parts} partitions");
        }
    }
}

#[test]
fn rewritten_result_shapes() {
    let docs = people();
    let run_rw = |sql: &str| parse(sql).unwrap().execute_rewritten(&docs, &p0(), &fixed_clock_opts()).unwrap();
    // ORDER BY: {"orderByItems": [{"item": ..}], "payload": ..}; a missing item key means undefined
    let rows = run_rw("SELECT c.id FROM c ORDER BY c.age");
    assert_eq!(rows[0], json!({"orderByItems": [{"item": 25}], "payload": {"id": "b"}}));
    assert_eq!(rows.len(), 7);
    let mut o = fixed_clock_opts();
    o.order_by_excludes_undefined = false;
    let rows = parse("SELECT c.id FROM c ORDER BY c.score").unwrap().execute_rewritten(&docs, &p0(), &o).unwrap();
    assert_eq!(rows[0], json!({"orderByItems": [{}], "payload": {"id": "c"}}));
    // _rid is part of the row when the document has one
    let with_rid = vec![json!({"id": "x", "_rid": "abc==", "n": 1})];
    let rows = parse("SELECT * FROM c ORDER BY c.n").unwrap().execute_rewritten(&with_rid, &p0(), &fixed_clock_opts()).unwrap();
    assert_eq!(rows[0], json!({"_rid": "abc==", "orderByItems": [{"item": 1}], "payload": {"id": "x", "_rid": "abc==", "n": 1}}));
    // aggregates
    assert_eq!(run_rw("SELECT VALUE COUNT(1) FROM c"), vec![json!([{"item": 7}])]);
    assert_eq!(run_rw("SELECT VALUE SUM(c.score) FROM c"), vec![json!([{"item": 25.5}])]);
    assert_eq!(run_rw("SELECT VALUE AVG(c.score) FROM c"), vec![json!([{"item": {"sum": 25.5, "count": 6}}])]);
    assert_eq!(run_rw("SELECT VALUE MAX(c.age) FROM c"), vec![json!([{"item": "old"}])]);
    assert_eq!(run_rw("SELECT VALUE MIN(c.age) FROM c"), vec![json!([{"item": 25}])]);
    assert_eq!(run_rw("SELECT VALUE SUM(c.nothing) FROM c"), vec![json!([{}])]);
    // AVG without values: the SDK skips items that have no "sum" key
    assert_eq!(run_rw("SELECT VALUE AVG(c.nothing) FROM c"), vec![json!([{"item": {"count": 0}}])]);
    assert_eq!(run_rw("SELECT VALUE COUNT(1) FROM c WHERE false"), vec![json!([{"item": 0}])]);
    // GROUP BY
    let rows = run_rw("SELECT c.city, COUNT(1) AS n FROM c GROUP BY c.city");
    assert_eq!(rows[0], json!({"groupByItems": [{"item": "Seattle"}], "payload": {"city": "Seattle", "n": {"item": 2}}}));
    assert_eq!(rows[2], json!({"groupByItems": [{}], "payload": {"n": {"item": 1}}}));
    // no rewriting needed: the original query is executed
    assert_eq!(run_rw("SELECT VALUE c.id FROM c WHERE c.age = 25"), vec![json!("b"), json!("d")]);
    // the placeholder is accepted as `true`, also written by the gateway in other positions
    let q = parse("SELECT c.id FROM c WHERE ({documentdb-formattableorderbyquery-filter}) AND (c.age = 25)").unwrap();
    assert_eq!(q.execute(&docs, &p0(), &fixed_clock_opts()).unwrap(), vec![json!({"id": "b"}), json!({"id": "d"})]);
    // rewritten text is itself valid input for parse()
    for sql in [
        "SELECT * FROM c ORDER BY c.age DESC",
        "SELECT VALUE AVG(c.x) FROM c",
        "SELECT c.city, MAX(c.age) AS m FROM c GROUP BY c.city",
        "SELECT TOP 2 c.id FROM c ORDER BY c.age",
    ] {
        let info = parse(sql).unwrap().info(&p0());
        assert!(parse(&info.rewritten_query).is_ok(), "{}", info.rewritten_query);
    }
    // parameters survive rewriting
    let mut params = Params::new();
    params.insert("@a", json!(26));
    let q = parse("SELECT c.id FROM c WHERE c.age > @a ORDER BY c.age").unwrap();
    let rows = q.execute_rewritten(&docs, &params, &fixed_clock_opts()).unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["payload"], json!({"id": "a"}));
    let _ = EvalOptions::default();
}

fn pk(sql: &str, paths: &[&str], params: Value) -> Option<Vec<Value>> {
    let paths: Vec<String> = paths.iter().map(|s| s.to_string()).collect();
    parse(sql).unwrap().partition_key_filter(&paths, &Params::from_cosmos(&params))
}

#[test]
fn partition_key_filter_cases() {
    let n = json!([]);
    let some = |v: Vec<Value>| Some(v);
    assert_eq!(pk("SELECT * FROM c WHERE c.pk = 'a'", &["/pk"], n.clone()), some(vec![json!("a")]));
    assert_eq!(pk("SELECT * FROM c WHERE c.pk = 'a' AND c.x > 1", &["/pk"], n.clone()), some(vec![json!("a")]));
    assert_eq!(pk("SELECT * FROM c WHERE c.x > 1 AND c.pk = 'a'", &["/pk"], n.clone()), some(vec![json!("a")]));
    assert_eq!(pk("SELECT * FROM c WHERE 'a' = c.pk", &["/pk"], n.clone()), some(vec![json!("a")]));
    assert_eq!(pk("SELECT * FROM c WHERE c[\"pk\"] = 5", &["/pk"], n.clone()), some(vec![json!(5)]));
    assert_eq!(pk("SELECT * FROM c WHERE c.pk = true", &["/pk"], n.clone()), some(vec![json!(true)]));
    assert_eq!(pk("SELECT * FROM c WHERE c.pk = null", &["/pk"], n.clone()), some(vec![Value::Null]));
    assert_eq!(pk("SELECT * FROM c WHERE c.pk = 1.5", &["/pk"], n.clone()), some(vec![json!(1.5)]));
    assert_eq!(pk("SELECT * FROM c WHERE c.pk = @p", &["/pk"], json!([{"name": "@p", "value": "v"}])), some(vec![json!("v")]));
    assert_eq!(pk("SELECT * FROM c WHERE c.a.b = 1", &["/a/b"], n.clone()), some(vec![json!(1)]));
    assert_eq!(pk("SELECT * FROM c WHERE c.a.b = 1 AND c.t = 'x'", &["/t", "/a/b"], n.clone()), some(vec![json!("x"), json!(1)]));
    assert_eq!(pk("SELECT * FROM c WHERE c.pk = 'a' AND c.pk = 'a'", &["/pk"], n.clone()), some(vec![json!("a")]));
    assert_eq!(pk("SELECT * FROM c WHERE (c.pk = 'a' AND (c.x = 1))", &["/pk"], n.clone()), some(vec![json!("a")]));
    assert_eq!(pk("SELECT * FROM root r WHERE r.pk = 'a'", &["/pk"], n.clone()), some(vec![json!("a")]));
    assert_eq!(pk("SELECT VALUE c.id FROM c JOIN t IN c.tags WHERE c.pk = 'a' AND t = 1", &["/pk"], n.clone()), some(vec![json!("a")]));
    assert_eq!(pk("SELECT VALUE COUNT(1) FROM c WHERE c.pk = 'a' ORDER BY c.x", &["/pk"], n.clone()), some(vec![json!("a")]));
    // not pinned
    for (sql, paths) in [
        ("SELECT * FROM c", vec!["/pk"]),
        ("SELECT * FROM c WHERE c.x = 1", vec!["/pk"]),
        ("SELECT * FROM c WHERE c.pk = 'a' OR c.pk = 'b'", vec!["/pk"]),
        ("SELECT * FROM c WHERE c.pk = 'a' OR c.x = 1", vec!["/pk"]),
        ("SELECT * FROM c WHERE (c.pk = 'a' AND c.x = 1) OR c.y = 2", vec!["/pk"]),
        ("SELECT * FROM c WHERE c.pk IN ('a')", vec!["/pk"]),
        ("SELECT * FROM c WHERE c.pk > 'a'", vec!["/pk"]),
        ("SELECT * FROM c WHERE c.pk != 'a'", vec!["/pk"]),
        ("SELECT * FROM c WHERE NOT (c.pk = 'a')", vec!["/pk"]),
        ("SELECT * FROM c WHERE c.pk = 'a' AND c.pk = 'b'", vec!["/pk"]),
        ("SELECT * FROM c WHERE c.pk = c.other", vec!["/pk"]),
        ("SELECT * FROM c WHERE c.pk = [1]", vec!["/pk"]),
        ("SELECT * FROM c WHERE c.pk = c.x + 1", vec!["/pk"]),
        ("SELECT * FROM c WHERE c.pk = 'a'", vec![]),
        ("SELECT * FROM c WHERE c.pk = 'a'", vec!["/other"]),
        ("SELECT * FROM c WHERE c.pk = 'a'", vec!["/pk", "/pk2"]),
        ("SELECT * FROM c IN c.items WHERE c.pk = 1", vec!["/pk"]),
        ("SELECT * FROM c.inner i WHERE i.pk = 1", vec!["/pk"]),
        ("SELECT * FROM (SELECT * FROM c) t WHERE t.pk = 1", vec!["/pk"]),
        ("SELECT * FROM c WHERE c.PK = 'a'", vec!["/pk"]),
        ("SELECT * FROM c WHERE (SELECT VALUE c.pk) = 'a'", vec!["/pk"]),
        ("SELECT * FROM c WHERE c.pk = undefined", vec!["/pk"]),
    ] {
        assert_eq!(pk(sql, &paths, n.clone()), None, "{sql} {paths:?}");
    }
    assert_eq!(pk("SELECT * FROM c WHERE c.pk = @p", &["/pk"], json!([{"name": "@p", "value": [1]}])), None);
    assert_eq!(pk("SELECT * FROM c WHERE c.pk = @p", &["/pk"], n.clone()), None);
}

// ---------------------------------------------------------------------------------------------
// The JavaScript SDK's pipeline (`@azure/cosmos` 4.10.2: GroupByValueEndpointComponent,
// GroupByEndpointComponent, emptyGroup.js, Aggregators/*), for a client that says it is at least
// at query version 1.4 and takes composite aggregates.
// ---------------------------------------------------------------------------------------------

use azemu_cosmos_sql::PlanOptions;

/// `extractAggregateResult`: `null` for `{}`, else `item2` if set, else `item`.
fn js_extract(payload: &Value) -> Option<Value> {
    let o = payload.as_object()?;
    if o.is_empty() {
        return None;
    }
    match o.get("item2") {
        Some(v) if !v.is_null() && v != &json!(false) && v != &json!(0) => Some(v.clone()),
        _ => Some(o.get("item").cloned().unwrap_or(Value::Null)),
    }
}

#[derive(Default)]
struct JsAgg {
    value: Option<Value>,
    sum: Option<f64>,
    count: f64,
}

impl JsAgg {
    /// `other` is what `extractAggregateResult` gave; `None` stands for JS `null` / `undefined`.
    fn aggregate(&mut self, kind: Option<AggregateKind>, other: Option<Value>) {
        let ord = |a: &Value, b: &Value| sdk_compare(&json!({"item": a}), &json!({"item": b}));
        match kind {
            Some(AggregateKind::Count) => self.count += other.and_then(|v| v.as_f64()).unwrap_or(0.0),
            Some(AggregateKind::Sum) => {
                if let Some(v) = other.and_then(|v| v.as_f64()) {
                    self.sum = Some(self.sum.unwrap_or(0.0) + v);
                }
            }
            Some(AggregateKind::Average) => {
                if let Some(o) = other {
                    if let Some(s) = o.get("sum").and_then(Value::as_f64) {
                        self.sum = Some(self.sum.unwrap_or(0.0) + s);
                        self.count += o["count"].as_f64().unwrap();
                    }
                }
            }
            Some(k @ (AggregateKind::Min | AggregateKind::Max)) => {
                // MinAggregator.aggregate(other): `other.min === undefined` skips; a `null` other would throw.
                let other = other.expect("the SDK would throw on a null partial MIN/MAX");
                let field = if k == AggregateKind::Min { "min" } else { "max" };
                let Some(v) = other.get(field) else { return };
                let better = match &self.value {
                    None => true,
                    Some(b) => {
                        (k == AggregateKind::Min && ord(v, b) == Ordering::Less)
                            || (k == AggregateKind::Max && ord(v, b) == Ordering::Greater)
                    }
                };
                if better {
                    self.value = Some(v.clone());
                }
            }
            // StaticValueAggregator: the first value stays
            None => {
                if self.value.is_none() {
                    self.value = other;
                }
            }
        }
    }

    fn result(&self, kind: Option<AggregateKind>) -> Option<Value> {
        match kind {
            Some(AggregateKind::Count) => Some(json!(self.count as i64)),
            Some(AggregateKind::Sum) => self.sum.map(|s| json!(s)),
            Some(AggregateKind::Average) => self.sum.filter(|_| self.count > 0.0).map(|s| json!(s / self.count)),
            _ => self.value.clone(),
        }
    }
}

/// One output field of a group: its alias, its aggregate kind and the aggregator.
type Slot = (String, Option<AggregateKind>, JsAgg);

fn js_run(sql: &str, docs: &[Value], parts: usize) -> Vec<Value> {
    let q = parse(sql).unwrap();
    let params = Params::new();
    let info = q.info_for(&params, PlanOptions { composite_min_max: true, ..PlanOptions::default() });
    let chunks: Vec<&[Value]> = docs.chunks(docs.len().div_ceil(parts)).collect();
    // what the service does with the rewritten text
    let per_part: Vec<Vec<Value>> =
        chunks.iter().map(|c| parse(&info.rewritten_query).unwrap().execute(*c, &params, &fixed_clock_opts()).unwrap()).collect();
    let rows: Vec<Value> = per_part.into_iter().flatten().collect();
    if !info.group_by_expressions.is_empty() || (info.has_select_value && !info.aggregates.is_empty()) {
        if info.has_select_value {
            // GroupByValueEndpointComponent: aggregateType = queryInfo.aggregates[0]
            let kind = info.aggregates.first().copied();
            let mut aggs: Vec<(String, JsAgg)> = vec![];
            for item in &rows {
                // a payload that is undefined has no `payload` member
                let (group, payload) = match item.get("groupByItems") {
                    Some(g) => (g.to_string(), item.get("payload").cloned()),
                    None => ("__empty__".to_string(), Some(item.clone())),
                };
                if !aggs.iter().any(|(g, _)| *g == group) {
                    aggs.push((group.clone(), JsAgg::default()));
                }
                let agg = &mut aggs.iter_mut().find(|(g, _)| *g == group).unwrap().1;
                match kind {
                    Some(_) => agg.aggregate(kind, payload.as_ref().and_then(|p| js_extract(&p[0]))),
                    None => agg.aggregate(None, payload),
                }
            }
            return aggs.iter().filter_map(|(_, a)| a.result(kind)).collect();
        }
        // GroupByEndpointComponent
        let mut groups: Vec<(String, Vec<Slot>)> = vec![];
        for item in &rows {
            let group = item.get("groupByItems").map(|g| g.to_string()).unwrap_or_else(|| "__empty__".to_string());
            let payload = item["payload"].as_object().unwrap();
            if !groups.iter().any(|(g, _)| *g == group) {
                groups.push((group.clone(), vec![]));
            }
            let entry = &mut groups.iter_mut().find(|(g, _)| *g == group).unwrap().1;
            for (key, val) in payload {
                let kind = info.group_by_alias_to_aggregate_type.iter().find(|(a, _)| a == key).and_then(|(_, k)| *k);
                if !entry.iter().any(|(k, _, _)| k == key) {
                    entry.push((key.clone(), kind, JsAgg::default()));
                }
                let slot = entry.iter_mut().find(|(k, _, _)| k == key).unwrap();
                if kind.is_some() {
                    slot.2.aggregate(kind, js_extract(val));
                } else {
                    slot.2.aggregate(None, Some(val.clone()));
                }
            }
        }
        return groups
            .into_iter()
            .map(|(_, e)| {
                let mut o = serde_json::Map::new();
                for (k, kind, a) in e {
                    if let Some(v) = a.result(kind) {
                        o.insert(k, v);
                    }
                }
                Value::Object(o)
            })
            .collect();
    }
    rows
}

/// Floats that are whole numbers compare equal to the integer (the SDK works in doubles).
fn norm(v: &Value) -> Value {
    match v {
        Value::Number(n) => n.as_f64().map(|f| json!(f)).unwrap_or(v.clone()),
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), norm(x))).collect()),
        _ => v.clone(),
    }
}

#[test]
fn js_pipeline_matches_single_pass_for_every_aggregate() {
    let docs = people();
    for sql in [
        "SELECT VALUE COUNT(1) FROM c",
        "SELECT VALUE COUNT(c.score) FROM c",
        "SELECT VALUE SUM(c.score) FROM c",
        "SELECT VALUE SUM(c.score) FROM c WHERE c.age > 26",
        "SELECT VALUE AVG(c.score) FROM c",
        "SELECT VALUE AVG(c.nothing) FROM c",
        "SELECT VALUE MIN(c.age) FROM c",
        "SELECT VALUE MAX(c.age) FROM c",
        "SELECT VALUE MIN(c.score) FROM c",
        "SELECT VALUE MAX(c.score) FROM c WHERE c.age > 26",
        "SELECT VALUE MAX(c.city) FROM c",
        "SELECT VALUE MIN(c.city) FROM c",
        "SELECT VALUE MIN(c.nothing) FROM c",
        "SELECT VALUE MAX(c.nothing) FROM c",
        "SELECT VALUE MIN(c.score) FROM c WHERE c.age > 1000",
        "SELECT VALUE COUNT(1) FROM c WHERE c.age > 1000",
        "SELECT c.city, COUNT(1) AS n FROM c GROUP BY c.city",
        "SELECT VALUE COUNT(1) FROM c GROUP BY c.city",
        "SELECT VALUE SUM(c.score) FROM c GROUP BY c.city",
        "SELECT VALUE AVG(c.score) FROM c GROUP BY c.city",
        "SELECT VALUE MAX(c.age) FROM c GROUP BY c.city",
        "SELECT VALUE c.city FROM c GROUP BY c.city",
        "SELECT c.city, MIN(c.age) AS lo, MAX(c.score) AS hi FROM c GROUP BY c.city",
        "SELECT c.city, AVG(c.score) AS a, SUM(c.score) AS s, COUNT(c.score) AS n FROM c GROUP BY c.city",
        "SELECT c.city, MIN(c.score) AS lo FROM c WHERE c.age > 26 GROUP BY c.city",
    ] {
        let expected = norm(&Value::Array(parse(sql).unwrap().execute(&docs, &p0(), &fixed_clock_opts()).unwrap()));
        for parts in [1, 2, 3, 7] {
            let mut got = js_run(sql, &docs, parts);
            let mut want = expected.as_array().unwrap().clone();
            if sql.contains("GROUP BY") {
                got.sort_by_key(|v| v.to_string());
                want.sort_by_key(|v| v.to_string());
            }
            assert_eq!(norm(&Value::Array(got)), Value::Array(want), "{sql} over {parts} partitions");
        }
    }
}

#[test]
fn composite_min_max_is_asked_for_only_by_options() {
    let q = parse("SELECT VALUE MIN(c.age) FROM c").unwrap();
    // default: the bare value (the Python SDK)
    assert_eq!(q.info(&p0()).rewritten_query, "SELECT VALUE [{\"item\": MIN(c.age)}]\nFROM c");
    // composite: {"min", "count"} (the JavaScript SDK); an empty partition has count 0 and no min
    let info = q.info_for(&p0(), PlanOptions { composite_min_max: true, ..PlanOptions::default() });
    assert_eq!(info.rewritten_query, "SELECT VALUE [{\"item\": {\"min\": MIN(c.age), \"count\": COUNT(c.age)}}]\nFROM c");
    assert_eq!(info.aggregates, vec![AggregateKind::Min]);
    let rows = parse(&info.rewritten_query).unwrap().execute(&people(), &p0(), &fixed_clock_opts()).unwrap();
    assert_eq!(rows, vec![json!([{"item": {"min": 25, "count": 7}}])]);
    let none = parse(&info.rewritten_query).unwrap().execute(&[] as &[Value], &p0(), &fixed_clock_opts()).unwrap();
    assert_eq!(none, vec![json!([{"item": {"count": 0}}])]);
    // MAX and GROUP BY use the same form
    let g = parse("SELECT c.city, MAX(c.age) AS m FROM c GROUP BY c.city")
        .unwrap()
        .info_for(&p0(), PlanOptions { composite_min_max: true, ..PlanOptions::default() });
    assert!(
        g.rewritten_query.contains("\"m\": {\"item\": {\"max\": MAX(c.age), \"count\": COUNT(c.age)}}")
            || g.rewritten_query.contains("\"m\": {\"item\": {\"max\""),
        "{}",
        g.rewritten_query
    );
    assert!(parse(&g.rewritten_query).is_ok());
}

#[test]
fn select_value_aggregate_with_group_by_names_its_operator_and_wraps_the_payload() {
    // What the JavaScript SDK reads (`aggregates[0]`, `payload[0]`) and the Java SDK needs (`aggregates` not empty, else it
    // keeps the first payload as the result of the group): the operator in `aggregates`, the partial in a one element array.
    let q = parse("SELECT VALUE COUNT(1) FROM c WHERE c.age > 1 GROUP BY c.city").unwrap();
    let i = q.info(&p0());
    assert_eq!(i.aggregates, vec![AggregateKind::Count]);
    assert!(i.has_select_value);
    assert_eq!(i.group_by_aliases, vec!["$1".to_string()]);
    assert_eq!(
        i.rewritten_query,
        "SELECT [{\"item\": c.city}] AS groupByItems, [{\"item\": COUNT(1)}] AS payload\nFROM c\nWHERE c.age > 1\nGROUP BY c.city"
    );
    // a value that is no aggregate stays bare and names no operator
    let v = parse("SELECT VALUE c.city FROM c GROUP BY c.city").unwrap().info(&p0());
    assert!(v.aggregates.is_empty());
    assert_eq!(v.rewritten_query, "SELECT [{\"item\": c.city}] AS groupByItems, c.city AS payload\nFROM c\nGROUP BY c.city");
}

#[test]
fn select_list_of_aggregates_without_group_by_is_planned_like_one_group_for_clients_that_declare_it() {
    // C4 of the Java suite: the Java SDK (feature `NonValueAggregate`) reads each partition's answer as `{"payload": {alias: {"item": ..}}}`
    // and merges by `groupByAliasToAggregateType`; without the feature the old form (one array of items) stays.
    let q = parse("SELECT COUNT(1) AS n, MAX(c.age) AS hi, AVG(c.age) FROM c WHERE c.age > 1").unwrap();
    let info = q.info_for(&p0(), PlanOptions { non_value_aggregate: true, ..PlanOptions::default() });
    assert!(!info.has_select_value);
    assert_eq!(info.group_by_aliases, vec!["n".to_string(), "hi".to_string(), "$1".to_string()]);
    assert_eq!(
        info.group_by_alias_to_aggregate_type,
        vec![
            ("n".to_string(), Some(AggregateKind::Count)),
            ("hi".to_string(), Some(AggregateKind::Max)),
            ("$1".to_string(), Some(AggregateKind::Average))
        ]
    );
    assert_eq!(
        info.rewritten_query,
        "SELECT VALUE {\"payload\": {\"n\": {\"item\": COUNT(1)}, \"hi\": {\"item\": MAX(c.age)}, \"$1\": {\"item\": {\"sum\": SUM(c.age), \"count\": COUNT(c.age)}}}}\nFROM c\nWHERE c.age > 1"
    );
    // each partition answers one object with a payload, also when it has no row
    let docs = vec![json!({"age": 30}), json!({"age": 40})];
    let rows = parse(&info.rewritten_query).unwrap().execute(&docs, &p0(), &fixed_clock_opts()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["payload"]["n"], json!({"item": 2}));
    let none = parse(&info.rewritten_query).unwrap().execute(&Vec::<Value>::new(), &p0(), &fixed_clock_opts()).unwrap();
    assert_eq!(none, vec![json!({"payload": {"n": {"item": 0}, "hi": {}, "$1": {"item": {"count": 0}}}})]);

    let old = q.info(&p0());
    assert!(old.group_by_aliases.is_empty() && old.group_by_alias_to_aggregate_type.is_empty());
    assert!(old.rewritten_query.starts_with("SELECT VALUE [{\"item\": COUNT(1)}"));
}
