//! Static checks run after parsing: identifiers resolve, aggregates are placed legally, `SELECT *` has a single
//! source, select lists agree with GROUP BY, a scalar subquery cannot return several rows, ORDER BY names a path.
//!
//! The codes and texts are the ones a real account answers (checked in November 2026).

use crate::ast::*;
use crate::error::SqlError;

pub(crate) fn validate(q: &Query, source: &str) -> Result<(), SqlError> {
    let src: Vec<char> = source.chars().collect();
    validate_query(q, &[], true, &src, false)
}

/// ORDER BY, TOP and OFFSET LIMIT are refused in a subquery (a scalar one, `ARRAY`, `EXISTS`, or in FROM and JOIN).
fn nested_clause_errors(q: &Query) -> Vec<(&'static str, &'static str, Span)> {
    let mut out = Vec::new();
    if let Some(sp) = q.order_span {
        out.push(("SC2202", "'ORDER BY' is not supported in subqueries.", sp));
    }
    if let Some(sp) = q.select.top_span {
        out.push(("SC2203", "'TOP' is not supported in subqueries.", sp));
    }
    if let Some(ol) = &q.offset_limit {
        out.push(("SC2204", "'OFFSET LIMIT' clause is not supported in subqueries.", ol.span));
    }
    out
}

fn nested_error(q: &Query) -> Option<SqlError> {
    let mut it = nested_clause_errors(q).into_iter();
    let (code, msg, sp) = it.next()?;
    let mut err = SqlError::at(code, msg.to_string(), sp.start, sp.end);
    for (code, msg, sp) in it {
        err = err.and(code, msg.to_string(), sp.start, sp.end);
    }
    Some(err)
}

fn find_aggregate(e: &Expr) -> Option<&Expr> {
    if matches!(e.kind, ExprKind::Aggregate { .. }) {
        return Some(e);
    }
    e.children().into_iter().find_map(find_aggregate)
}

fn no_aggregate_in_where(e: &Expr) -> Result<(), SqlError> {
    match find_aggregate(e) {
        Some(a) => Err(SqlError::at("SC2104", "Aggregates are not allowed in the WHERE clause.".to_string(), a.span.start, a.span.end)),
        None => Ok(()),
    }
}

fn text(src: &[char], s: Span) -> String {
    src[s.start.min(src.len())..s.end.min(src.len())].iter().collect()
}

/// The text of an expression without its white space: two expressions are the same when these are.
fn squash(src: &[char], e: &Expr) -> String {
    text(src, e.span).chars().filter(|c| !c.is_whitespace()).collect()
}

fn validate_query(q: &Query, outer: &[String], container_scoped: bool, src: &[char], nested: bool) -> Result<(), SqlError> {
    if nested {
        if let Some(e) = nested_error(q) {
            return Err(e);
        }
    }
    if q.select.top.is_some() && q.offset_limit.is_some() {
        return Err(SqlError::at(
            "SC2205",
            "A TOP cannot be used in the same query or subquery as an OFFSET.".to_string(),
            q.span.start,
            q.span.end,
        ));
    }
    let n_sources = q.from.as_ref().map(|f| f.sources.len()).unwrap_or(0);
    let star = matches!(q.select.spec, SelectSpec::Star);
    if star && q.from.is_none() {
        return Err(SqlError::syntax(
            "Syntax error, 'SELECT *' is not valid if FROM clause is omitted.",
            q.select.items_span.start,
            q.select.items_span.end,
        )
        .with_code("SC1021"));
    }

    let mut visible: Vec<String> = outer.to_vec();
    let mut own: Vec<&str> = Vec::new();
    if let Some(f) = &q.from {
        for (i, s) in f.sources.iter().enumerate() {
            match &s.source {
                SourceExpr::Expr(e) => check_path(e, &visible, i == 0 && container_scoped)?,
                SourceExpr::Subquery(sq) => validate_query(sq, &visible, container_scoped && i == 0, src, true)?,
            }
            if own.contains(&s.alias.as_str()) {
                let sp = s.alias_span.unwrap_or(s.span);
                let mut err = SqlError::at(
                    "SC2021",
                    format!("The input set name or alias '{}' is specified more than once in the FROM clause.", s.alias),
                    sp.start,
                    sp.end,
                );
                if star {
                    err = err.and(
                        "SC2042",
                        "'SELECT *' is only valid with a single input set.".to_string(),
                        q.select.items_span.start,
                        q.select.items_span.end,
                    );
                }
                return Err(err);
            }
            own.push(&s.alias);
            visible.push(s.alias.clone());
        }
    }
    if star {
        if n_sources != 1 {
            return Err(SqlError::at(
                "SC2042",
                "'SELECT *' is only valid with a single input set.".to_string(),
                q.select.items_span.start,
                q.select.items_span.end,
            ));
        }
        if !q.group_by.is_empty() {
            return Err(SqlError::at(
                "SC2041",
                "'SELECT *' is not allowed with GROUP BY.".to_string(),
                q.select.items_span.start,
                q.select.items_span.end,
            ));
        }
    }

    if let Some(w) = &q.where_clause {
        no_aggregate_in_where(w)?;
        check_expr(w, &visible, src)?;
    }
    for g in &q.group_by {
        check_group_expr(g, &visible, &own, src)?;
    }
    match &q.select.spec {
        SelectSpec::Star => {}
        SelectSpec::Value(e) => check_expr(e, &visible, src)?,
        SelectSpec::Items(items) => {
            for i in items {
                check_expr(&i.expr, &visible, src)?;
            }
        }
    }
    for o in &q.order_by {
        check_expr(&o.expr, &visible, src)?;
        check_order_item(o, q)?;
    }
    if q.is_aggregate {
        check_select_refs(q, &own, src)?;
        if !q.group_by.is_empty() {
            check_order_refs(q, &own, src)?;
        }
    }
    Ok(())
}

/// One GROUP BY expression: no aggregate or subquery in it, and at least one property reference.
fn check_group_expr(g: &Expr, visible: &[String], own: &[&str], src: &[char]) -> Result<(), SqlError> {
    if let Some(a) = find_aggregate(g).or_else(|| find_subquery(g)) {
        return Err(SqlError::at(
            "SC2111",
            "Cannot use an aggregate or a subquery in an expression used for the group by list of a GROUP BY clause.".to_string(),
            a.span.start,
            a.span.end,
        )
        .and(
            "SC2110",
            "Each GROUP BY expression must contain at least one property reference.".to_string(),
            a.span.start,
            a.span.end,
        ));
    }
    if let Err(e) = check_expr(g, visible, src) {
        return Err(e.and(
            "SC2110",
            "Each GROUP BY expression must contain at least one property reference.".to_string(),
            g.span.start,
            g.span.end,
        ));
    }
    if !references_alias(g, own) {
        return Err(SqlError::at(
            "SC2110",
            "Each GROUP BY expression must contain at least one property reference.".to_string(),
            g.span.start,
            g.span.end,
        ));
    }
    Ok(())
}

fn find_subquery(e: &Expr) -> Option<&Expr> {
    if matches!(e.kind, ExprKind::Subquery(_) | ExprKind::Exists(_) | ExprKind::ArraySubquery(_)) {
        return Some(e);
    }
    e.children().into_iter().find_map(find_subquery)
}

fn references_alias(e: &Expr, own: &[&str]) -> bool {
    match &e.kind {
        ExprKind::Ident(n) => own.contains(&n.as_str()),
        _ => e.children().into_iter().any(|c| references_alias(c, own)),
    }
}

/// ORDER BY takes a path of the first source: `c.a`, `c["a"]`, `c.a[0]`. A function, an operator, a constant or an
/// aggregate is `2206`; a path that starts at a JOIN alias is refused too.
fn check_order_item(o: &OrderItem, q: &Query) -> Result<(), SqlError> {
    fn root(e: &Expr) -> Option<&str> {
        match &e.kind {
            ExprKind::Ident(n) => Some(n),
            ExprKind::Property(b, _) | ExprKind::Index(b, _) => root(b),
            _ => None,
        }
    }
    match root(&o.expr) {
        None => Err(SqlError::coded(
            2206,
            "Unsupported ORDER BY clause. ORDER BY item expression could not be mapped to a document path.",
            None,
        )),
        Some(r) => {
            let first = q.from.as_ref().map(|f| f.sources[0].alias.as_str());
            let own_alias = q.from.as_ref().map(|f| f.sources.iter().any(|s| s.alias == r)).unwrap_or(false);
            if own_alias && Some(r) != first {
                return Err(SqlError::gateway("Order-by over correlated collections is not supported."));
            }
            Ok(())
        }
    }
}

/// In a query with aggregates or GROUP BY, every property reference of the select list is inside an aggregate or is
/// (or starts with) a GROUP BY expression. The select list of a subquery counts as well.
fn check_select_refs(q: &Query, own: &[&str], src: &[char]) -> Result<(), SqlError> {
    let groups: Vec<String> = q.group_by.iter().map(|g| squash(src, g)).collect();
    match &q.select.spec {
        SelectSpec::Star => Ok(()),
        SelectSpec::Value(e) => walk_refs(e, own, &groups, src, "the select list", "SC2102"),
        SelectSpec::Items(items) => items.iter().try_for_each(|i| walk_refs(&i.expr, own, &groups, src, "the select list", "SC2102")),
    }
}

/// The same for ORDER BY of such a query.
fn check_order_refs(q: &Query, own: &[&str], src: &[char]) -> Result<(), SqlError> {
    let groups: Vec<String> = q.group_by.iter().map(|g| squash(src, g)).collect();
    q.order_by.iter().try_for_each(|o| walk_refs(&o.expr, own, &groups, src, "the ORDER BY clause", "SC2103"))
}

fn walk_refs(e: &Expr, own: &[&str], groups: &[String], src: &[char], place: &str, code: &str) -> Result<(), SqlError> {
    let me = squash(src, e);
    if groups.contains(&me) {
        return Ok(());
    }
    match &e.kind {
        ExprKind::Aggregate { .. } => Ok(()),
        ExprKind::Subquery(q) | ExprKind::Exists(q) | ExprKind::ArraySubquery(q) => {
            // a name the subquery declares hides the outer one
            let inner: Vec<&str> = q.from.iter().flat_map(|f| f.sources.iter().map(|s| s.alias.as_str())).collect();
            let outer: Vec<&str> = own.iter().copied().filter(|a| !inner.contains(a)).collect();
            match &q.select.spec {
                SelectSpec::Star => Ok(()),
                SelectSpec::Value(v) => walk_refs(v, &outer, groups, src, place, code),
                SelectSpec::Items(items) => items.iter().try_for_each(|i| walk_refs(&i.expr, &outer, groups, src, place, code)),
            }
        }
        ExprKind::Ident(_) | ExprKind::Property(..) | ExprKind::Index(..) => {
            if !references_alias(e, own) {
                return Ok(());
            }
            // a path below a GROUP BY path is covered by it
            let covered =
                groups.iter().any(|g| me.starts_with(g.as_str()) && matches!(me[g.len()..].chars().next(), Some('.') | Some('[')));
            if covered {
                return Ok(());
            }
            Err(SqlError::at(
                code,
                format!(
                    "Property reference '{}' is invalid in {place} because it is not contained in either an aggregate function or the GROUP BY clause.",
                    text(src, e.span)
                ),
                e.span.start,
                e.span.end,
            ))
        }
        _ => e.children().into_iter().try_for_each(|c| walk_refs(c, own, groups, src, place, code)),
    }
}

/// True if the query can return at most one row, whatever the data: no FROM, an aggregate without GROUP BY, or sources
/// that are single values (`FROM c.o x`, not `x IN c.arr`).
fn at_most_one_row(q: &Query) -> bool {
    if !q.group_by.is_empty() {
        return false;
    }
    if q.is_aggregate {
        return true;
    }
    match &q.from {
        None => true,
        Some(f) => f.sources.iter().all(|s| {
            !s.iterate
                && match &s.source {
                    SourceExpr::Expr(_) => true,
                    SourceExpr::Subquery(sq) => at_most_one_row(sq),
                }
        }),
    }
}

/// A path in FROM/JOIN. The head of the first source in a container-scoped query names the container.
fn check_path(e: &Expr, visible: &[String], head_exempt: bool) -> Result<(), SqlError> {
    match &e.kind {
        ExprKind::Ident(_) if head_exempt => Ok(()),
        ExprKind::Property(b, _) => check_path(b, visible, head_exempt),
        ExprKind::Index(b, _) => check_path(b, visible, head_exempt),
        _ => check_expr(e, visible, &[]),
    }
}

fn check_expr(e: &Expr, visible: &[String], src: &[char]) -> Result<(), SqlError> {
    match &e.kind {
        ExprKind::Ident(n) => {
            if !visible.iter().any(|v| v == n) {
                return Err(SqlError::semantic("SC2001", format!("Identifier '{n}' could not be resolved."), e.span.start, e.span.end));
            }
            Ok(())
        }
        ExprKind::Aggregate { arg, .. } => {
            if find_aggregate(arg).is_some() {
                return Err(SqlError::at(
                    "SC2101",
                    "Cannot perform an aggregate function on an expression containing an aggregate or a subquery.".to_string(),
                    e.span.start,
                    e.span.end,
                ));
            }
            check_expr(arg, visible, src)
        }
        ExprKind::Subquery(q) => {
            let msg = "The cardinality of a scalar subquery result set cannot be greater than one.".to_string();
            if let Some(mut first) = nested_error(q) {
                if !at_most_one_row(q) {
                    first = first.and("SC2201", msg, q.span.start, q.span.end);
                }
                return Err(first);
            }
            validate_query(q, visible, false, src, false)?;
            if !at_most_one_row(q) {
                return Err(SqlError::at("SC2201", msg, q.span.start, q.span.end));
            }
            Ok(())
        }
        ExprKind::Exists(q) | ExprKind::ArraySubquery(q) => validate_query(q, visible, false, src, true),
        _ => {
            for c in e.children() {
                check_expr(c, visible, src)?;
            }
            Ok(())
        }
    }
}
