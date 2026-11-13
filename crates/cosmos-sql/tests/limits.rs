//! Request input must never overflow the stack or allocate without bound: deeply nested
//! queries are refused with an error. Every case runs on a 2 MiB thread, the stack size of a
//! tokio worker thread (where the service runs queries), in a debug build.

use azemu_cosmos_sql::{parse, ErrorKind, EvalOptions, Params};
use serde_json::{json, Value};

fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("the query overflowed the stack or panicked")
}

/// Parse and run `sql` over one document; the result must be an error or a result, never a crash.
fn run(sql: String) -> Result<Vec<Value>, azemu_cosmos_sql::SqlError> {
    on_small_stack(move || {
        let docs = vec![json!({"id": "1", "a": [1, 2, 3], "x": 1})];
        parse(&sql)?.execute(&docs, &Params::new(), &EvalOptions::default())
    })
}

fn refused(sql: String) {
    match run(sql.clone()) {
        Err(e) => assert!(
            matches!(e.kind, ErrorKind::LimitExceeded | ErrorKind::Syntax),
            "wrong error kind {:?} for {}",
            e.kind,
            &sql[..sql.len().min(60)]
        ),
        Ok(_) => panic!("a query nested too deeply was accepted: {}", &sql[..sql.len().min(60)]),
    }
}

const N: usize = 10_000;

#[test]
fn parentheses_10000_deep() {
    refused(format!("SELECT VALUE {}1{} FROM c", "(".repeat(N), ")".repeat(N)));
}

#[test]
fn unclosed_parentheses_10000_deep() {
    refused(format!("SELECT VALUE {}1 FROM c", "(".repeat(N)));
}

#[test]
fn array_subqueries_10000_deep() {
    let mut s = String::from("SELECT VALUE ");
    for _ in 0..N {
        s.push_str("ARRAY(SELECT VALUE ");
    }
    s.push('1');
    for _ in 0..N {
        s.push(')');
    }
    refused(s);
}

#[test]
fn exists_and_scalar_subqueries_10000_deep() {
    let mut s = String::from("SELECT VALUE ");
    for i in 0..N {
        s.push_str(if i % 2 == 0 { "EXISTS(SELECT VALUE " } else { "(SELECT VALUE " });
    }
    s.push('1');
    for _ in 0..N {
        s.push(')');
    }
    refused(s);
}

#[test]
fn nested_arrays_inside_in_10000_deep() {
    refused(format!("SELECT VALUE c.x IN ({}1{}) FROM c", "[".repeat(N), "]".repeat(N)));
}

#[test]
fn nested_array_and_object_literals_10000_deep() {
    refused(format!("SELECT VALUE {}1{} FROM c", "[".repeat(N), "]".repeat(N)));
    refused(format!("SELECT VALUE {}1{} FROM c", "{\"a\":".repeat(N), "}".repeat(N)));
}

#[test]
fn unary_chains_100000_long() {
    refused(format!("SELECT VALUE {}1 FROM c", "-".repeat(100_000)));
    refused(format!("SELECT VALUE {}true FROM c", "NOT ".repeat(100_000)));
    refused(format!("SELECT VALUE {}1 FROM c", "~".repeat(100_000)));
}

#[test]
fn function_calls_10000_deep() {
    refused(format!("SELECT VALUE {}1{} FROM c", "ABS(".repeat(N), ")".repeat(N)));
}

#[test]
fn index_expressions_10000_deep() {
    refused(format!("SELECT VALUE c{}0{} FROM c", "[c".repeat(N), "]".repeat(N)));
}

#[test]
fn from_subqueries_10000_deep() {
    let mut s = String::new();
    for _ in 0..N {
        s.push_str("SELECT * FROM (");
    }
    s.push_str("SELECT * FROM c");
    for _ in 0..N {
        s.push(')');
    }
    refused(s);
}

#[test]
fn right_nested_ternary_and_coalesce_chains() {
    refused(format!("SELECT VALUE {}1 FROM c", "true ? 1 : ".repeat(N)));
    refused(format!("SELECT VALUE {}1 FROM c", "c.q ?? ".repeat(N)));
}

#[test]
fn long_left_associative_chains() {
    for op in ["+", "||", "*", "-", "<", "=", "&", "<<"] {
        let sql = format!("SELECT VALUE 1{} FROM c", format!(" {op} 1").repeat(N));
        if let Err(e) = run(sql) {
            assert!(matches!(e.kind, ErrorKind::LimitExceeded | ErrorKind::Syntax), "{op}");
        }
    }
}

#[test]
fn long_property_chain() {
    refused(format!("SELECT VALUE c{} FROM c", ".a".repeat(100_000)));
}

#[test]
fn long_flat_and_or_chain_is_fine() {
    let sql = format!("SELECT VALUE c.x FROM c WHERE c.x = 1{}", " OR c.x = 2".repeat(20_000));
    assert_eq!(run(sql).expect("flat chains are allowed"), vec![json!(1)]);
}

#[test]
fn between_and_in_chains() {
    refused(format!("SELECT VALUE {}1 FROM c", "1 BETWEEN 0 AND ".repeat(N)));
    refused(format!("SELECT VALUE {}1 FROM c", "1 IN (".repeat(N)));
}

#[test]
fn the_deepest_allowed_query_still_runs() {
    // 20 levels of parentheses is far beyond real queries and must work.
    let sql = format!("SELECT VALUE {}c.x{} FROM c", "(".repeat(20), ")".repeat(20));
    assert_eq!(run(sql).expect("20 levels"), vec![json!(1)]);
    let mut s = String::from("SELECT VALUE ");
    for _ in 0..8 {
        s.push_str("ARRAY(SELECT VALUE ");
    }
    s.push('1');
    for _ in 0..8 {
        s.push(')');
    }
    assert!(run(s).is_ok());
}

#[test]
fn replicate_and_friends_are_bounded() {
    // Cosmos limits REPLICATE to 10000 characters; a huge count must not allocate gigabytes.
    for sql in [
        "SELECT VALUE REPLICATE('abc', 2000000000) FROM c",
        "SELECT VALUE REPLICATE('abc', 4000000000) FROM c",
        "SELECT VALUE REPLICATE('abc', 1e18) FROM c",
    ] {
        if let Ok(v) = run(sql.to_string()) {
            assert!(v.iter().all(|x| x.as_str().map(|s| s.len() <= 100_000).unwrap_or(true)));
        }
    }
}

#[test]
fn like_does_not_blow_up_exponentially() {
    let started = std::time::Instant::now();
    let pattern = "%a".repeat(30) + "b";
    let sql = format!("SELECT VALUE c.s LIKE '{pattern}' FROM c");
    let docs = vec![json!({"s": "a".repeat(200)})];
    on_small_stack(move || {
        let _ = parse(&sql).and_then(|q| q.execute(&docs, &Params::new(), &EvalOptions::default()));
    });
    assert!(started.elapsed().as_secs() < 5, "LIKE took {:?}", started.elapsed());
}

/// The biggest query each shape allows (the limit itself) must run on a small stack.
#[test]
fn the_largest_accepted_queries_run_on_a_small_stack() {
    type Shape = fn(usize) -> String;
    let shapes: Vec<(&str, Shape)> = vec![
        ("parens", |n| format!("SELECT VALUE {}c.x{} FROM c", "(".repeat(n), ")".repeat(n))),
        ("array subqueries", |n| format!("SELECT VALUE {}1{} FROM c", "ARRAY(SELECT VALUE ".repeat(n), ")".repeat(n))),
        ("exists subqueries", |n| format!("SELECT VALUE {}c.x{} FROM c", "EXISTS(SELECT VALUE ".repeat(n), ")".repeat(n))),
        ("correlated join subqueries", |n| {
            format!("SELECT VALUE {}c.x{} FROM c", "ARRAY(SELECT VALUE x FROM x IN c.a WHERE ".repeat(n), ")".repeat(n))
        }),
        ("from subqueries", |n| format!("{}c{}", "SELECT * FROM (".repeat(n), ") ".repeat(n))),
        ("negation", |n| format!("SELECT VALUE {}c.x FROM c", "-".repeat(n))),
        ("not", |n| format!("SELECT VALUE {}true FROM c", "NOT ".repeat(n))),
        ("sum chain", |n| format!("SELECT VALUE c.x{} FROM c", " + c.x".repeat(n))),
        ("ternary", |n| format!("SELECT VALUE {}1 FROM c", "true ? 1 : ".repeat(n))),
        ("coalesce", |n| format!("SELECT VALUE {}1 FROM c", "c.q ?? ".repeat(n))),
        ("function calls", |n| format!("SELECT VALUE {}c.x{} FROM c", "ABS(".repeat(n), ")".repeat(n))),
        ("array literals", |n| format!("SELECT VALUE {}1{} FROM c", "[".repeat(n), "]".repeat(n))),
        ("object literals", |n| format!("SELECT VALUE {}1{} FROM c", "{\"a\":".repeat(n), "}".repeat(n))),
        ("in lists", |n| format!("SELECT VALUE c.x IN ({}1{}) FROM c", "[".repeat(n), "]".repeat(n))),
    ];
    for (name, shape) in shapes {
        // largest n that parses (the limits are well below 200)
        let mut best = 0;
        for n in 1..200 {
            let sql = shape(n);
            if on_small_stack(move || parse(&sql).is_ok()) {
                best = n;
            } else {
                break;
            }
        }
        assert!(best >= 8, "{name}: only {best} levels allowed");
        let sql = shape(best);
        let r = run(sql);
        assert!(r.is_ok() || r.is_err(), "{name}");
        println!("{name}: {best} levels, ran");
    }
}

#[test]
fn a_query_of_thousands_of_joins_is_refused_not_a_stack_overflow() {
    // 400 KB of JOINs: the evaluator is recursive over the sources, so the parser stops far below that
    let joins = " JOIN a IN c.x".repeat(30_000);
    let sql = format!("SELECT VALUE 1 FROM c{joins}");
    assert!(sql.len() < 512 * 1024);
    let e = run(sql).unwrap_err();
    assert_eq!(e.kind, ErrorKind::LimitExceeded);
    // 64 sources, each with one row, run 64 levels deep on a small stack
    let deep = |joins: usize| {
        on_small_stack(move || {
            let docs = vec![json!({"one": [1]})];
            let sql: String = (0..joins).map(|i| format!(" JOIN a{i} IN c.one")).collect();
            parse(&format!("SELECT VALUE 1 FROM c{sql}"))?.execute(&docs, &Params::new(), &EvalOptions::default())
        })
    };
    assert_eq!(deep(63).unwrap(), vec![json!(1)]);
    assert_eq!(deep(64).unwrap_err().kind, ErrorKind::LimitExceeded);
}
