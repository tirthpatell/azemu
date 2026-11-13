//! Query requests: recognising them, parsing the body, running them on a list of JSON
//! values, and the generic feed (list or query) used by every resource type.

use azemu_cosmos_sql::{parse, EvalOptions, Params, Query};
use serde_json::{json, Map, Value};

use crate::errors::{CosmosError, R};
use crate::out::{Cx, Out};
use crate::paging;

/// True for a POST that carries a query (SDKs set `x-ms-documentdb-isquery` and the
/// `application/query+json` content type).
pub fn is_query(cx: &Cx) -> bool {
    if cx.req.method != "POST" {
        return false;
    }
    if cx.flag("x-ms-documentdb-isquery") {
        return true;
    }
    cx.header("content-type")
        .map(|c| {
            let c = c.to_ascii_lowercase();
            c.contains("query+json") || c.contains("application/sql")
        })
        .unwrap_or(false)
}

pub struct QueryBody {
    pub text: String,
    pub params_json: Value,
}

pub fn parse_body(cx: &Cx) -> R<QueryBody> {
    let body = parse_body_unchecked(cx)?;
    if body.text.len() > crate::limits::MAX_QUERY_BYTES {
        return Err(CosmosError::bad_request("The query text is too long: the limit is 512 KB."));
    }
    Ok(body)
}

fn parse_body_unchecked(cx: &Cx) -> R<QueryBody> {
    let ct = cx.header("content-type").unwrap_or("").to_ascii_lowercase();
    if ct.contains("application/sql") {
        return Ok(QueryBody { text: String::from_utf8_lossy(&cx.req.body).to_string(), params_json: Value::Array(vec![]) });
    }
    let v = cx.body_json()?;
    let text = v
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| CosmosError::bad_request("The query body must have a \"query\" string."))?
        .to_string();
    Ok(QueryBody { text, params_json: v.get("parameters").cloned().unwrap_or(Value::Array(vec![])) })
}

/// A parsed query ready to run.
pub struct Prepared {
    pub query: Query,
    pub params: Params,
}

pub fn prepare(body: &QueryBody) -> R<Prepared> {
    let query = parse(&body.text).map_err(|e| CosmosError::from_sql(&e))?;
    let params = Params::from_cosmos(&body.params_json);
    Ok(Prepared { query, params })
}

pub fn eval_options(cx: &Cx) -> EvalOptions {
    eval_options_at(cx.now)
}

pub fn eval_options_at(now: i64) -> EvalOptions {
    EvalOptions {
        now: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(now.max(0) as u64)),
        max_bytes: crate::limits::QUERY_MEMORY_BYTES,
        ..EvalOptions::default()
    }
}

/// Run a prepared query over `docs` as written (no rewriting).
pub fn run<'a, I: IntoIterator<Item = &'a Value>>(cx: &Cx, p: &Prepared, docs: I) -> R<Vec<Value>> {
    p.query.execute(docs, &p.params, &eval_options(cx)).map_err(|e| CosmosError::from_sql(&e))
}

/// A feed response: `{"_rid":…, "<key>":[…], "_count":n}`.
pub fn feed_body(rid: &str, key: &str, items: Vec<Value>) -> Value {
    let n = items.len();
    let mut m = Map::new();
    m.insert("_rid".into(), json!(rid));
    m.insert(key.into(), Value::Array(items));
    m.insert("_count".into(), json!(n));
    Value::Object(m)
}

/// List (GET) or query (POST) over resources already rendered to JSON.
pub fn feed(cx: &Cx, rid: &str, key: &str, items: Vec<Value>) -> R<Out> {
    let items = if is_query(cx) {
        let body = parse_body(cx)?;
        let p = prepare(&body)?;
        run(cx, &p, items.iter())?
    } else {
        items
    };
    let (page, next) = paging::page(items, cx.header("x-ms-continuation"), cx.max_item_count());
    let n = page.len();
    let mut out = Out::json(200, feed_body(rid, key, page)).header("x-ms-item-count", n);
    if let Some(t) = next {
        out = out.header("x-ms-continuation", t);
    }
    Ok(out.charge(2.0 + n as f64 * 0.01))
}
