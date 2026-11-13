//! Query pipeline: FROM/JOIN, WHERE, GROUP BY / aggregates, projection, ORDER BY,
//! DISTINCT, OFFSET/LIMIT, TOP.
//!
//! The rows of FROM / JOIN are produced one at a time and handed straight to WHERE and then to the projection (or
//! the group), so the product of a JOIN is never held in memory: only what the query keeps is, and that is
//! counted against a byte budget (`Ctx::charge_bytes`). Documents of the container are not converted either: a
//! row holds the shared document (`Val::Root`) and property paths read the JSON directly. A query without ORDER BY,
//! GROUP BY or aggregates that has TOP / LIMIT stops as soon as it has enough rows.

use super::{Ctx, Env, GroupCtx};
use crate::aggregate::Acc;
use crate::ast::{Expr, ExprKind, FromSource, Literal, Query, SelectSpec, SourceExpr};
use crate::error::SqlError;
use crate::value::{canon_key, cmp_total, json_index, Val};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// The documents a query runs over: shared with the container, or borrowed from the caller (copied when a
/// row needs them).
pub(crate) enum DocSet<'a> {
    Shared(&'a [Arc<Value>]),
    Borrowed(Vec<&'a Value>),
}

impl DocSet<'_> {
    fn len(&self) -> usize {
        match self {
            DocSet::Shared(s) => s.len(),
            DocSet::Borrowed(b) => b.len(),
        }
    }

    fn get(&self, i: usize) -> Arc<Value> {
        match self {
            DocSet::Shared(s) => s[i].clone(),
            DocSet::Borrowed(b) => Arc::new(b[i].clone()),
        }
    }
}

/// Where a (sub)query gets its data from.
#[derive(Clone, Copy)]
pub(crate) enum Scope<'a> {
    /// The documents of the container: the first FROM source starts at the container.
    Container(&'a DocSet<'a>),
    /// A correlated subquery: names resolve in the enclosing scope.
    Correlated(&'a Env<'a>),
}

type Row = Vec<Val>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

type Sink<'s> = dyn FnMut(&Row) -> Result<Flow, SqlError> + 's;

/// Runs a query. `hint` is how many rows the caller will look at at most (EXISTS and a scalar subquery
/// need one), so the query can stop early.
pub(crate) fn run(ctx: &Ctx, q: &Query, scope: Scope, hint: Option<usize>) -> Result<Vec<Val>, SqlError> {
    let correlated = matches!(scope, Scope::Correlated(_));
    let held_before = ctx.bytes_used();
    let out = run_inner(ctx, q, scope, hint);
    if correlated {
        // the result goes to the expression that asked; what it keeps, it counts itself
        ctx.reset_bytes(held_before);
    }
    out
}

fn run_inner(ctx: &Ctx, q: &Query, scope: Scope, hint: Option<usize>) -> Result<Vec<Val>, SqlError> {
    let parent: Option<&Env> = match &scope {
        Scope::Correlated(e) => Some(e),
        Scope::Container(_) => None,
    };
    let aliases: Vec<String> = q.from.as_ref().map(|f| f.sources.iter().map(|s| s.alias.clone()).collect()).unwrap_or_default();
    let penv = Env { parent, aliases: &[], vals: &[] };
    let top = match &q.select.top {
        Some(t) => Some(count_value(ctx, t, &penv, "TOP")?),
        None => None,
    };
    let window = match &q.offset_limit {
        Some(ol) => Some((count_value(ctx, &ol.offset, &penv, "OFFSET")?, count_value(ctx, &ol.limit, &penv, "LIMIT")?)),
        None => None,
    };
    // how many rows of the sorted / arrival order anything after this stage can use
    let mut need = match (top, window) {
        (t, Some((o, l))) => Some(o.saturating_add(l.min(t.unwrap_or(usize::MAX)))),
        (Some(t), None) => Some(t),
        (None, None) => None,
    };
    if let Some(h) = hint {
        need = Some(need.map_or(h, |n| n.min(h)));
    }
    let arrival_order = !q.is_aggregate && q.order_by.is_empty();
    if arrival_order && need == Some(0) {
        return Ok(Vec::new());
    }
    let keep_root = parent.is_none();
    let aggs = if q.is_aggregate { collect_aggregates(q) } else { Vec::new() };

    let mut collect = Collect {
        q,
        entries: Vec::new(),
        seen: HashSet::new(),
        held: 0,
        // DISTINCT without ORDER BY is decided as rows arrive; with ORDER BY it is applied to the sorted rows
        inline_distinct: q.select.distinct && q.order_by.is_empty(),
        prune_at: if !q.is_aggregate && !q.order_by.is_empty() && !q.select.distinct {
            need.map(|n| n.saturating_mul(2).max(1024))
        } else {
            None
        },
        need,
        stop_at: if arrival_order { need } else { None },
        keep_root,
    };
    let mut groups: Vec<Group> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();

    {
        let mut sink = |row: &Row| -> Result<Flow, SqlError> {
            let env = Env { parent, aliases: &aliases, vals: row };
            if let Some(w) = &q.where_clause {
                if !matches!(ctx.eval(w, &env, None)?, Val::Bool(true)) {
                    return Ok(Flow::Continue);
                }
            }
            if !q.is_aggregate {
                return collect.push(ctx, &env, None);
            }
            let mut key = String::new();
            for g in &q.group_by {
                key.push_str(&canon_key(&ctx.eval(g, &env, None)?));
                key.push('|');
            }
            let gi = match index.get(&key) {
                Some(&i) => i,
                None => {
                    let first = row.clone();
                    let held: usize = first.iter().map(|v| v.est_bytes(ctx.bytes_left())).sum();
                    ctx.charge_bytes(key.len() + held + aggs.len() * 96 + 160)?;
                    index.insert(key, groups.len());
                    groups.push(Group { first, accs: aggs.iter().map(|a| new_acc(a)).collect() });
                    groups.len() - 1
                }
            };
            for (i, a) in aggs.iter().enumerate() {
                if let ExprKind::Aggregate { arg, .. } = &a.kind {
                    let v = ctx.eval(arg, &env, None)?;
                    let extra = groups[gi].accs[i].push(&v);
                    if extra > 0 {
                        ctx.charge_bytes(extra)?;
                    }
                }
            }
            Ok(Flow::Continue)
        };
        stream(ctx, q, scope, &aliases, parent, &mut sink)?;
    }

    if q.is_aggregate {
        if q.group_by.is_empty() && groups.is_empty() {
            groups.push(Group { first: vec![Val::Undefined; aliases.len()], accs: aggs.iter().map(|a| new_acc(a)).collect() });
        }
        for g in &groups {
            let mut results: HashMap<usize, Val> = HashMap::with_capacity(aggs.len());
            for (a, acc) in aggs.iter().zip(&g.accs) {
                results.insert(*a as *const Expr as usize, acc.finish());
            }
            let env = Env { parent, aliases: &aliases, vals: &g.first };
            collect.push(ctx, &env, Some(&GroupCtx { results: &results }))?;
        }
    }

    let mut entries = collect.entries;
    if !q.order_by.is_empty() {
        sort_entries(q, &mut entries);
    }
    let mut out: Vec<Val> = entries.into_iter().map(|(v, _)| v).collect();

    if q.select.distinct && !collect.inline_distinct {
        let mut seen = HashSet::new();
        let mut unique = Vec::with_capacity(out.len());
        for v in out {
            let k = canon_key(&v);
            if !seen.contains(&k) {
                ctx.charge_bytes(k.len() + 64)?;
                seen.insert(k);
                unique.push(v);
            }
        }
        out = unique;
    }

    if let Some((off, lim)) = window {
        out = out.into_iter().skip(off).take(lim).collect();
    }
    if let Some(t) = top {
        out.truncate(t);
    }
    if let Some(h) = hint {
        out.truncate(h);
    }
    Ok(out)
}

/// What the rows that pass WHERE (or the finished groups) turn into: the projection and the ORDER BY keys.
struct Collect<'q> {
    q: &'q Query,
    entries: Vec<(Val, Vec<Val>)>,
    /// DISTINCT without ORDER BY: the canonical text of the rows kept so far.
    seen: HashSet<String>,
    /// Bytes `entries` holds, as counted against the budget.
    held: usize,
    inline_distinct: bool,
    /// ORDER BY with a known row count (TOP, LIMIT): sort and cut the list when it reaches this length.
    prune_at: Option<usize>,
    need: Option<usize>,
    /// No ORDER BY, no aggregate: stop the FROM stage when this many rows are collected.
    stop_at: Option<usize>,
    keep_root: bool,
}

impl Collect<'_> {
    fn push(&mut self, ctx: &Ctx, env: &Env, grp: Option<&GroupCtx>) -> Result<Flow, SqlError> {
        let q = self.q;
        let proj = project(ctx, q, env, grp, self.keep_root)?;
        if proj.is_undefined() {
            return Ok(Flow::Continue);
        }
        if self.inline_distinct {
            let k = canon_key(&proj);
            if self.seen.contains(&k) {
                return Ok(Flow::Continue);
            }
            ctx.charge_bytes(k.len() + 64)?;
            self.seen.insert(k);
        }
        let mut keys = Vec::with_capacity(q.order_by.len());
        for item in &q.order_by {
            keys.push(ctx.eval(&item.expr, env, grp)?);
        }
        // Cosmos serves ORDER BY from the index; an item whose sort value is undefined is not in
        // the index of that path unless the path is indexed explicitly or a composite index is
        // used (Microsoft Learn, "Indexing policies"). The caller says which case it is.
        if ctx.order_by_excludes_undefined && keys.iter().any(|k| k.is_undefined()) {
            return Ok(Flow::Continue);
        }
        let bytes = entry_bytes(ctx, &proj, &keys);
        ctx.charge_bytes(bytes)?;
        self.held += bytes;
        self.entries.push((proj, keys));
        if let (Some(at), Some(n)) = (self.prune_at, self.need) {
            if self.entries.len() >= at {
                sort_entries(q, &mut self.entries);
                self.entries.truncate(n);
                let kept: usize = self.entries.iter().map(|(p, k)| entry_bytes(ctx, p, k)).sum();
                ctx.release_bytes(self.held.saturating_sub(kept));
                self.held = kept;
            }
        }
        if let Some(n) = self.stop_at {
            if self.entries.len() >= n {
                return Ok(Flow::Stop);
            }
        }
        Ok(Flow::Continue)
    }
}

fn entry_bytes(ctx: &Ctx, proj: &Val, keys: &[Val]) -> usize {
    let cap = ctx.bytes_left();
    let mut n = 64 + proj.est_bytes(cap);
    for k in keys {
        n += k.est_bytes(cap);
    }
    n
}

fn sort_entries(q: &Query, entries: &mut [(Val, Vec<Val>)]) {
    entries.sort_by(|a, b| {
        for (i, item) in q.order_by.iter().enumerate() {
            let o = cmp_total(&a.1[i], &b.1[i]);
            let o = if item.descending { o.reverse() } else { o };
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    });
}

/// One group: its first row (what non aggregate select items see) and its running aggregates.
struct Group {
    first: Row,
    accs: Vec<Acc>,
}

fn new_acc(a: &Expr) -> Acc {
    match &a.kind {
        ExprKind::Aggregate { kind, .. } => Acc::new(*kind),
        _ => Acc::new(crate::ast::AggregateKind::Count),
    }
}

/// The aggregate calls of this query level (not those of subqueries, which have their own groups).
fn collect_aggregates(q: &Query) -> Vec<&Expr> {
    fn walk<'q>(e: &'q Expr, out: &mut Vec<&'q Expr>) {
        match &e.kind {
            ExprKind::Aggregate { .. } => out.push(e),
            ExprKind::Property(b, _) | ExprKind::Unary(_, b) => walk(b, out),
            ExprKind::Index(a, b) | ExprKind::Binary(_, a, b) => {
                walk(a, out);
                walk(b, out);
            }
            ExprKind::And(v) | ExprKind::Or(v) | ExprKind::Array(v) => v.iter().for_each(|x| walk(x, out)),
            ExprKind::Between { expr, low, high, .. } => {
                walk(expr, out);
                walk(low, out);
                walk(high, out);
            }
            ExprKind::In { expr, list, .. } => {
                walk(expr, out);
                list.iter().for_each(|x| walk(x, out));
            }
            ExprKind::Like { expr, pattern, escape, .. } => {
                walk(expr, out);
                walk(pattern, out);
                if let Some(x) = escape {
                    walk(x, out);
                }
            }
            ExprKind::Ternary(a, b, c) => {
                walk(a, out);
                walk(b, out);
                walk(c, out);
            }
            ExprKind::Call { args, .. } | ExprKind::Udf { args, .. } => args.iter().for_each(|x| walk(x, out)),
            ExprKind::Object(pairs) => pairs.iter().for_each(|(_, x)| walk(x, out)),
            ExprKind::Literal(_)
            | ExprKind::Param(_)
            | ExprKind::Ident(_)
            | ExprKind::Subquery(_)
            | ExprKind::Exists(_)
            | ExprKind::ArraySubquery(_) => {}
        }
    }
    let mut out = Vec::new();
    match &q.select.spec {
        SelectSpec::Star => {}
        SelectSpec::Value(e) => walk(e, &mut out),
        SelectSpec::Items(items) => items.iter().for_each(|i| walk(&i.expr, &mut out)),
    }
    for o in &q.order_by {
        walk(&o.expr, &mut out);
    }
    out
}

fn count_value(ctx: &Ctx, e: &Expr, env: &Env, what: &str) -> Result<usize, SqlError> {
    match ctx.eval(e, env, None)?.as_i64() {
        Some(n) if n >= 0 => Ok(n as usize),
        _ => Err(SqlError::eval(format!("{what} must be a non-negative integer."))),
    }
}

/// Name of a select item without an alias: the last property of a path, else `None`.
pub(crate) fn inferred_name(e: &Expr) -> Option<String> {
    match &e.kind {
        ExprKind::Property(_, n) => Some(n.clone()),
        ExprKind::Index(_, i) => match &i.kind {
            ExprKind::Literal(Literal::Str(s)) => Some(s.clone()),
            _ => None,
        },
        ExprKind::Ident(n) => Some(n.clone()),
        _ => None,
    }
}

fn project(ctx: &Ctx, q: &Query, env: &Env, grp: Option<&GroupCtx>, keep_root: bool) -> Result<Val, SqlError> {
    match &q.select.spec {
        SelectSpec::Star => Ok(env.vals.first().cloned().unwrap_or(Val::Undefined)),
        SelectSpec::Value(e) => {
            // SELECT VALUE c on a shared document hands the document on instead of converting it
            if keep_root {
                if let ExprKind::Ident(n) = &e.kind {
                    if let Some(v @ Val::Root(_)) = env.lookup(n) {
                        return Ok(v.clone());
                    }
                }
            }
            ctx.eval(e, env, grp)
        }
        SelectSpec::Items(items) => {
            let mut pairs: Vec<(Arc<str>, Val)> = Vec::with_capacity(items.len());
            let mut unnamed = 0;
            let mut size = 0usize;
            for it in items {
                let v = ctx.eval(&it.expr, env, grp)?;
                size = size.saturating_add(v.est_bytes(ctx.bytes_left()));
                ctx.check_room(size)?;
                let name = match it.alias.clone().or_else(|| inferred_name(&it.expr)) {
                    Some(n) => n,
                    None => {
                        unnamed += 1;
                        format!("${unnamed}")
                    }
                };
                pairs.push((Arc::from(name.as_str()), v));
            }
            Ok(Val::object(pairs))
        }
    }
}

fn expand(values: Vec<Val>, iterate: bool) -> Vec<Val> {
    if iterate {
        let mut out = Vec::new();
        for v in values {
            if let Val::Arr(a) = v {
                out.extend(a.iter().cloned());
            }
        }
        out
    } else {
        values.into_iter().filter(|v| !v.is_undefined()).collect()
    }
}

/// A path from the container item (`c`, `c.a.b[0]`) read straight from the JSON of the document.
/// `None` when `e` is not such a path; `Some(None)` is undefined.
fn json_path<'v>(ctx: &Ctx, e: &Expr, doc: &'v Value) -> Result<Option<Option<&'v Value>>, SqlError> {
    match &e.kind {
        ExprKind::Ident(_) => Ok(Some(Some(doc))),
        ExprKind::Property(b, n) => Ok(json_path(ctx, b, doc)?.map(|x| x.and_then(|v| v.as_object()).and_then(|m| m.get(n.as_str())))),
        ExprKind::Index(b, i) => match json_path(ctx, b, doc)? {
            None => Ok(None),
            Some(x) => {
                let idx = ctx.eval(i, &Env::empty(), None)?;
                Ok(Some(x.and_then(|v| json_index(v, &idx))))
            }
        },
        _ => Ok(None),
    }
}

/// Evaluates a first-source path with the container item as its root.
fn eval_root_path(ctx: &Ctx, e: &Expr, root: &Val) -> Result<Val, SqlError> {
    if let ExprKind::Ident(_) = &e.kind {
        // `FROM c`: the row holds the shared document itself
        return Ok(root.clone());
    }
    if let Val::Root(doc) = root {
        if let Some(found) = json_path(ctx, e, doc)? {
            return Ok(found.map(Val::from_json).unwrap_or(Val::Undefined));
        }
    }
    ctx.eval(e, &Env::empty(), None)
}

/// Hands every row of FROM / JOIN to `sink`, one at a time.
fn stream(ctx: &Ctx, q: &Query, scope: Scope, aliases: &[String], parent: Option<&Env>, sink: &mut Sink) -> Result<(), SqlError> {
    let Some(from) = &q.from else {
        sink(&Vec::new())?;
        return Ok(());
    };
    let mut row: Row = Vec::with_capacity(from.sources.len());
    level(ctx, &from.sources, scope, aliases, parent, 0, &mut row, sink)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn level(
    ctx: &Ctx,
    sources: &[FromSource],
    scope: Scope,
    aliases: &[String],
    parent: Option<&Env>,
    lvl: usize,
    row: &mut Row,
    sink: &mut Sink,
) -> Result<Flow, SqlError> {
    if lvl == sources.len() {
        return sink(row);
    }
    let src = &sources[lvl];
    if lvl == 0 {
        return match (&src.source, scope) {
            (SourceExpr::Expr(e), Scope::Container(docs)) => {
                for i in 0..docs.len() {
                    let root = Val::Root(docs.get(i));
                    let v = eval_root_path(ctx, e, &root)?;
                    let values = expand(vec![v], src.iterate);
                    if descend(ctx, sources, scope, aliases, parent, lvl, row, sink, values)? == Flow::Stop {
                        return Ok(Flow::Stop);
                    }
                }
                Ok(Flow::Continue)
            }
            (SourceExpr::Expr(e), Scope::Correlated(env)) => {
                let v = ctx.eval(e, env, None)?;
                descend(ctx, sources, scope, aliases, parent, lvl, row, sink, expand(vec![v], src.iterate))
            }
            (SourceExpr::Subquery(sq), Scope::Container(docs)) => {
                let values = expand(run(ctx, sq, Scope::Container(docs), None)?, src.iterate);
                descend(ctx, sources, scope, aliases, parent, lvl, row, sink, values)
            }
            (SourceExpr::Subquery(sq), Scope::Correlated(env)) => {
                let values = expand(run(ctx, sq, Scope::Correlated(env), None)?, src.iterate);
                descend(ctx, sources, scope, aliases, parent, lvl, row, sink, values)
            }
        };
    }
    let values = {
        let env = Env { parent, aliases: &aliases[..lvl], vals: row };
        match &src.source {
            SourceExpr::Expr(e) => vec![ctx.eval(e, &env, None)?],
            SourceExpr::Subquery(sq) => run(ctx, sq, Scope::Correlated(&env), None)?,
        }
    };
    descend(ctx, sources, scope, aliases, parent, lvl, row, sink, expand(values, src.iterate))
}

/// Adds each value as the next column of the row and goes on with the next source.
#[allow(clippy::too_many_arguments)]
fn descend(
    ctx: &Ctx,
    sources: &[FromSource],
    scope: Scope,
    aliases: &[String],
    parent: Option<&Env>,
    lvl: usize,
    row: &mut Row,
    sink: &mut Sink,
    values: Vec<Val>,
) -> Result<Flow, SqlError> {
    for x in values {
        ctx.charge(1)?;
        row.push(x);
        let flow = level(ctx, sources, scope, aliases, parent, lvl + 1, row, sink)?;
        row.pop();
        if flow == Flow::Stop {
            return Ok(Flow::Stop);
        }
    }
    Ok(Flow::Continue)
}
