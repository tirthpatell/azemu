#![allow(dead_code)]
//! Helpers shared by the integration tests.

use azemu_cosmos_sql::{parse, EvalOptions, Params, SqlError};
use serde_json::{json, Value};
use std::time::{Duration, SystemTime};

/// Clock used by the documentation examples for NOW/AGO: 2024-10-14T14:20:30.6668888Z.
pub fn fixed_clock_opts() -> EvalOptions {
    // 2024-10-14T14:20:30Z = 1728915630 s since the epoch
    EvalOptions {
        now: Some(SystemTime::UNIX_EPOCH + Duration::new(1_728_915_630, 666_888_800)),
        rand_seed: Some(42),
        ..EvalOptions::default()
    }
}

pub fn try_run(sql: &str, docs: &[Value], opts: &EvalOptions) -> Result<Vec<Value>, SqlError> {
    try_run_p(sql, docs, &Params::new(), opts)
}

pub fn try_run_p(sql: &str, docs: &[Value], params: &Params, opts: &EvalOptions) -> Result<Vec<Value>, SqlError> {
    parse(sql)?.execute(docs, params, opts)
}

/// Runs a query, panicking with the query text on error.
pub fn run(sql: &str, docs: &[Value]) -> Vec<Value> {
    try_run(sql, docs, &fixed_clock_opts()).unwrap_or_else(|e| panic!("query failed: {sql}\n  {e}"))
}

pub fn run_p(sql: &str, docs: &[Value], params: Value) -> Vec<Value> {
    let p = Params::from_cosmos(&params);
    try_run_p(sql, docs, &p, &fixed_clock_opts()).unwrap_or_else(|e| panic!("query failed: {sql}\n  {e}"))
}

pub fn err(sql: &str) -> SqlError {
    match parse(sql) {
        Err(e) => e,
        Ok(q) => match q.execute(&[] as &[Value], &Params::new(), &fixed_clock_opts()) {
            Err(e) => e,
            Ok(v) => panic!("expected an error for: {sql}\n  got {v:?}"),
        },
    }
}

/// Table-driven check: `(sql, expected JSON array text)` over `docs`.
pub fn check_table(docs: &[Value], cases: &[(&str, &str)]) {
    let mut failures = Vec::new();
    for (sql, expected) in cases {
        let expected: Value = serde_json::from_str(expected).unwrap_or_else(|e| panic!("bad expectation {expected}: {e}"));
        match try_run(sql, docs, &fixed_clock_opts()) {
            Ok(got) => {
                if Value::Array(got.clone()) != expected {
                    failures.push(format!("{sql}\n   expected {expected}\n   got      {}", Value::Array(got)));
                }
            }
            Err(e) => failures.push(format!("{sql}\n   error {e}")),
        }
    }
    assert!(failures.is_empty(), "{} of {} cases failed:\n{}", failures.len(), cases.len(), failures.join("\n"));
}

/// Like `check_table` but with items compared as a multiset (order not significant).
pub fn check_table_unordered(docs: &[Value], cases: &[(&str, &str)]) {
    let mut failures = Vec::new();
    for (sql, expected) in cases {
        let mut expected: Vec<Value> = serde_json::from_str(expected).unwrap();
        match try_run(sql, docs, &fixed_clock_opts()) {
            Ok(mut got) => {
                let key = |v: &Value| v.to_string();
                got.sort_by_key(key);
                expected.sort_by_key(key);
                if got != expected {
                    failures.push(format!("{sql}\n   expected {expected:?}\n   got      {got:?}"));
                }
            }
            Err(e) => failures.push(format!("{sql}\n   error {e}")),
        }
    }
    assert!(failures.is_empty(), "{} of {} cases failed:\n{}", failures.len(), cases.len(), failures.join("\n"));
}

pub fn families() -> Vec<Value> {
    // The Families sample used throughout older Cosmos DB documentation.
    vec![
        json!({
            "id": "AndersenFamily",
            "lastName": "Andersen",
            "parents": [{"firstName": "Thomas"}, {"firstName": "Mary Kay"}],
            "children": [{
                "firstName": "Henriette Thaulow", "gender": "female", "grade": 5,
                "pets": [{"givenName": "Fluffy"}]
            }],
            "address": {"state": "WA", "county": "King", "city": "Seattle"},
            "creationDate": 1431620472,
            "isRegistered": true
        }),
        json!({
            "id": "WakefieldFamily",
            "parents": [
                {"familyName": "Wakefield", "givenName": "Robin"},
                {"familyName": "Miller", "givenName": "Ben"}
            ],
            "children": [
                {"familyName": "Merriam", "givenName": "Jesse", "gender": "female", "grade": 1,
                 "pets": [{"givenName": "Goofy"}, {"givenName": "Shadow"}]},
                {"familyName": "Miller", "givenName": "Lisa", "gender": "female", "grade": 8}
            ],
            "address": {"state": "NY", "county": "Manhattan", "city": "NY"},
            "creationDate": 1431620462,
            "isRegistered": false
        }),
    ]
}

/// JSON equality where numbers may differ by 1e-12 relative (libm last-digit differences).
pub fn approx_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            x == y || (x - y).abs() <= 1e-12 * x.abs().max(y.abs())
        }
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| approx_eq(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).map(|w| approx_eq(v, w)).unwrap_or(false))
        }
        _ => a == b,
    }
}
