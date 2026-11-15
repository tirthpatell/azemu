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

/// The query features the client says it supports (`x-ms-cosmos-supported-query-features`, comma separated). A request
/// without the header supports none (a real account refuses an ORDER BY plan for it).
fn declared_features(cx: &Cx) -> Vec<String> {
    cx.header("x-ms-cosmos-supported-query-features")
        .map(|f| f.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default()
}

/// What the SDK that sends this request can read in the answers of the partitions. A real account plans MIN and MAX with
/// `{"item": .., "item2": {"min": .., "count": ..}}` for every client (the Python SDK reads `item`, the JavaScript and Java SDKs
/// read `item2`); a client that lists `NonValueAggregate` gets a select list of aggregates planned as a group of one.
fn plan_options(cx: &Cx) -> PlanOptions {
    let features = declared_features(cx);
    let declares = |feature: &str| features.iter().any(|x| x.eq_ignore_ascii_case(feature));
    PlanOptions { composite_min_max: true, non_value_aggregate: declares("NonValueAggregate") }
}

/// A cross partition query that Azure refuses before it plans anything.
fn check_cross_partition(p: &Prepared) -> R<()> {
    if p.query.has_composed_aggregate() {
        return Err(CosmosError::bad_request("Compositions of aggregates and other expressions are not allowed."));
    }
    Ok(())
}

/// The plan request of a client that cannot read what the query needs is a 400 that names each missing feature.
fn check_client_features(cx: &Cx, p: &Prepared) -> R<()> {
    let declared = declared_features(cx);
    let missing: Vec<&str> =
        p.query.required_features().into_iter().filter(|f| !declared.iter().any(|d| d.eq_ignore_ascii_case(f))).collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut msg = String::from(
        "Query contains 1 or more unsupported features. Upgrade your SDK to a version that does support the requested features:",
    );
    for f in missing {
        msg.push_str(&format!("\r\nQuery contained {f}, which the calling client does not support."));
    }
    Err(CosmosError::plain(400, "BadRequest", msg))
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
    check_cross_partition(&p)?;
    check_client_features(cx, &p)?;
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

/// ORDER BY is served from the index: the path has to be indexed, and several fields need a composite index with the same
/// paths in the same order (all directions equal or all inverted). The texts are the ones a real account answers.
fn check_order_by(c: &Container, p: &Prepared) -> R<()> {
    let info = p.query.info(&p.params);
    if info.order_by_expressions.is_empty() {
        return Ok(());
    }
    let policy = c.props.get("indexingPolicy");
    if policy.and_then(|p| p.get("indexingMode")).and_then(Value::as_str).is_some_and(|m| m.eq_ignore_ascii_case("none")) {
        return Err(CosmosError::bad_request(
            "A query has been specified for a document collection with indexing mode 'none'. Consider adding allow scan header in the request.",
        ));
    }
    if info.order_by_expressions.len() < 2 {
        let path = order_path(&info.order_by_expressions[0]);
        if path == "/_rid" {
            return Err(CosmosError::bad_request("Primary key order-by is not supported."));
        }
        // `id` and `_ts` are always in the index
        if path.is_empty() || !(matches!(path.as_str(), "/id" | "/_ts") || path_is_indexed(policy, &path)) {
            return Err(CosmosError::bad_request("The index path corresponding to the specified order-by item is excluded."));
        }
        return Ok(());
    }
    let wanted: Vec<(String, bool)> = info
        .order_by_expressions
        .iter()
        .zip(info.order_by.iter())
        .map(|(e, o)| (order_path(e), matches!(o, azemu_cosmos_sql::SortOrder::Ascending)))
        .collect();
    let comps = policy.and_then(|p| p.get("compositeIndexes")).and_then(Value::as_array).cloned().unwrap_or_default();
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

/// The segments of an index path: `/a/b/?` is `a`, `b` and a `?`; `/*` is no segment and a `*`.
fn index_rule(path: &str) -> (Vec<String>, bool) {
    let mut segs: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(|s| s.replace('"', "")).collect();
    let wildcard = !matches!(segs.last().map(String::as_str), Some("?"));
    if matches!(segs.last().map(String::as_str), Some("?") | Some("*")) {
        segs.pop();
    }
    (segs, wildcard)
}

/// Whether the index covers `path` (like `/a/b`): the most specific of the included and excluded paths that matches it
/// decides; `/a/?` covers `/a` only, `/a/*` covers `/a` and everything below it. Without a policy `/*` is included.
fn path_is_indexed(policy: Option<&Value>, path: &str) -> bool {
    let target: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
    let listed = |key: &str| -> Vec<String> {
        policy
            .and_then(|p| p.get(key))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|i| i.get("path").and_then(Value::as_str)).map(str::to_string).collect())
            .unwrap_or_default()
    };
    let mut included = listed("includedPaths");
    if policy.and_then(|p| p.get("includedPaths")).is_none() {
        included.push("/*".to_string());
    }
    let excluded = listed("excludedPaths");
    let mut best: Option<(usize, bool, bool)> = None; // (segments, is_included, exact rule)
    for (paths, is_included) in [(&included, true), (&excluded, false)] {
        for rule in paths {
            let (segs, wildcard) = index_rule(rule);
            let matches = if wildcard { target.len() >= segs.len() && target[..segs.len()] == segs[..] } else { target == segs };
            if !matches {
                continue;
            }
            let better = match best {
                None => true,
                Some((n, inc, _)) => segs.len() > n || (segs.len() == n && inc && !is_included),
            };
            if better {
                best = Some((segs.len(), is_included, !wildcard));
            }
        }
    }
    best.is_some_and(|(_, inc, _)| inc)
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
            // what the gateway refuses before it asks the client for a plan
            check_cross_partition(&p)?;
            if p.query.has_select_list_aggregate() {
                return Err(CosmosError::bad_request("Cross partition query only supports 'VALUE <AggregateFunc>' for aggregates."));
            }
            let mut e = CosmosError::plain(
                400,
                "BadRequest",
                "The provided cross partition query can not be directly served by the gateway. This is a first chance (internal) exception that all newer clients will know how to handle gracefully. This exception is traced, but unless you see it bubble up as an exception (which only happens on older SDK clients), then you can safely ignore this message.",
            )
            .sub(1004);
            e.additional_info = Some(plan_json(&p, plan_options(cx)).to_string());
            return Err(e);
        }
    }
    check_order_by(c, &p)?;
    let sig = signature(cx, &c.meta.rid);
    let tok = paging::decode(cx.header("x-ms-continuation"), &c.meta.rid)?;
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
        // documents come in the order they were created (the order of an index scan on Azure), which decides the rows
        // with equal sort keys, the first appearance of a group or a DISTINCT value
        let mut found: Vec<(u64, &Doc)> = found.into_iter().map(|d| (creation_number(&d.rid), d)).collect();
        found.sort_by_key(|(n, _)| *n);
        found.into_iter().map(|(_, d)| d.value.clone()).collect()
    };
    let scanned = docs.len();
    Ok(Out::deferred(QueryJob {
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

/// The number a document got when it was created, read from its resource id (the counter sits in the first seven of the
/// last eight bytes, little endian; `rid.rs`). 0 for an id that cannot be read.
fn creation_number(rid: &str) -> u64 {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    let Ok(bytes) = STANDARD.decode(rid.replace('-', "/")) else { return 0 };
    if bytes.len() < 16 {
        return 0;
    }
    let mut eight = [0u8; 8];
    eight.copy_from_slice(&bytes[bytes.len() - 8..]);
    eight[7] = 0;
    u64::from_le_bytes(eight)
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
        let QueryJob { p, docs, scanned, hit, sig, size, token, rid, lsn, metrics: want_metrics, now, udfs, settings } = self;
        let _running = cache.map(|c| c.running_guard());
        let (rows, scanned, cached_id) = match hit {
            Some((rows, scanned)) => (rows, scanned, token.cache_id),
            None => {
                if let Some(c) = cache {
                    c.count_execution();
                }
                let mut opts = query::eval_options_at(now);
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
            Some(paging::encode(&paging::Token { skip: end, cache_id: id, after: None }, &rid))
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
