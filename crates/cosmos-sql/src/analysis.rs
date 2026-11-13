//! Query analysis: the facts the SDKs ask for in a query plan, the rewritten queries the
//! SDKs expect, and partition-key extraction.

use crate::ast::*;
use crate::eval::query::inferred_name;
use crate::Params;
use serde_json::{json, Value};

/// How DISTINCT is evaluated across partitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistinctType {
    /// No DISTINCT.
    None,
    /// DISTINCT together with ORDER BY (duplicates are adjacent).
    Ordered,
    /// DISTINCT without ORDER BY.
    Unordered,
}

impl DistinctType {
    /// `"None"`, `"Ordered"` or `"Unordered"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            DistinctType::None => "None",
            DistinctType::Ordered => "Ordered",
            DistinctType::Unordered => "Unordered",
        }
    }
}

/// ORDER BY direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    /// `ASC`
    Ascending,
    /// `DESC`
    Descending,
}

impl SortOrder {
    /// `"Ascending"` or `"Descending"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            SortOrder::Ascending => "Ascending",
            SortOrder::Descending => "Descending",
        }
    }
}

/// What the SDK that asks for the plan can read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanOptions {
    /// The partitions answer MIN and MAX as `{"min": MIN(x), "count": COUNT(x)}` (`{"max": ..., "count": ...}`)
    /// instead of the bare value. The JavaScript SDK's `MinAggregator` and `MaxAggregator` read that form
    /// (the count is 0 for a partition without a value, and `min` is then missing); the Python SDK reads
    /// the bare value. The gateway chooses by the query version and the query features the SDK declares
    /// (see `azemu-cosmos` `doc_query.rs`).
    pub composite_min_max: bool,
    /// The client declares the `NonValueAggregate` query feature (the Java and JavaScript SDKs do, the Python SDK does
    /// not): a select list of aggregates without `VALUE` and without GROUP BY (`SELECT COUNT(1) AS n, MAX(c.x) AS hi`)
    /// is planned like a group by with one group: each partition answers `{"payload": {"n": {"item": ..}, ..}}`, and
    /// the plan lists the aliases (`groupByAliases`) and their aggregates (`groupByAliasToAggregateType`).
    /// Without the feature the partitions answer one array of `{"item": ..}` entries.
    pub non_value_aggregate: bool,
}

/// The `queryInfo` part of a Cosmos `partitionedQueryExecutionInfo` (query plan).
#[derive(Debug, Clone, PartialEq)]
pub struct QueryInfo {
    /// DISTINCT handling.
    pub distinct_type: DistinctType,
    /// `TOP n`
    pub top: Option<u64>,
    /// `OFFSET n`
    pub offset: Option<u64>,
    /// `LIMIT m`
    pub limit: Option<u64>,
    /// Direction of each ORDER BY item.
    pub order_by: Vec<SortOrder>,
    /// Source text of each ORDER BY expression.
    pub order_by_expressions: Vec<String>,
    /// Source text of each GROUP BY expression.
    pub group_by_expressions: Vec<String>,
    /// Output names of the select list of a GROUP BY query.
    pub group_by_aliases: Vec<String>,
    /// Aggregates directly in the select list (no GROUP BY): e.g. `[Count]` for `SELECT VALUE COUNT(1)`.
    pub aggregates: Vec<AggregateKind>,
    /// For GROUP BY queries: select-list name to aggregate (`None` for a plain group key).
    pub group_by_alias_to_aggregate_type: Vec<(String, Option<AggregateKind>)>,
    /// `SELECT VALUE`
    pub has_select_value: bool,
    /// Always false (vector / rank ordering is not supported).
    pub has_non_streaming_order_by: bool,
    /// Query text for the partitions, or empty when the original text can be used as is.
    pub rewritten_query: String,
}

impl QueryInfo {
    /// The `queryInfo` JSON object as the gateway sends it.
    pub fn to_json(&self) -> Value {
        let mut alias_map = serde_json::Map::new();
        for (a, k) in &self.group_by_alias_to_aggregate_type {
            alias_map.insert(a.clone(), k.map(|k| Value::from(k.as_str())).unwrap_or(Value::Null));
        }
        json!({
            "distinctType": self.distinct_type.as_str(),
            "top": self.top,
            "offset": self.offset,
            "limit": self.limit,
            "orderBy": self.order_by.iter().map(|o| o.as_str()).collect::<Vec<_>>(),
            "orderByExpressions": self.order_by_expressions,
            "groupByExpressions": self.group_by_expressions,
            "groupByAliases": self.group_by_aliases,
            "aggregates": self.aggregates.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            "groupByAliasToAggregateType": Value::Object(alias_map),
            "rewrittenQuery": self.rewritten_query,
            "hasSelectValue": self.has_select_value,
            "hasNonStreamingOrderBy": self.has_non_streaming_order_by,
            "dCountInfo": Value::Null,
        })
    }
}

/// Placeholder the gateway puts in rewritten ORDER BY queries. `parse` accepts it as `true`.
pub const ORDER_BY_FILTER_PLACEHOLDER: &str = "{documentdb-formattableorderbyquery-filter}";

fn resolve_u64(e: &Expr, params: &Params) -> Option<u64> {
    match &e.kind {
        ExprKind::Literal(Literal::Int(i)) if *i >= 0 => Some(*i as u64),
        ExprKind::Param(name) => params.get(name).and_then(|v| v.as_u64()),
        _ => None,
    }
}

struct Ctx<'a> {
    q: &'a Query,
    src: Vec<char>,
    opts: PlanOptions,
}

impl Ctx<'_> {
    fn text(&self, s: Span) -> String {
        self.src[s.start.min(self.src.len())..s.end.min(self.src.len())].iter().collect()
    }
    fn etext(&self, e: &Expr) -> String {
        self.text(e.span)
    }
    fn first_alias(&self) -> String {
        self.q.from.as_ref().map(|f| f.sources[0].alias.clone()).unwrap_or_else(|| "root".to_string())
    }
}

fn direct_aggregate(e: &Expr) -> Option<(AggregateKind, &Expr)> {
    match &e.kind {
        ExprKind::Aggregate { kind, arg } => Some((*kind, arg)),
        _ => None,
    }
}

/// `{"item": <partial aggregate>}` entry for an expression of the select list.
fn partial_item(c: &Ctx, e: &Expr) -> String {
    match direct_aggregate(e) {
        Some((AggregateKind::Average, arg)) => {
            let a = c.etext(arg);
            format!("{{\"item\": {{\"sum\": SUM({a}), \"count\": COUNT({a})}}}}")
        }
        Some((kind @ (AggregateKind::Min | AggregateKind::Max), arg)) if c.opts.composite_min_max => {
            let a = c.etext(arg);
            let (name, f) = if kind == AggregateKind::Min { ("min", "MIN") } else { ("max", "MAX") };
            format!("{{\"item\": {{\"{name}\": {f}({a}), \"count\": COUNT({a})}}}}")
        }
        _ => format!("{{\"item\": {}}}", c.etext(e)),
    }
}

/// Group-by payload entry: aggregates are wrapped as `{"item": ...}`, group keys stay plain.
fn group_item(c: &Ctx, e: &Expr) -> String {
    if direct_aggregate(e).is_some() {
        partial_item(c, e)
    } else {
        c.etext(e)
    }
}

/// Output names of the select items (alias, inferred name, or `$n`).
fn item_names(items: &[SelectItem]) -> Vec<String> {
    let mut unnamed = 0;
    items
        .iter()
        .map(|i| {
            i.alias.clone().or_else(|| inferred_name(&i.expr)).unwrap_or_else(|| {
                unnamed += 1;
                format!("${unnamed}")
            })
        })
        .collect()
}

fn json_str(s: &str) -> String {
    Value::from(s).to_string()
}

/// The select clause as one expression text (the "payload").
fn payload(c: &Ctx, grouped: bool) -> String {
    match &c.q.select.spec {
        SelectSpec::Star => c.first_alias(),
        SelectSpec::Value(e) => {
            if !grouped {
                c.etext(e)
            } else if direct_aggregate(e).is_some() {
                // `SELECT VALUE COUNT(1) ... GROUP BY`: the aggregate travels in a one element array, as in the
                // no GROUP BY form. The JavaScript SDK reads `payload[0]`; the Java SDK takes the element out.
                format!("[{}]", partial_item(c, e))
            } else {
                c.etext(e)
            }
        }
        SelectSpec::Items(items) => {
            let names = item_names(items);
            let parts: Vec<String> = items
                .iter()
                .zip(names)
                .map(|(i, n)| {
                    let v = if grouped { group_item(c, &i.expr) } else { c.etext(&i.expr) };
                    format!("{}: {}", json_str(&n), v)
                })
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

fn rewrite(c: &Ctx, top: Option<u64>) -> String {
    let q = c.q;
    let from = q.from.as_ref().map(|f| format!("\n{}", c.text(f.span))).unwrap_or_default();
    let orig_where = q.where_clause.as_ref().map(|w| c.etext(w));
    let top_text = |c: &Ctx| -> String {
        if let Some(t) = &q.select.top {
            format!("TOP {} ", c.etext(t))
        } else if let Some(n) = top {
            format!("TOP {n} ")
        } else {
            String::new()
        }
    };

    if !q.group_by.is_empty() {
        let keys: Vec<String> = q.group_by.iter().map(|g| format!("{{\"item\": {}}}", c.etext(g))).collect();
        let mut s = format!("SELECT [{}] AS groupByItems, {} AS payload{}", keys.join(", "), payload(c, true), from);
        if let Some(w) = orig_where {
            s.push_str(&format!("\nWHERE {w}"));
        }
        let gs: Vec<String> = q.group_by.iter().map(|g| c.etext(g)).collect();
        s.push_str(&format!("\nGROUP BY {}", gs.join(", ")));
        return s;
    }
    if q.is_aggregate && q.order_by.is_empty() {
        if let (SelectSpec::Items(items), true) = (&q.select.spec, c.opts.non_value_aggregate) {
            // one group: every partition answers one object, `{"payload": {alias: {"item": partial}, ..}}`
            let parts: Vec<String> =
                items.iter().zip(item_names(items)).map(|(i, n)| format!("{}: {}", json_str(&n), group_item(c, &i.expr))).collect();
            let mut s = format!("SELECT VALUE {{\"payload\": {{{}}}}}{}", parts.join(", "), from);
            if let Some(w) = orig_where {
                s.push_str(&format!("\nWHERE {w}"));
            }
            return s;
        }
        let items: Vec<String> = match &q.select.spec {
            SelectSpec::Value(e) => vec![partial_item(c, e)],
            SelectSpec::Items(items) => items.iter().map(|i| partial_item(c, &i.expr)).collect(),
            SelectSpec::Star => vec![],
        };
        let mut s = format!("SELECT VALUE [{}]{}", items.join(", "), from);
        if let Some(w) = orig_where {
            s.push_str(&format!("\nWHERE {w}"));
        }
        return s;
    }
    if !q.order_by.is_empty() {
        let items: Vec<String> = q.order_by.iter().map(|o| format!("{{\"item\": {}}}", c.etext(&o.expr))).collect();
        let order: Vec<String> =
            q.order_by.iter().map(|o| format!("{} {}", c.etext(&o.expr), if o.descending { "DESC" } else { "ASC" })).collect();
        // DISTINCT with ORDER BY is "ordered": every SDK removes the repeats itself (next to the previous row). Without TOP and
        // OFFSET LIMIT the partitions can therefore send every row, with its `_rid`, which the Java SDK needs to build the
        // continuation token of a page (`OrderByContinuationToken`: "rid must not be null"). With TOP or OFFSET LIMIT the
        // partition must remove the repeats before it counts, so the DISTINCT form stays (and has no `_rid`).
        let head = if q.select.distinct && (q.select.top.is_some() || top.is_some()) {
            format!("SELECT DISTINCT {}VALUE {{\"orderByItems\": [{}], \"payload\": {}}}", top_text(c), items.join(", "), payload(c, false))
        } else {
            format!(
                "SELECT {}{}._rid, [{}] AS orderByItems, {} AS payload",
                top_text(c),
                c.first_alias(),
                items.join(", "),
                payload(c, false)
            )
        };
        let mut s = format!("{head}{from}\nWHERE ({ORDER_BY_FILTER_PLACEHOLDER})");
        if let Some(w) = orig_where {
            s.push_str(&format!(" AND ({w})"));
        }
        s.push_str(&format!("\nORDER BY {}", order.join(", ")));
        return s;
    }
    if q.offset_limit.is_some() {
        let mut s =
            format!("SELECT {}{}{}{}", if q.select.distinct { "DISTINCT " } else { "" }, top_text(c), c.text(q.select.items_span), from);
        if let Some(w) = orig_where {
            s.push_str(&format!("\nWHERE {w}"));
        }
        return s;
    }
    String::new()
}

pub(crate) fn info(q: &Query, source: &str, params: &Params, opts: PlanOptions) -> QueryInfo {
    let c = Ctx { q, src: source.chars().collect(), opts };
    let off = q.offset_limit.as_ref().and_then(|o| resolve_u64(&o.offset, params));
    let lim = q.offset_limit.as_ref().and_then(|o| resolve_u64(&o.limit, params));
    let top = q.select.top.as_ref().and_then(|t| resolve_u64(t, params));

    let distinct_type = if !q.select.distinct {
        DistinctType::None
    } else if q.order_by.is_empty() {
        DistinctType::Unordered
    } else {
        DistinctType::Ordered
    };

    let mut aggregates = Vec::new();
    let mut aliases = Vec::new();
    let mut alias_map = Vec::new();
    if q.group_by.is_empty() {
        if q.is_aggregate {
            match &q.select.spec {
                SelectSpec::Value(e) => aggregates.extend(direct_aggregate(e).map(|a| a.0)),
                SelectSpec::Items(items) => {
                    aggregates.extend(items.iter().filter_map(|i| direct_aggregate(&i.expr).map(|a| a.0)));
                    if opts.non_value_aggregate {
                        for (i, n) in items.iter().zip(item_names(items)) {
                            aliases.push(n.clone());
                            alias_map.push((n, direct_aggregate(&i.expr).map(|a| a.0)));
                        }
                    }
                }
                SelectSpec::Star => {}
            }
        }
    } else {
        match &q.select.spec {
            SelectSpec::Value(e) => {
                // the SDKs find the operator of `SELECT VALUE <aggregate> ... GROUP BY` here, not in the alias map
                aggregates.extend(direct_aggregate(e).map(|a| a.0));
                aliases.push("$1".to_string());
                alias_map.push(("$1".to_string(), direct_aggregate(e).map(|a| a.0)));
            }
            SelectSpec::Items(items) => {
                for (i, n) in items.iter().zip(item_names(items)) {
                    aliases.push(n.clone());
                    alias_map.push((n, direct_aggregate(&i.expr).map(|a| a.0)));
                }
            }
            SelectSpec::Star => {}
        }
    }

    // OFFSET LIMIT is applied by the SDK; partitions must return offset + limit rows.
    let rewrite_top = match (off, lim) {
        (Some(o), Some(l)) => Some(o.saturating_add(l)),
        _ => None,
    };
    QueryInfo {
        distinct_type,
        top,
        offset: off,
        limit: lim,
        order_by: q.order_by.iter().map(|o| if o.descending { SortOrder::Descending } else { SortOrder::Ascending }).collect(),
        order_by_expressions: q.order_by.iter().map(|o| c.etext(&o.expr)).collect(),
        group_by_expressions: q.group_by.iter().map(|g| c.etext(g)).collect(),
        group_by_aliases: aliases,
        aggregates,
        group_by_alias_to_aggregate_type: alias_map,
        has_select_value: matches!(q.select.spec, SelectSpec::Value(_)),
        has_non_streaming_order_by: false,
        rewritten_query: rewrite(&c, rewrite_top),
    }
}

fn path_of(e: &Expr, alias: &str) -> Option<Vec<String>> {
    match &e.kind {
        ExprKind::Ident(n) if n == alias => Some(vec![]),
        ExprKind::Property(b, n) => {
            let mut p = path_of(b, alias)?;
            p.push(n.clone());
            Some(p)
        }
        ExprKind::Index(b, i) => match &i.kind {
            ExprKind::Literal(Literal::Str(s)) => {
                let mut p = path_of(b, alias)?;
                p.push(s.clone());
                Some(p)
            }
            _ => None,
        },
        _ => None,
    }
}

fn const_value(e: &Expr, params: &Params) -> Option<Value> {
    match &e.kind {
        ExprKind::Literal(Literal::Int(i)) => Some(Value::from(*i)),
        ExprKind::Literal(Literal::Float(f)) => serde_json::Number::from_f64(*f).map(Value::Number),
        ExprKind::Literal(Literal::Str(s)) => Some(Value::from(s.as_str())),
        ExprKind::Literal(Literal::Bool(b)) => Some(Value::Bool(*b)),
        ExprKind::Literal(Literal::Null) => Some(Value::Null),
        ExprKind::Param(n) => params.get(n).filter(|v| !v.is_array() && !v.is_object()).cloned(),
        _ => None,
    }
}

fn conjuncts<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    match &e.kind {
        ExprKind::And(items) => items.iter().for_each(|i| conjuncts(i, out)),
        _ => out.push(e),
    }
}

pub(crate) fn partition_key_filter(q: &Query, pk_paths: &[String], params: &Params) -> Option<Vec<Value>> {
    if pk_paths.is_empty() {
        return None;
    }
    let from = q.from.as_ref()?;
    let first = &from.sources[0];
    match &first.source {
        SourceExpr::Expr(Expr { kind: ExprKind::Ident(_), .. }) if !first.iterate => {}
        _ => return None,
    }
    let alias = first.alias.as_str();
    let mut found: Vec<(Vec<String>, Value)> = Vec::new();
    let mut cs = Vec::new();
    conjuncts(q.where_clause.as_ref()?, &mut cs);
    for c in cs {
        if let ExprKind::Binary(BinaryOp::Eq, a, b) = &c.kind {
            let pair = match (path_of(a, alias), const_value(b, params)) {
                (Some(p), Some(v)) => Some((p, v)),
                _ => match (path_of(b, alias), const_value(a, params)) {
                    (Some(p), Some(v)) => Some((p, v)),
                    _ => None,
                },
            };
            if let Some((p, v)) = pair {
                if let Some((_, old)) = found.iter().find(|(fp, _)| *fp == p) {
                    if *old != v {
                        return None;
                    }
                } else {
                    found.push((p, v));
                }
            }
        }
    }
    let mut out = Vec::with_capacity(pk_paths.len());
    for pk in pk_paths {
        let segs: Vec<String> = pk.split('/').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
        let (_, v) = found.iter().find(|(p, _)| *p == segs)?;
        out.push(v.clone());
    }
    Some(out)
}

/// Collects the names of the user-defined functions a query calls, including those in subqueries.
pub(crate) fn udf_names(q: &Query, out: &mut Vec<String>) {
    fn expr(e: &Expr, out: &mut Vec<String>) {
        match &e.kind {
            ExprKind::Udf { name, .. } => {
                if !out.contains(name) {
                    out.push(name.clone());
                }
            }
            ExprKind::Subquery(q) | ExprKind::Exists(q) | ExprKind::ArraySubquery(q) => udf_names(q, out),
            _ => {}
        }
        for c in e.children() {
            expr(c, out);
        }
    }
    for e in q.exprs() {
        expr(e, out);
    }
    if let Some(f) = &q.from {
        for s in &f.sources {
            if let SourceExpr::Subquery(sq) = &s.source {
                udf_names(sq, out);
            }
        }
    }
}
