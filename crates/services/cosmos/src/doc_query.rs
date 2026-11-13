//! Queries over the documents of a container, plus the query plan the SDKs ask for.
//!
//! Design: a container is one physical partition ("0", `""` to `"FF"`).
//! * A request pinned to one logical partition (partition key header, or a `WHERE` that
//!   pins the key) runs the query as written.
//! * A cross partition request without `x-ms-documentdb-query-enablecrosspartition` is a
//!   400, as on the real service. With it, a query that needs client side merging
//!   (ORDER BY, aggregates, GROUP BY, DISTINCT, TOP, OFFSET LIMIT) is answered with
//!   400 / sub-status 1004 and the plan in `additionalErrorInfo`; the SDK then fetches the
//!   plan and the partition ranges and re-sends the rewritten query to range "0", which
//!   runs as written. Everything else runs directly.
//! * Paging: the result of a query with more pages is kept behind the continuation token
//!   (see `paging.rs`); the query runs after the service lock is released (`QueryJob`).

use std::sync::Arc;

use serde_json::{json, Value};

use azemu_cosmos_sql::{Item, PlanOptions};

use crate::errors::{CosmosError, R};
use crate::out::{query_charge, Cx, Out};
use crate::paging::{self, PageCache};
use crate::pk::PkComp;
use crate::query::{self, Prepared};
use crate::select::Selector;
use crate::state::{Container, Doc};

const DEFAULT_PAGE: usize = 100;

/// What the SDK that sends this request can read in the answers of the partitions. The JavaScript SDK
/// (query version 1.4, all query features) reads MIN and MAX as `{"min": .., "count": ..}`; the Python SDK
/// (query version 1.0) reads the bare value. A client that says it is at least 1.4 and that it takes
/// composite aggregates gets the first form, every other client the second.
fn plan_options(cx: &Cx) -> PlanOptions {
    let version_ok = cx
        .header("x-ms-cosmos-query-version")
        .and_then(|v| {
            let (major, minor) = v.trim().split_once('.')?;
            Some((major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?) >= (1, 4))
        })
        .unwrap_or(false);
    let declares = |feature: &str| {
        cx.header("x-ms-cosmos-supported-query-features")
            .map(|f| f.split(',').any(|x| x.trim().eq_ignore_ascii_case(feature)))
            .unwrap_or(false)
    };
    PlanOptions { composite_min_max: version_ok && declares("CompositeAggregate"), non_value_aggregate: declares("NonValueAggregate") }
}

pub fn plan_json(p: &Prepared, opts: PlanOptions) -> Value {
    json!({
        "partitionedQueryExecutionInfoVersion": 2,
        "queryInfo": p.query.info_for(&p.params, opts).to_json(),
        "queryRanges": [{
            "min": "",
            "max": "FF",
            "isMinInclusive": true,
            "isMaxInclusive": false
        }]
    })
}

pub fn plan_request(cx: &Cx) -> R<Out> {
    let body = query::parse_body(cx)?;
    let p = query::prepare(&body)?;
    Ok(Out::json(200, plan_json(&p, plan_options(cx))).header("x-ms-documentdb-query-metrics", "").charge(1.0))
}

fn metrics(scanned: usize, out: usize, bytes: usize) -> String {
    format!(
        "totalExecutionTimeInMs=0.30;queryCompileTimeInMs=0.05;queryLogicalPlanBuildTimeInMs=0.01;\
         queryPhysicalPlanBuildTimeInMs=0.02;queryOptimizationTimeInMs=0.00;VMExecutionTimeInMs=0.10;\
         indexLookupTimeInMs=0.02;instructionCount=41;documentLoadTimeInMs=0.04;\
         systemFunctionExecuteTimeInMs=0.00;userFunctionExecuteTimeInMs=0.00;\
         retrievedDocumentCount={scanned};retrievedDocumentSize={bytes};outputDocumentCount={out};\
         outputDocumentSize={bytes};writeOutputTimeInMs=0.01;indexUtilizationRatio=1.00"
    )
}

/// Composite-index requirement: more than one ORDER BY field needs a composite index with
/// the same paths in the same order (all directions equal or all inverted).
fn check_order_by(c: &Container, p: &Prepared) -> R<()> {
    let info = p.query.info(&p.params);
    if info.order_by_expressions.len() < 2 {
        return Ok(());
    }
    let wanted: Vec<(String, bool)> = info
        .order_by_expressions
        .iter()
        .zip(info.order_by.iter())
        .map(|(e, o)| (order_path(e), matches!(o, azemu_cosmos_sql::SortOrder::Ascending)))
        .collect();
    let comps =
        c.props.get("indexingPolicy").and_then(|p| p.get("compositeIndexes")).and_then(Value::as_array).cloned().unwrap_or_default();
    for comp in &comps {
        let Some(items) = comp.as_array() else { continue };
        if items.len() != wanted.len() {
            continue;
        }
        let have: Vec<(String, bool)> = items
            .iter()
            .map(|i| {
                (
                    i.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
                    i.get("order").and_then(Value::as_str).unwrap_or("ascending").eq_ignore_ascii_case("ascending"),
                )
            })
            .collect();
        let same = have.iter().zip(&wanted).all(|(h, w)| h.0 == w.0 && h.1 == w.1);
        let inverted = have.iter().zip(&wanted).all(|(h, w)| h.0 == w.0 && h.1 != w.1);
        if same || inverted {
            return Ok(());
        }
    }
    Err(CosmosError::bad_request("The order by query does not have a corresponding composite index that it can be served from."))
}

/// Whether ORDER BY leaves out the documents that lack the sort property.
///
/// Azure serves ORDER BY from the index, and a document is only in the index of a path when
/// the path is defined, unless the path is indexed explicitly. Microsoft Learn, "Indexing
/// policies" (include/exclude strategy): "If an explicitly indexed path doesn't exist in an
/// item, a value is added to the index to indicate that the path is undefined"; the same for
/// composite indexes ("Composite indexes": "If a path in a composite index doesn't exist in an
/// item ... a value is added to the index to indicate that the path is undefined"). So:
/// * one ORDER BY field covered only by the default `/*` path: documents without it are left out;
/// * one field with its own included path (`/field/?`): they are returned, undefined first;
/// * several fields (a composite index is required, see `check_order_by`): returned.
fn order_by_excludes_undefined(c: &Container, p: &Prepared) -> bool {
    let info = p.query.info(&p.params);
    if info.order_by_expressions.len() != 1 {
        return false;
    }
    let path = order_path(&info.order_by_expressions[0]);
    let explicit = c
        .props
        .get("indexingPolicy")
        .and_then(|p| p.get("includedPaths"))
        .and_then(Value::as_array)
        .map(|inc| {
            inc.iter().filter_map(|i| i.get("path").and_then(Value::as_str)).any(|ip| {
                let ip = ip.replace('"', "");
                ip == format!("{path}/?")
            })
        })
        .unwrap_or(false);
    !explicit
}

/// `c.a.b` or `c["a"]["b"]` to `/a/b`.
fn order_path(expr: &str) -> String {
    let rest = expr.trim();
    let rest = match rest.find(['.', '[']) {
        Some(i) => &rest[i..],
        None => return String::new(),
    };
    let mut out = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '.' => {
                out.push('/');
            }
            '[' => {
                out.push('/');
                let mut name = String::new();
                for n in chars.by_ref() {
                    if n == ']' {
                        break;
                    }
                    name.push(n);
                }
                out.push_str(name.trim_matches(|x| x == '"' || x == '\''));
            }
            _ => out.push(ch),
        }
    }
    out
}

pub fn handle(cx: &Cx, c: &mut Container) -> R<Out> {
    c.purge_expired(cx.now);
    let body = query::parse_body(cx)?;
    let p = query::prepare(&body)?;
    let udf_names = p.query.udf_names();
    let udfs = if udf_names.is_empty() {
        Vec::new()
    } else {
        let all: Vec<(String, String)> =
            c.udfs.iter().filter_map(|(id, r)| r.props.get("body").and_then(Value::as_str).map(|b| (id.clone(), b.to_string()))).collect();
        crate::udf::definitions(&all, &udf_names)?
    };
    let sel = Selector::from_headers(cx, c)?;
    let ranged = cx.header("x-ms-documentdb-partitionkeyrangeid").is_some();
    let mut pinned: Option<Vec<PkComp>> = sel.pk.clone().filter(|v| v.len() == c.pk.paths.len());
    if pinned.is_none() && !ranged && sel.pk.is_none() {
        if let Some(vals) = p.query.partition_key_filter(&c.pk.paths, &p.params) {
            let comps: R<Vec<PkComp>> = vals.iter().map(PkComp::from_json).collect();
            if let Ok(k) = comps {
                if k.len() == c.pk.paths.len() {
                    pinned = Some(k);
                }
            }
        }
    }
    let one_partition = pinned.is_some() || sel.pk.is_some() || ranged;
    if !one_partition {
        if !cx.flag("x-ms-documentdb-query-enablecrosspartition") {
            return Err(CosmosError::bad_request(
                "Cross partition query is required but disabled. Please set x-ms-documentdb-query-enablecrosspartition to true, specify x-ms-documentdb-partitionkey, or revise your query to avoid this exception.",
            ));
        }
        if p.query.needs_query_plan() {
            let mut e = CosmosError::bad_request(
                "The provided cross partition query can not be directly served by the gateway. This is a first chance (internal) exception that all newer clients will know how to handle gracefully. This exception is traced, but unless you see it bubble up as an exception (which only happens on older SDK clients), then you can safely ignore this message.",
            )
            .sub(1004);
            e.additional_info = Some(plan_json(&p, plan_options(cx)).to_string());
            return Err(e);
        }
    }
    check_order_by(c, &p)?;
    let sig = signature(cx, &c.meta.rid);
    let tok = paging::decode(cx.header("x-ms-continuation"));
    // A later page of a query whose result is kept needs no documents at all.
    let hit = match (tok.cache_id, &cx.cache) {
        (Some(id), Some(cache)) => cache.get(id, sig, cx.now),
        _ => None,
    };
    let docs: Vec<Arc<Value>> = if hit.is_some() {
        Vec::new()
    } else {
        let found: Vec<&Doc> = match (&pinned, &sel.pk) {
            (Some(k), None) => c.partition(k).map(|(_, d)| d).collect(),
            _ => sel.docs(c),
        };
        found.into_iter().map(|d| d.value.clone()).collect()
    };
    let scanned = docs.len();
    let excludes_undefined = order_by_excludes_undefined(c, &p);
    Ok(Out::deferred(QueryJob {
        excludes_undefined,
        p,
        docs,
        scanned,
        hit,
        sig,
        size: cx.max_item_count().unwrap_or(DEFAULT_PAGE),
        token: tok,
        rid: c.meta.rid.clone(),
        lsn: c.lsn,
        metrics: cx.flag("x-ms-documentdb-populatequerymetrics"),
        now: cx.now,
        udfs,
        settings: cx.settings.clone().unwrap_or_default(),
    }))
}

/// Identifies a request for the page cache: the same container, path, query text, parameters and
/// addressing headers.
fn signature(cx: &Cx, container_rid: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    cx.req.account.hash(&mut h);
    // a container deleted and made again under the same name is another container
    container_rid.hash(&mut h);
    cx.req.path.hash(&mut h);
    cx.req.body.hash(&mut h);
    for name in ["x-ms-documentdb-partitionkey", "x-ms-documentdb-partitionkeyrangeid", "x-ms-start-epk", "x-ms-end-epk"] {
        cx.header(name).hash(&mut h);
    }
    h.finish()
}

/// A query ready to run over a snapshot of the documents. It owns everything it needs, so it
/// runs after the service lock is released (and off the async worker thread).
pub struct QueryJob {
    excludes_undefined: bool,
    p: Prepared,
    docs: Vec<Arc<Value>>,
    scanned: usize,
    /// The kept result of an earlier page, when the token named one that is still there.
    hit: Option<(Arc<Vec<Item>>, usize)>,
    sig: u64,
    size: usize,
    token: paging::Token,
    rid: String,
    lsn: u64,
    metrics: bool,
    now: i64,
    /// The user-defined functions the query calls (name, body).
    udfs: Vec<(String, String)>,
    settings: Arc<crate::js::ScriptSettings>,
}

impl QueryJob {
    /// The query calls user-defined functions: it must run on a script worker thread (`js::spawn_worker`).
    pub fn uses_udf(&self) -> bool {
        !self.udfs.is_empty()
    }

    pub fn run(self, cache: Option<&PageCache>) -> R<Out> {
        let QueryJob { excludes_undefined, p, docs, scanned, hit, sig, size, token, rid, lsn, metrics: want_metrics, now, udfs, settings } =
            self;
        let _running = cache.map(|c| c.running_guard());
        let (rows, scanned, cached_id) = match hit {
            Some((rows, scanned)) => (rows, scanned, token.cache_id),
            None => {
                if let Some(c) = cache {
                    c.count_execution();
                }
                let mut opts = query::eval_options_at(now);
                opts.order_by_excludes_undefined = excludes_undefined;
                if !udfs.is_empty() {
                    opts.udf = Some(Arc::new(crate::udf::UdfSource::new(udfs, &settings, std::time::Instant::now() + settings.timeout)));
                }
                let rows = p.query.execute_shared(&docs, &p.params, &opts).map_err(|e| CosmosError::from_sql(&e))?;
                (Arc::new(rows), scanned, None)
            }
        };
        let n = paging::page_len(&rows, token.skip, Some(size));
        let end = token.skip.saturating_add(n);
        // only the rows of this page are copied out of the (shared) result
        let page: Vec<Value> = rows.get(token.skip..end.min(rows.len())).unwrap_or(&[]).iter().map(|r| r.value().clone()).collect();
        let next = if end < rows.len() {
            let id = match (cached_id, cache) {
                (Some(id), _) => Some(id),
                (None, Some(c)) => c.put(sig, rows.clone(), scanned, now),
                (None, None) => None,
            };
            Some(paging::encode(&paging::Token { skip: end, cache_id: id, after: None }))
        } else {
            if let (Some(id), Some(c)) = (cached_id, cache) {
                c.remove(id);
            }
            None
        };
        let n = page.len();
        let bytes = if want_metrics { serde_json::to_vec(&page).map(|v| v.len()).unwrap_or(0) } else { 0 };
        let mut out = Out::json(200, query::feed_body(&rid, "Documents", page))
            .header("x-ms-item-count", n)
            .charge(query_charge(scanned, n))
            .lsn(lsn);
        if want_metrics {
            out = out.header("x-ms-documentdb-query-metrics", metrics(scanned, n, bytes));
        }
        if let Some(t) = next {
            out = out.header("x-ms-continuation", t);
        }
        Ok(out)
    }
}
