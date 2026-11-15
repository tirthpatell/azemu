//! Cosmos DB for NoSQL query language: parser, evaluator and query analysis.
//!
//! A pure library: no I/O, no async, no `unsafe`. It implements the SQL dialect of
//! Azure Cosmos DB for NoSQL over `serde_json::Value` documents.
//!
//! ```
//! use azemu_cosmos_sql::{parse, EvalOptions, Params};
//! use serde_json::json;
//!
//! let docs = vec![json!({"id": "1", "n": 3}), json!({"id": "2", "n": 5})];
//! let q = parse("SELECT VALUE c.id FROM c WHERE c.n > @min ORDER BY c.n DESC").unwrap();
//! let mut params = Params::new();
//! params.insert("@min", json!(1));
//! let out = q.execute(&docs, &params, &EvalOptions::default()).unwrap();
//! assert_eq!(out, vec![json!("2"), json!("1")]);
//! ```
//!
//! # Public API
//!
//! * [`parse`] turns text into a [`Query`] (syntax, function names, identifiers are checked;
//!   errors are [`SqlError`] values that map to Cosmos `400 BadRequest`).
//! * [`Query::execute`] runs a query over the documents of one container and returns the items
//!   exactly as Cosmos returns them in the `Documents` array.
//! * [`Query::info`] and [`Query::needs_query_plan`] give the facts of a query plan.
//! * [`Query::execute_rewritten`] runs the query in the form the SDKs expect from a partition when
//!   they have asked for a query plan (see below).
//! * [`Query::partition_key_filter`] finds a partition key pinned by `WHERE`.
//!
//! # The SDK query plan
//!
//! The Python SDK first sends a cross-partition query as is. Only if the service answers
//! `400` with sub-status `1004` does it ask for a plan (`x-ms-cosmos-is-query-plan-request`) and
//! then re-send [`QueryInfo::rewritten_query`] to every partition. The service decides with
//! [`Query::needs_query_plan`]: it is `false` for queries without ORDER BY, aggregates, GROUP BY,
//! DISTINCT, OFFSET/LIMIT and TOP, which a single-partition gateway can serve directly.
//!
//! The rewritten text is ordinary SQL of this dialect. It is **parsed and executed by the same
//! engine** ([`parse`]); nothing special is needed to produce the SDK-facing shapes:
//!
//! * ORDER BY: `SELECT c._rid, [{"item": c.x}] AS orderByItems, <projection> AS payload FROM c
//!   WHERE ({documentdb-formattableorderbyquery-filter}) ORDER BY c.x`. Each result is
//!   `{"_rid"?, "orderByItems": [{"item": v}, ...], "payload": <original projection>}`. A missing
//!   `item` key means undefined. The placeholder is accepted by the parser as `true` (the Python SDK
//!   also replaces it by `true` itself).
//! * Aggregates: `SELECT VALUE [{"item": COUNT(1)}] FROM c` gives `[{"item": 3}]`; for `AVG` the
//!   item is `{"sum": .., "count": ..}`; over no values SUM is `0` and COUNT `0`, AVG `{"sum": 0, "count": 0}`, MIN and MAX
//!   `{"item2": {"count": 0}}` (they are `{"item": v, "item2": {"min": v, "count": n}}` otherwise).
//! * GROUP BY: `SELECT [{"item": g}] AS groupByItems, {"alias": {"item": COUNT(1)}} AS payload ...`
//! * OFFSET/LIMIT is replaced by `OFFSET 0 LIMIT offset+limit` (the SDK applies offset and limit).
//! * Queries that need no rewriting have an empty `rewritten_query` (the SDK then re-sends the
//!   original text).
//!
//! [`Query::execute_rewritten`] is `parse(info.rewritten_query)` + `execute` (or plain `execute`
//! when the rewritten text is empty).

mod aggregate;
mod analysis;
pub mod ast;
mod datetime;
mod error;
mod eval;
mod functions;
mod lexer;
mod like;
mod ops;
mod parser;
mod validate;
mod value;

pub use analysis::{DistinctType, PlanOptions, QueryInfo, SortOrder, ORDER_BY_FILTER_PLACEHOLDER};
pub use ast::AggregateKind;
pub use error::{Detail, ErrorKind, Shape, SqlError};

use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

/// One row of a query result: a stored document that came out unchanged (shared with the caller, not copied) or a
/// value the query built.
#[derive(Debug, Clone)]
pub enum Item {
    /// The document the caller passed in, as it is.
    Shared(Arc<Value>),
    /// A projection or computed value.
    Owned(Value),
}

impl Item {
    /// The row as JSON.
    pub fn value(&self) -> &Value {
        match self {
            Item::Shared(v) => v,
            Item::Owned(v) => v,
        }
    }

    /// The row as an owned JSON value (copies a shared document unless nobody else holds it).
    pub fn into_value(self) -> Value {
        match self {
            Item::Shared(v) => Arc::try_unwrap(v).unwrap_or_else(|a| (*a).clone()),
            Item::Owned(v) => v,
        }
    }
}

/// Query parameters: name (with or without the leading `@`) to JSON value.
#[derive(Debug, Clone, Default)]
pub struct Params(BTreeMap<String, Value>);

impl Params {
    /// Empty parameter set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a parameter. The leading `@` is optional.
    pub fn insert(&mut self, name: &str, value: Value) {
        self.0.insert(name.strip_prefix('@').unwrap_or(name).to_string(), value);
    }

    /// Look a parameter up by name (with or without `@`).
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.get(name.strip_prefix('@').unwrap_or(name))
    }

    /// Builds parameters from the Cosmos request form `[{"name": "@a", "value": 1}, ...]`.
    /// Entries without a string `name` are ignored.
    pub fn from_cosmos(parameters: &Value) -> Params {
        let mut p = Params::new();
        if let Some(arr) = parameters.as_array() {
            for e in arr {
                if let Some(n) = e.get("name").and_then(|n| n.as_str()) {
                    p.insert(n, e.get("value").cloned().unwrap_or(Value::Null));
                }
            }
        }
        p
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter()
    }
}

impl FromIterator<(String, Value)> for Params {
    fn from_iter<T: IntoIterator<Item = (String, Value)>>(iter: T) -> Self {
        let mut p = Params::new();
        for (k, v) in iter {
            p.insert(&k, v);
        }
        p
    }
}

/// Runs user-defined functions (`udf.name(args)`) for the evaluator; made by a [`UdfFactory`] for each execution. The service implements it over its own script engine; this
/// crate has no JavaScript.
pub trait UdfHost {
    /// Calls `udf.<name>` with the arguments (`None` is an undefined argument). `Ok(None)` is an undefined result.
    /// The `Err` text is the message of the query error.
    fn call(&self, name: &str, args: &[Option<Value>]) -> Result<Option<Value>, String>;
}

/// Makes the [`UdfHost`] of one query execution, on the thread that runs it (a script engine cannot move between threads,
/// while [`EvalOptions`] can).
pub trait UdfFactory: std::fmt::Debug + Send + Sync {
    fn make(&self) -> Box<dyn UdfHost>;
}

/// Options for [`Query::execute`].
#[derive(Debug, Clone)]
pub struct EvalOptions {
    /// Fixed clock for the date/time functions (`GETCURRENTDATETIME`, `NOW`, `AGO`, ...).
    /// `None` reads the system clock (the `...STATIC` functions read it once per execution).
    pub now: Option<SystemTime>,
    /// Seed of the `RAND()` generator. `None` derives it from the clock.
    pub rand_seed: Option<u64>,
    /// Upper bound for the rows a query produces while it runs (every row of FROM and of each JOIN counts,
    /// whether or not WHERE keeps it): a limit on the work of one query. Default 5 million.
    pub max_rows: usize,
    /// Upper bound, in bytes, for what one query holds at the same time: the rows it collects for ORDER BY,
    /// DISTINCT and the result, its groups and their running aggregates. A join does not count (its rows are
    /// handed on one at a time). Default 64 MiB; a query over it fails with `ErrorKind::LimitExceeded`.
    pub max_bytes: usize,
    /// Runs `udf.name(..)` calls. Without it such a call is an "unsupported" error.
    pub udf: Option<Arc<dyn UdfFactory>>,
}

impl Default for EvalOptions {
    fn default() -> Self {
        EvalOptions { now: None, rand_seed: None, max_rows: 5_000_000, max_bytes: 64 * 1024 * 1024, udf: None }
    }
}

/// A parsed and checked query.
#[derive(Debug, Clone)]
pub struct Query {
    ast: ast::Query,
    source: String,
}

/// Texts longer than this run their recursive work (parse, check, evaluate) on a thread with a big stack: a query may nest
/// hundreds of levels (Azure takes 300 parentheses and a chain of 3000 terms), which needs more than the 2 MiB of a worker thread
/// in a debug build. Short queries, which is nearly all of them, run where they are.
/// Deepest array or object nesting a result may have.
const MAX_OUTPUT_DEPTH: usize = 256;
const DEEP_QUERY_BYTES: usize = 400;
const BIG_STACK: usize = 256 * 1024 * 1024;

fn big_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    let mut f = Some(f);
    let out = std::thread::scope(|s| {
        let slot = &mut f;
        match std::thread::Builder::new().stack_size(BIG_STACK).spawn_scoped(s, move || (slot.take().expect("run once"))()) {
            Ok(h) => Some(h.join()),
            Err(_) => None,
        }
    });
    match out {
        Some(Ok(v)) => v,
        Some(Err(panic)) => std::panic::resume_unwind(panic),
        // no thread to be had: run here
        None => (f.take().expect("not run"))(),
    }
}

/// Whether the text can nest deeply enough to need the big stack: long, or brackets more than a dozen levels deep.
fn is_deep(sql: &str) -> bool {
    if sql.len() > DEEP_QUERY_BYTES {
        return true;
    }
    let (mut depth, mut max) = (0usize, 0usize);
    for b in sql.bytes() {
        match b {
            b'(' | b'[' | b'{' => {
                depth += 1;
                max = max.max(depth);
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max > 12
}

fn deep<T: Send>(sql: &str, f: impl FnOnce() -> T + Send) -> T {
    if is_deep(sql) {
        big_stack(f)
    } else {
        f()
    }
}

/// Parses a query. Errors carry Cosmos-style codes and character positions.
pub fn parse(sql: &str) -> Result<Query, SqlError> {
    deep(sql, || {
        let ast = parser::parse_text(sql)?;
        validate::validate(&ast, sql)?;
        Ok(Query { ast, source: sql.to_string() })
    })
}

impl Query {
    /// The syntax tree.
    pub fn ast(&self) -> &ast::Query {
        &self.ast
    }

    /// The query text as given to [`parse`].
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Runs the whole query (FROM, JOIN, WHERE, GROUP BY, ORDER BY, DISTINCT, OFFSET/LIMIT, TOP,
    /// projection) over the documents of one container and returns the result items in the form
    /// Cosmos puts in the `Documents` array.
    pub fn execute<'a, I>(&self, docs: I, params: &Params, opts: &EvalOptions) -> Result<Vec<Value>, SqlError>
    where
        I: IntoIterator<Item = &'a Value>,
    {
        let docs = eval::query::DocSet::Borrowed(docs.into_iter().collect());
        Ok(self.run_docs(&docs, params, opts)?.into_iter().map(Item::into_value).collect())
    }

    /// Like [`Query::execute`] over documents the caller shares (`Arc`). Nothing is copied that the query
    /// does not change: a document that comes out as it went in (`SELECT *`, `SELECT VALUE c`, a filter) is
    /// returned as the same `Arc` ([`Item::Shared`]), and a property path reads the JSON of the document
    /// without converting it. Memory use is bounded by [`EvalOptions::max_bytes`].
    pub fn execute_shared(&self, docs: &[Arc<Value>], params: &Params, opts: &EvalOptions) -> Result<Vec<Item>, SqlError> {
        self.run_docs(&eval::query::DocSet::Shared(docs), params, opts)
    }

    fn run_docs(&self, docs: &eval::query::DocSet, params: &Params, opts: &EvalOptions) -> Result<Vec<Item>, SqlError> {
        deep(&self.source, || self.run_docs_here(docs, params, opts))
    }

    fn run_docs_here(&self, docs: &eval::query::DocSet, params: &Params, opts: &EvalOptions) -> Result<Vec<Item>, SqlError> {
        let ctx = eval::Ctx::new(params, opts);
        let rows = eval::query::run(&ctx, &self.ast, eval::query::Scope::Container(docs), None)?;
        let mut out = Vec::with_capacity(rows.len());
        for v in rows {
            // JSON cannot hold NaN or Infinity: Azure fails the request instead of writing `null`
            if v.has_non_finite() {
                return Err(SqlError::coded(
                    4001,
                    "The response content contains a numeric value that cannot be represented in JSON (e.g. Infinity and NaN).",
                    Some(4001),
                ));
            }
            if v.depth_exceeds(MAX_OUTPUT_DEPTH) {
                return Err(SqlError::gateway(format!(
                    "The maximum nesting level exceeded the maximum limit of {MAX_OUTPUT_DEPTH} in your query's output. Ensure that the nesting level doesn't exceed our supported value of {MAX_OUTPUT_DEPTH}."
                )));
            }
            match v {
                value::Val::Root(d) => out.push(Item::Shared(d)),
                other => out.extend(other.to_json().map(Item::Owned)),
            }
        }
        Ok(out)
    }

    /// Runs the query in the form a partition returns it to the SDK after a query plan
    /// request (see the crate docs): `parse(info.rewritten_query)` then `execute`, or plain
    /// `execute` when no rewriting is needed.
    pub fn execute_rewritten<'a, I>(&self, docs: I, params: &Params, opts: &EvalOptions) -> Result<Vec<Value>, SqlError>
    where
        I: IntoIterator<Item = &'a Value>,
    {
        let info = self.info(params);
        if info.rewritten_query.is_empty() {
            return self.execute(docs, params, opts);
        }
        parse(&info.rewritten_query)?.execute(docs, params, opts)
    }

    /// Facts of the query plan. `params` resolves `TOP @n` and `OFFSET @a LIMIT @b`.
    pub fn info(&self, params: &Params) -> QueryInfo {
        deep(&self.source, || analysis::info(&self.ast, &self.source, params, PlanOptions::default()))
    }

    /// Like [`Query::info`], for an SDK that reads the partitions' answers in the forms `opts` names.
    pub fn info_for(&self, params: &Params, opts: PlanOptions) -> QueryInfo {
        deep(&self.source, || analysis::info(&self.ast, &self.source, params, opts))
    }

    /// Checks the counts of TOP, OFFSET and LIMIT against the parameters (a parameter that is not a whole number from 0 up is an
    /// error on the text of the parameter). Azure does this when it compiles the query, before any document is read.
    pub fn check_counts(&self, params: &Params) -> Result<(), SqlError> {
        let ctx = eval::Ctx::new(params, &EvalOptions::default());
        let env = eval::Env::empty();
        if let Some(t) = &self.ast.select.top {
            eval::query::count_value(&ctx, t, &env, ("SC2060", "The count value provided for a TOP clause must be an integer."))?;
        }
        if let Some(ol) = &self.ast.offset_limit {
            eval::query::count_value(&ctx, &ol.offset, &env, ("SC2062", "The OFFSET count value exceeds the maximum allowed value."))?;
            eval::query::count_value(&ctx, &ol.limit, &env, ("SC2061", "The LIMIT count value exceeds the maximum allowed value."))?;
        }
        Ok(())
    }

    /// The names of the user-defined functions the query calls (`udf.name(..)`), each once, in order of appearance.
    pub fn udf_names(&self) -> Vec<String> {
        deep(&self.source, || {
            let mut out: Vec<String> = Vec::new();
            analysis::udf_names(&self.ast, &mut out);
            out
        })
    }

    /// The query features a client has to declare to be served this query across partitions, by the names of
    /// `x-ms-cosmos-supported-query-features` (`"OrderBy"`, `"GroupBy"`, `"NonValueAggregate"`, ...).
    pub fn required_features(&self) -> Vec<&'static str> {
        deep(&self.source, || analysis::required_features(&self.ast))
    }

    /// `SELECT VALUE` of an expression that holds an aggregate and is more than the aggregate (`COUNT(1) + 1`): such a
    /// query is refused across partitions.
    pub fn has_composed_aggregate(&self) -> bool {
        deep(&self.source, || analysis::has_composed_aggregate(&self.ast))
    }

    /// A select list without VALUE that has an aggregate in one of its items.
    pub fn has_select_list_aggregate(&self) -> bool {
        deep(&self.source, || analysis::has_select_list_aggregate(&self.ast))
    }

    /// True if the SDK needs a query plan for this query: it has ORDER BY, an aggregate, GROUP BY,
    /// DISTINCT, OFFSET/LIMIT or TOP. A gateway can serve all other queries directly.
    pub fn needs_query_plan(&self) -> bool {
        plan_needed(&self.ast)
    }

    /// If `WHERE` pins every partition key path to one value through equality (`c.pk = 'a' AND ...`),
    /// returns the values in the order of `pk_paths` (paths like `"/pk"` or `"/a/b"`).
    /// Returns `None` when the filter does not pin the key (OR, missing path, conflicting values,
    /// FROM not directly on the container).
    pub fn partition_key_filter(&self, pk_paths: &[String], params: &Params) -> Option<Vec<Value>> {
        analysis::partition_key_filter(&self.ast, pk_paths, params)
    }
}

/// A container-scoped subquery in FROM counts like the outer query.
fn plan_needed(a: &ast::Query) -> bool {
    if a.select.distinct || a.select.top.is_some() || a.offset_limit.is_some() || !a.order_by.is_empty() || a.is_aggregate {
        return true;
    }
    match a.from.as_ref().map(|f| &f.sources[0].source) {
        Some(ast::SourceExpr::Subquery(sq)) => plan_needed(sq),
        _ => false,
    }
}
