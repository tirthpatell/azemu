//! Syntax tree shared by filters and actions.

use crate::value::{Scope, Value};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PropRef {
    pub scope: Scope,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Lit(Value),
    Param(String),
    Prop(PropRef),
    /// `property(name)` / `p(name)`
    PropertyFn(Box<Expr>),
    NewId,
    Neg(Box<Expr>),
    Plus(Box<Expr>),
    Not(Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    In {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        escape: Option<Box<Expr>>,
        negated: bool,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    Exists(PropRef),
}

impl Expr {
    /// Every parameter (`@name`, without the `@`) and every property the expression reads, in order of appearance.
    pub(crate) fn collect(&self, params: &mut Vec<String>, props: &mut Vec<PropRef>) {
        match self {
            Expr::Lit(_) | Expr::NewId => {}
            Expr::Param(n) => params.push(n.clone()),
            Expr::Prop(p) | Expr::Exists(p) => props.push(p.clone()),
            Expr::PropertyFn(e) | Expr::Neg(e) | Expr::Plus(e) | Expr::Not(e) => e.collect(params, props),
            Expr::Binary(_, l, r) => {
                l.collect(params, props);
                r.collect(params, props);
            }
            Expr::In { expr, list, .. } => {
                expr.collect(params, props);
                for e in list {
                    e.collect(params, props);
                }
            }
            Expr::Like { expr, pattern, escape, .. } => {
                expr.collect(params, props);
                pattern.collect(params, props);
                if let Some(e) = escape {
                    e.collect(params, props);
                }
            }
            Expr::IsNull { expr, .. } => expr.collect(params, props),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stmt {
    Set { target: PropRef, expr: Expr },
    Remove(PropRef),
}
