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

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stmt {
    Set { target: PropRef, expr: Expr },
    Remove(PropRef),
}
