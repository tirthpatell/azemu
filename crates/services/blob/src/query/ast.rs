//! The syntax tree of the query language.

use super::datetime::Part;
use super::value::{CastType, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// `name` (unquoted names match case-insensitively) or `"Name"` (exact).
    Field { name: String, quoted: bool },
    /// `_3` on a delimited row.
    Ordinal(usize),
    /// `[2]` on a JSON array.
    Index(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuncId {
    CharLength,
    Lower,
    Upper,
    Substring,
    DateAdd,
    DateDiff,
    ToString,
    ToTimestamp,
    UtcNow,
    Coalesce,
    NullIf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFn {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimMode {
    Both,
    Leading,
    Trailing,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Lit(Value),
    Ref(Vec<Step>),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    IsNull {
        e: Box<Expr>,
        neg: bool,
    },
    IsMissing {
        e: Box<Expr>,
        neg: bool,
    },
    Between {
        e: Box<Expr>,
        lo: Box<Expr>,
        hi: Box<Expr>,
        neg: bool,
    },
    In {
        e: Box<Expr>,
        list: Vec<Expr>,
        neg: bool,
    },
    Like {
        e: Box<Expr>,
        pat: Box<Expr>,
        esc: Option<Box<Expr>>,
        neg: bool,
    },
    Cast(Box<Expr>, CastType),
    Func(FuncId, Option<Part>, Vec<Expr>),
    Extract(Part, Box<Expr>),
    Trim {
        mode: TrimMode,
        chars: Option<Box<Expr>>,
        e: Box<Expr>,
    },
    /// `COUNT(*)` has no argument.
    Agg(AggFn, Option<Box<Expr>>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum FromStep {
    Field(String),
    /// `[*]`: every element of an array (an object or scalar stands for itself).
    All,
    Index(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub expr: Expr,
    pub alias: Option<String>,
    /// The text of the expression, for naming output columns.
    pub source: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Projection {
    Star,
    List(Vec<Item>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub projection: Projection,
    pub from: Vec<FromStep>,
    pub filter: Option<Expr>,
    pub limit: Option<u64>,
    /// Every select item is an aggregate: one row comes out at the end.
    pub aggregate: bool,
}
