//! Abstract syntax tree produced by [`crate::parse`].
//!
//! All `Span`s are character offsets into the original query text.

use crate::functions::FuncSpec;

/// Half-open character range in the query text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    /// First character.
    pub start: usize,
    /// One past the last character.
    pub end: usize,
}

/// Aggregate function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateKind {
    /// `COUNT`
    Count,
    /// `SUM`
    Sum,
    /// `AVG`
    Average,
    /// `MIN`
    Min,
    /// `MAX`
    Max,
}

impl AggregateKind {
    /// Name used by the query plan (`"Count"`, `"Sum"`, `"Average"`, `"Min"`, `"Max"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            AggregateKind::Count => "Count",
            AggregateKind::Sum => "Sum",
            AggregateKind::Average => "Average",
            AggregateKind::Min => "Min",
            AggregateKind::Max => "Max",
        }
    }
    /// SQL function name.
    pub fn sql_name(&self) -> &'static str {
        match self {
            AggregateKind::Count => "COUNT",
            AggregateKind::Sum => "SUM",
            AggregateKind::Average => "AVG",
            AggregateKind::Min => "MIN",
            AggregateKind::Max => "MAX",
        }
    }
}

/// A parsed query (also used for subqueries).
#[derive(Debug, Clone)]
pub struct Query {
    /// The SELECT clause.
    pub select: Select,
    /// The FROM clause (the first source plus the JOINs). `None` for `SELECT 1`.
    pub from: Option<From>,
    /// WHERE condition.
    pub where_clause: Option<Expr>,
    /// GROUP BY expressions.
    pub group_by: Vec<Expr>,
    /// ORDER BY items.
    pub order_by: Vec<OrderItem>,
    /// OFFSET ... LIMIT ...
    pub offset_limit: Option<OffsetLimit>,
    /// True when the query aggregates (an aggregate function in SELECT/ORDER BY, or GROUP BY).
    pub is_aggregate: bool,
    /// Whole query.
    pub span: Span,
}

/// SELECT clause.
#[derive(Debug, Clone)]
pub struct Select {
    /// `DISTINCT`
    pub distinct: bool,
    /// `TOP n` (a literal or a parameter).
    pub top: Option<Expr>,
    /// What is selected.
    pub spec: SelectSpec,
    /// Text of the select list (after DISTINCT/TOP).
    pub items_span: Span,
}

/// What a SELECT projects.
#[derive(Debug, Clone)]
pub enum SelectSpec {
    /// `SELECT *`
    Star,
    /// `SELECT VALUE expr`
    Value(Expr),
    /// `SELECT a, b AS x, ...`
    Items(Vec<SelectItem>),
}

/// One item of a select list.
#[derive(Debug, Clone)]
pub struct SelectItem {
    /// The expression.
    pub expr: Expr,
    /// Explicit alias (`AS x` or implicit).
    pub alias: Option<String>,
}

/// FROM clause.
#[derive(Debug, Clone)]
pub struct From {
    /// First source followed by the JOIN sources.
    pub sources: Vec<FromSource>,
    /// From the `FROM` keyword to the end of the last source.
    pub span: Span,
}

/// One source in FROM or JOIN.
#[derive(Debug, Clone)]
pub struct FromSource {
    /// Alias (explicit or inferred; generated for unnamed subqueries).
    pub alias: String,
    /// `alias IN expr`: iterate the array elements.
    pub iterate: bool,
    /// Where the values come from.
    pub source: SourceExpr,
    /// Whole source text.
    pub span: Span,
}

/// Source of a FROM/JOIN entry.
#[derive(Debug, Clone)]
pub enum SourceExpr {
    /// Path expression such as `c.children` (the first source starts at the container).
    Expr(Expr),
    /// `(SELECT ...)`
    Subquery(Box<Query>),
}

/// ORDER BY item.
#[derive(Debug, Clone)]
pub struct OrderItem {
    /// Sort expression.
    pub expr: Expr,
    /// `DESC`
    pub descending: bool,
}

/// OFFSET and LIMIT.
#[derive(Debug, Clone)]
pub struct OffsetLimit {
    /// `OFFSET n`
    pub offset: Expr,
    /// `LIMIT m`
    pub limit: Expr,
}

/// An expression with its source position.
#[derive(Debug, Clone)]
pub struct Expr {
    /// What the expression is.
    pub kind: ExprKind,
    /// Source text range.
    pub span: Span,
    /// Height of the expression tree (used for the nesting limit).
    pub depth: u16,
}

/// Literal constant.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// `undefined`
    Undefined,
    /// `null`
    Null,
    /// `true` / `false`
    Bool(bool),
    /// Integer
    Int(i64),
    /// Decimal / exponent number
    Float(f64),
    /// String
    Str(String),
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `NOT`
    Not,
    /// `-`
    Neg,
    /// `+`
    Plus,
    /// `~`
    BitNot,
}

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `=`
    Eq,
    /// `!=` / `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `&`
    BitAnd,
    /// `|`
    BitOr,
    /// `^`
    BitXor,
    /// `<<`
    Shl,
    /// `>>`
    Shr,
    /// `>>>`
    UShr,
    /// `||`
    Concat,
    /// `??`
    Coalesce,
}

/// Expression kinds.
#[derive(Debug, Clone)]
pub enum ExprKind {
    /// Constant.
    Literal(Literal),
    /// `@name` (stored without the `@`).
    Param(String),
    /// Alias reference.
    Ident(String),
    /// `base.name`
    Property(Box<Expr>, String),
    /// `base[index]`
    Index(Box<Expr>, Box<Expr>),
    /// Unary operation.
    Unary(UnaryOp, Box<Expr>),
    /// Binary operation.
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    /// `a AND b AND ...`
    And(Vec<Expr>),
    /// `a OR b OR ...`
    Or(Vec<Expr>),
    /// `x [NOT] BETWEEN low AND high`
    Between {
        /// Tested value.
        expr: Box<Expr>,
        /// Lower bound (inclusive).
        low: Box<Expr>,
        /// Upper bound (inclusive).
        high: Box<Expr>,
        /// `NOT BETWEEN`
        negated: bool,
    },
    /// `x [NOT] IN (a, b, ...)`
    In {
        /// Tested value.
        expr: Box<Expr>,
        /// Candidates.
        list: Vec<Expr>,
        /// `NOT IN`
        negated: bool,
    },
    /// `x [NOT] LIKE pattern [ESCAPE e]`
    Like {
        /// Tested value.
        expr: Box<Expr>,
        /// Pattern.
        pattern: Box<Expr>,
        /// Escape character expression.
        escape: Option<Box<Expr>>,
        /// `NOT LIKE`
        negated: bool,
    },
    /// `cond ? a : b`
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// System function call.
    Call {
        /// Function as written (original case).
        name: String,
        /// Resolved function.
        func: FuncRef,
        /// Arguments.
        args: Vec<Expr>,
    },
    /// User-defined function call, `udf.name(args)`. Run by the host the caller gives in `EvalOptions::udf`.
    Udf {
        /// Function name as written (case matters).
        name: String,
        /// Arguments.
        args: Vec<Expr>,
    },
    /// Aggregate function call.
    Aggregate {
        /// Which aggregate.
        kind: AggregateKind,
        /// Its argument.
        arg: Box<Expr>,
    },
    /// `[a, b]`
    Array(Vec<Expr>),
    /// `{"k": v}`
    Object(Vec<(String, Expr)>),
    /// `(SELECT ...)` used as a value.
    Subquery(Box<Query>),
    /// `EXISTS (SELECT ...)`
    Exists(Box<Query>),
    /// `ARRAY (SELECT ...)`
    ArraySubquery(Box<Query>),
}

/// Handle to a built-in function implementation.
#[derive(Clone, Copy)]
pub struct FuncRef(pub(crate) &'static FuncSpec);

impl std::fmt::Debug for FuncRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FuncRef({})", self.0.name)
    }
}

impl FuncRef {
    /// Canonical (upper case) function name.
    pub fn name(&self) -> &'static str {
        self.0.name
    }
}

impl ExprKind {
    /// Direct sub-expressions, not looking into subqueries.
    pub(crate) fn children(&self) -> Vec<&Expr> {
        match self {
            ExprKind::Literal(_) | ExprKind::Param(_) | ExprKind::Ident(_) => vec![],
            ExprKind::Property(b, _) => vec![b],
            ExprKind::Index(b, i) => vec![b, i],
            ExprKind::Unary(_, x) => vec![x],
            ExprKind::Binary(_, a, b) => vec![a, b],
            ExprKind::And(v) | ExprKind::Or(v) | ExprKind::Array(v) => v.iter().collect(),
            ExprKind::Between { expr, low, high, .. } => vec![expr, low, high],
            ExprKind::In { expr, list, .. } => std::iter::once(&**expr).chain(list.iter()).collect(),
            ExprKind::Like { expr, pattern, escape, .. } => {
                let mut v: Vec<&Expr> = vec![expr, pattern];
                if let Some(e) = escape {
                    v.push(e);
                }
                v
            }
            ExprKind::Ternary(c, a, b) => vec![c, a, b],
            ExprKind::Call { args, .. } | ExprKind::Udf { args, .. } => args.iter().collect(),
            ExprKind::Aggregate { arg, .. } => vec![arg],
            ExprKind::Object(p) => p.iter().map(|(_, e)| e).collect(),
            ExprKind::Subquery(_) | ExprKind::Exists(_) | ExprKind::ArraySubquery(_) => vec![],
        }
    }
}

impl Expr {
    pub(crate) fn children(&self) -> Vec<&Expr> {
        self.kind.children()
    }

    /// True if an aggregate function appears in this expression (subqueries excluded).
    pub(crate) fn contains_aggregate(&self) -> bool {
        matches!(self.kind, ExprKind::Aggregate { .. }) || self.children().iter().any(|c| c.contains_aggregate())
    }
}

impl Query {
    /// All expressions of this query (not looking into nested queries).
    pub(crate) fn exprs(&self) -> Vec<&Expr> {
        let mut v: Vec<&Expr> = Vec::new();
        if let Some(t) = &self.select.top {
            v.push(t);
        }
        match &self.select.spec {
            SelectSpec::Star => {}
            SelectSpec::Value(e) => v.push(e),
            SelectSpec::Items(items) => v.extend(items.iter().map(|i| &i.expr)),
        }
        if let Some(f) = &self.from {
            for s in &f.sources {
                if let SourceExpr::Expr(e) = &s.source {
                    v.push(e);
                }
            }
        }
        v.extend(self.where_clause.iter());
        v.extend(self.group_by.iter());
        v.extend(self.order_by.iter().map(|o| &o.expr));
        if let Some(ol) = &self.offset_limit {
            v.push(&ol.offset);
            v.push(&ol.limit);
        }
        v
    }
}
