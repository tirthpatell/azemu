//! `udf.name(args)`: the evaluator calls the host the caller gives in `EvalOptions::udf`.

use std::sync::{Arc, Mutex};

use azemu_cosmos_sql::{parse, ErrorKind, EvalOptions, Params, UdfFactory, UdfHost};
use serde_json::{json, Value};

#[derive(Debug, Default)]
struct Calls(Mutex<Vec<(String, Vec<Option<Value>>)>>);

#[derive(Debug)]
struct Factory(Arc<Calls>);

struct Host(Arc<Calls>);

impl UdfFactory for Factory {
    fn make(&self) -> Box<dyn UdfHost> {
        Box::new(Host(self.0.clone()))
    }
}

impl UdfHost for Host {
    fn call(&self, name: &str, args: &[Option<Value>]) -> Result<Option<Value>, String> {
        self.0 .0.lock().unwrap().push((name.to_string(), args.to_vec()));
        match name {
            "double" => Ok(args[0].as_ref().and_then(Value::as_f64).map(|n| json!(n * 2.0))),
            "boom" => Err("it threw".to_string()),
            "isUndefined" => Ok(Some(json!(args[0].is_none()))),
            "obj" => Ok(Some(json!({"a": [1, 2], "b": args.len()}))),
            _ => Err(format!("no function {name}")),
        }
    }
}

fn run(q: &str, docs: &[Value]) -> (Result<Vec<Value>, azemu_cosmos_sql::SqlError>, Arc<Calls>) {
    let calls = Arc::new(Calls::default());
    let opts = EvalOptions { udf: Some(Arc::new(Factory(calls.clone()))), ..EvalOptions::default() };
    (parse(q).unwrap().execute(docs, &Params::new(), &opts), calls)
}

fn docs() -> Vec<Value> {
    vec![json!({"id": "a", "n": 1}), json!({"id": "b", "n": 5}), json!({"id": "c"})]
}

#[test]
fn a_udf_works_in_select_where_and_nested_expressions() {
    let (r, calls) = run("SELECT VALUE udf.double(c.n) FROM c", &docs());
    assert_eq!(r.unwrap(), vec![json!(2.0), json!(10.0)], "an undefined result leaves the row out");
    assert_eq!(calls.0.lock().unwrap().len(), 2, "once per row that has the argument");
    let (r, _) = run("SELECT c.id FROM c WHERE udf.double(c.n) > 5", &docs());
    assert_eq!(r.unwrap(), vec![json!({"id": "b"})]);
    let (r, _) = run("SELECT c.id, udf.double(udf.double(c.n)) AS q, udf.obj(1, 2).b AS b FROM c WHERE IS_DEFINED(c.n)", &docs());
    assert_eq!(r.unwrap(), vec![json!({"id": "a", "q": 4.0, "b": 2}), json!({"id": "b", "q": 20.0, "b": 2})]);
}

#[test]
fn a_function_is_not_called_with_an_undefined_argument() {
    // as on Azure: the call is left out and its result is undefined, so the row with no `n` gives nothing
    let (r, calls) = run("SELECT VALUE udf.isUndefined(c.n) FROM c", &docs());
    assert_eq!(r.unwrap(), vec![json!(false), json!(false)]);
    let calls = calls.0.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|(_, args)| args.iter().all(Option::is_some)));
}

#[test]
fn a_failing_function_fails_the_query_with_its_message() {
    let (r, _) = run("SELECT VALUE udf.boom(c.n) FROM c", &docs());
    let e = r.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Evaluation);
    assert!(e.message.contains("it threw"), "{e}");
}

#[test]
fn udf_names_lists_every_function_once_including_subqueries() {
    let q = parse("SELECT udf.a(c.x) AS v, ARRAY(SELECT VALUE udf.b(t) FROM t IN c.items WHERE udf.a(t) > 1) AS w FROM c WHERE udf.c(1) = 1 ORDER BY c.id").unwrap();
    assert_eq!(q.udf_names(), ["a", "b", "c"]);
    assert!(parse("SELECT * FROM c").unwrap().udf_names().is_empty());
}

#[test]
fn without_a_host_a_udf_call_is_unsupported() {
    let e = parse("SELECT VALUE udf.f(c.n) FROM c").unwrap().execute(&docs(), &Params::new(), &EvalOptions::default()).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unsupported);
    assert!(e.message.contains("udf.f"));
}

#[test]
fn a_udf_is_part_of_the_syntax_with_case_kept() {
    assert!(parse("SELECT udf.Tax(c.income) FROM c").is_ok());
    // the keyword is lower case on Azure: `UDF.Tax(..)` is a path followed by a bracket, a syntax error at the bracket
    assert_eq!(parse("SELECT UDF.Tax(c.income) FROM c").unwrap_err().code, "SC1001");
    assert!(parse("SELECT udf.Tax FROM c").is_err());
    assert!(parse("SELECT udf. (1) FROM c").is_err());
    let calls = Arc::new(Calls::default());
    let opts = EvalOptions { udf: Some(Arc::new(Factory(calls.clone()))), ..EvalOptions::default() };
    let _ = parse("SELECT VALUE udf.double(1)").unwrap().execute(&docs(), &Params::new(), &opts);
    assert_eq!(calls.0.lock().unwrap()[0].0, "double");
}

#[test]
fn order_by_takes_a_path_so_a_udf_there_is_refused() {
    let docs = vec![json!({"id": "x", "n": 9}), json!({"id": "y", "n": 1})];
    let _ = &docs;
    let e = parse("SELECT VALUE c.id FROM c ORDER BY udf.double(c.n)").unwrap_err();
    assert_eq!(e.code, "2206", "Azure: ORDER BY item expression could not be mapped to a document path");
}
