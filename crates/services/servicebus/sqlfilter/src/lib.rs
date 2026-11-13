//! Service Bus SQL filter and SQL rule action language.
//!
//! Implements the languages described in Microsoft's "SQLFilter syntax" and
//! "SQLRuleAction syntax" pages, used by topic subscription rules.
//!
//! # Semantics (choices where the documentation is silent)
//!
//! * **Unknown.** A missing user property, a missing parameter, a `Null`
//!   value and the `NULL` literal are all "unknown". Arithmetic,
//!   comparison, `LIKE` and `IN` (left side) with an unknown operand give
//!   unknown; `AND`/`OR`/`NOT` use the documented three-valued tables
//!   (`AND` and `OR` short-circuit, so `FALSE AND <error>` is `FALSE`).
//!   A filter matches only when it evaluates to `TRUE`.
//! * **Types.** Follow C# promotion: `Long` with `Double` compares and
//!   computes as `Double`; strings compare ordinally; booleans support only
//!   `=` and `<>`; incompatible types (string vs number, ...) make a
//!   comparison unknown, so both `=` and `<>` are false. A non-boolean
//!   operand of `AND`/`OR`/`NOT` is unknown. Integer arithmetic wraps;
//!   integer `/` or `%` by zero is an evaluation error; `+` concatenates
//!   when either side is a string. `Timestamp - Timestamp` yields a `Long`
//!   (milliseconds) and `Timestamp +/- Long` adds milliseconds, standing in
//!   for .NET `TimeSpan`.
//! * **System properties.** Reading a `sys.` property that has no value is
//!   unknown when it is one of the standard broker property names, and an
//!   evaluation error otherwise (the docs: a nonexistent system property is
//!   an error). `IS [NOT] NULL` and `EXISTS` never fail.
//! * **Errors.** A filter that fails to evaluate does not match; an action
//!   that fails changes nothing (see [`Action::try_apply`]).
//! * **Top-level filter.** Must be boolean-shaped: a bare number, string,
//!   `NULL` or arithmetic expression is rejected at parse time; a bare
//!   property, parameter or `TRUE`/`FALSE` is allowed and matches only when
//!   it is the boolean `TRUE`. `1=1` is the usual always-true filter.
//! * **Names.** Property names are case-insensitive in Service Bus; this
//!   crate passes the name as written and the [`Properties`] implementor
//!   does the lookup. Function names and keywords are case-insensitive.
//!   `property(name)`/`p(name)` look up a *user* property named by the
//!   string; `newid()` returns a random GUID string.
//! * **LIKE.** Case-sensitive, `%` and `_`; with `ESCAPE c` an escape
//!   followed by any character matches that character literally; an escape
//!   at the end of the pattern or an `ESCAPE` value that is not exactly one
//!   character is an evaluation error. `IN` treats unknown list items as
//!   not matching.
//! * **Actions.** Statements are `SET prop = expr` and `REMOVE prop`,
//!   separated by `;` (optional between statements), and `SET a = 1, b = 2`
//!   / `REMOVE a, b` / `SET a = 1, SET b = 2` lists are accepted. Later
//!   statements see earlier ones. `REMOVE sys.x` is a parse error. Setting
//!   an unknown value stores `Value::Null`. `SET` converts a string to a
//!   timestamp (ISO 8601) when the existing property is a timestamp, and an
//!   integer to a double when the existing property is a double.
//!
//! # Limits
//!
//! Filters and actions are limited to [`MAX_SQL_LEN`] characters, nesting
//! to [`MAX_DEPTH`] levels, and an action to [`MAX_ACTION_STATEMENTS`]
//! statements (Service Bus quotas).

mod action;
mod ast;
mod error;
mod eval;
mod lexer;
mod like;
mod ops;
mod parser;
mod time;
mod value;

pub use error::{EvalError, ParseError};
pub use value::{MapProperties, Params, Properties, PropertiesMut, Scope, Value};

/// Maximum length of a filter or action string (Service Bus quota: 1,024).
pub const MAX_SQL_LEN: usize = 1024;
/// Maximum expression nesting depth.
pub const MAX_DEPTH: usize = 100;
/// Maximum statements per action (Service Bus quota: 32).
pub const MAX_ACTION_STATEMENTS: usize = 32;

/// A parsed SQL filter.
#[derive(Debug, Clone)]
pub struct Filter {
    sql: String,
    expr: ast::Expr,
}

impl Filter {
    pub fn parse(sql: &str) -> Result<Filter, ParseError> {
        let expr = parser::parse_filter(sql)?;
        Ok(Filter { sql: sql.to_string(), expr })
    }

    /// Evaluates against a message's properties; evaluation errors count as
    /// "does not match".
    pub fn matches(&self, props: &dyn Properties, params: &Params) -> bool {
        self.try_matches(props, params).unwrap_or(false)
    }

    /// Like [`Filter::matches`] but reports evaluation errors.
    pub fn try_matches(&self, props: &dyn Properties, params: &Params) -> Result<bool, EvalError> {
        let cx = eval::Ctx { props, params };
        Ok(matches!(eval::eval(&self.expr, &cx)?, Some(Value::Bool(true))))
    }

    pub fn sql(&self) -> &str {
        &self.sql
    }
}

/// A parsed SQL rule action.
#[derive(Debug, Clone)]
pub struct Action {
    sql: String,
    stmts: Vec<ast::Stmt>,
}

impl Action {
    pub fn parse(sql: &str) -> Result<Action, ParseError> {
        let stmts = action::parse_action(sql)?;
        Ok(Action { sql: sql.to_string(), stmts })
    }

    /// Applies the action. If it fails (see [`Action::try_apply`]) the
    /// properties are left unchanged.
    pub fn apply(&self, props: &mut dyn PropertiesMut, params: &Params) {
        let _ = self.try_apply(props, params);
    }

    /// Applies the action atomically: either every statement takes effect or,
    /// on error, none does. Service Bus dead-letters the message on error.
    pub fn try_apply(&self, props: &mut dyn PropertiesMut, params: &Params) -> Result<(), EvalError> {
        action::run(&self.stmts, props, params)
    }

    pub fn sql(&self) -> &str {
        &self.sql
    }
}
