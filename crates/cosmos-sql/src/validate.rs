//! Static checks run after parsing: identifiers resolve, aggregates are placed legally,
//! `SELECT *` has a single source.

use crate::ast::*;
use crate::error::SqlError;

pub(crate) fn validate(q: &Query) -> Result<(), SqlError> {
    validate_query(q, &[], true)
}

fn find_aggregate(e: &Expr) -> Option<&Expr> {
    if matches!(e.kind, ExprKind::Aggregate { .. }) {
        return Some(e);
    }
    e.children().into_iter().find_map(find_aggregate)
}

fn no_aggregate(e: &Expr, place: &str) -> Result<(), SqlError> {
    match find_aggregate(e) {
        Some(a) => Err(SqlError::semantic("SC2102", format!("Aggregate functions are not allowed in {place}."), a.span.start, a.span.end)),
        None => Ok(()),
    }
}

fn validate_query(q: &Query, outer: &[String], container_scoped: bool) -> Result<(), SqlError> {
    if q.select.top.is_some() && q.offset_limit.is_some() {
        return Err(SqlError::syntax("TOP and OFFSET LIMIT cannot be used together.", q.span.start, q.span.end));
    }
    if matches!(q.select.spec, SelectSpec::Star) {
        let n = q.from.as_ref().map(|f| f.sources.len()).unwrap_or(0);
        if n != 1 {
            return Err(SqlError::semantic(
                "SC2103",
                "'SELECT *' is only valid with a single input set.".to_string(),
                q.select.items_span.start,
                q.select.items_span.end,
            ));
        }
        if q.is_aggregate {
            return Err(SqlError::semantic(
                "SC2103",
                "'SELECT *' is not valid with GROUP BY or aggregates.".to_string(),
                q.select.items_span.start,
                q.select.items_span.end,
            ));
        }
    }

    let mut visible: Vec<String> = outer.to_vec();
    let mut own: Vec<&str> = Vec::new();
    if let Some(f) = &q.from {
        for (i, s) in f.sources.iter().enumerate() {
            match &s.source {
                SourceExpr::Expr(e) => {
                    no_aggregate(e, "FROM")?;
                    check_path(e, &visible, i == 0 && container_scoped)?;
                }
                SourceExpr::Subquery(sq) => validate_query(sq, &visible, container_scoped && i == 0)?,
            }
            if own.contains(&s.alias.as_str()) {
                return Err(SqlError::semantic(
                    "SC2002",
                    format!("The alias '{}' is used more than once in the FROM clause.", s.alias),
                    s.span.start,
                    s.span.end,
                ));
            }
            own.push(&s.alias);
            visible.push(s.alias.clone());
        }
    }

    if let Some(w) = &q.where_clause {
        no_aggregate(w, "the WHERE clause")?;
        check_expr(w, &visible)?;
    }
    for g in &q.group_by {
        no_aggregate(g, "the GROUP BY clause")?;
        check_expr(g, &visible)?;
    }
    if let Some(t) = &q.select.top {
        no_aggregate(t, "TOP")?;
        check_expr(t, &visible)?;
    }
    if let Some(ol) = &q.offset_limit {
        no_aggregate(&ol.offset, "OFFSET")?;
        no_aggregate(&ol.limit, "LIMIT")?;
        check_expr(&ol.offset, &visible)?;
        check_expr(&ol.limit, &visible)?;
    }
    match &q.select.spec {
        SelectSpec::Star => {}
        SelectSpec::Value(e) => check_expr(e, &visible)?,
        SelectSpec::Items(items) => {
            for i in items {
                check_expr(&i.expr, &visible)?;
            }
        }
    }
    for o in &q.order_by {
        check_expr(&o.expr, &visible)?;
    }
    Ok(())
}

/// A FROM/JOIN path. The head of the first source in a container-scoped query names the container.
fn check_path(e: &Expr, visible: &[String], head_exempt: bool) -> Result<(), SqlError> {
    match &e.kind {
        ExprKind::Ident(_) if head_exempt => Ok(()),
        ExprKind::Property(b, _) => check_path(b, visible, head_exempt),
        ExprKind::Index(b, i) => {
            check_path(b, visible, head_exempt)?;
            check_expr(i, visible)
        }
        _ => check_expr(e, visible),
    }
}

fn check_expr(e: &Expr, visible: &[String]) -> Result<(), SqlError> {
    match &e.kind {
        ExprKind::Ident(n) => {
            if !visible.iter().any(|v| v == n) {
                return Err(SqlError::semantic("SC2001", format!("Identifier '{n}' could not be resolved."), e.span.start, e.span.end));
            }
            Ok(())
        }
        ExprKind::Aggregate { arg, .. } => {
            if let Some(a) = find_aggregate(arg) {
                return Err(SqlError::semantic("SC2104", "Aggregate functions cannot be nested.".to_string(), a.span.start, a.span.end));
            }
            check_expr(arg, visible)
        }
        ExprKind::Subquery(q) | ExprKind::Exists(q) | ExprKind::ArraySubquery(q) => validate_query(q, visible, false),
        _ => {
            for c in e.children() {
                check_expr(c, visible)?;
            }
            Ok(())
        }
    }
}
