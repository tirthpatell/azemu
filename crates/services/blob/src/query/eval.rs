//! Evaluating expressions against one row.

use std::cmp::Ordering;
use std::collections::HashMap;

use super::budget::Budget;
use super::datetime::{self, Part, Ts};
use super::sql::{AggFn, BinOp, Expr, FuncId, Step, TrimMode};
use super::value::{as_f64, cast, compare, parse_bool, to_number, CastType, EvalError, EvalResult, Value};

/// One input record.
pub enum Row {
    /// The fields of a delimited text record.
    Csv(Vec<String>),
    /// A JSON value (usually an object).
    Json(Value),
}

/// The header row of a delimited blob: what a field name means.
#[derive(Default)]
pub struct Columns {
    pub names: Vec<String>,
    exact: HashMap<String, usize>,
    lower: HashMap<String, usize>,
}

impl Columns {
    pub fn new(names: Vec<String>) -> Columns {
        let mut exact = HashMap::new();
        let mut lower = HashMap::new();
        for (i, n) in names.iter().enumerate() {
            exact.entry(n.clone()).or_insert(i);
            lower.entry(n.to_lowercase()).or_insert(i);
        }
        Columns { names, exact, lower }
    }

    /// Quoted names match exactly, unquoted ones ignore case.
    pub fn find(&self, name: &str, quoted: bool) -> Option<usize> {
        if quoted {
            self.exact.get(name).copied()
        } else {
            self.lower.get(&name.to_lowercase()).copied()
        }
    }
}

pub struct Ctx<'a> {
    pub row: &'a Row,
    pub columns: &'a Columns,
    /// `UTCNOW()`: the time the query started.
    pub now: Ts,
    /// Time limit and cancellation; every expression evaluated counts against it.
    pub budget: &'a Budget,
}

fn missing_column() -> EvalError {
    // the message is filled in by the caller, which knows the position in the select list
    EvalError::new("InvalidColumnOrdinal", String::new())
}

fn json_field<'a>(fields: &'a [(String, Value)], name: &str, quoted: bool) -> Option<&'a Value> {
    if let Some((_, v)) = fields.iter().find(|(k, _)| k == name) {
        return Some(v);
    }
    if quoted {
        return None;
    }
    fields.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v)
}

/// Follow the steps down a JSON value without copying it; only the value found is copied by the caller.
fn walk<'a>(mut cur: &'a Value, steps: &[Step]) -> Option<&'a Value> {
    for step in steps {
        cur = match (step, cur) {
            (Step::Field { name, quoted }, Value::Object(o)) => json_field(o, name, *quoted)?,
            (Step::Ordinal(n), Value::Object(o)) => json_field(o, &format!("_{n}"), false)?,
            (Step::Index(i), Value::Array(a)) => a.get(*i)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn eval_ref(steps: &[Step], ctx: &Ctx) -> EvalResult<Value> {
    let rest;
    let cur: Value = match ctx.row {
        Row::Csv(fields) => {
            let (first, tail) = steps.split_first().ok_or_else(missing_column)?;
            let idx = match first {
                Step::Ordinal(n) => n - 1,
                Step::Field { name, quoted } => match ctx.columns.find(name, *quoted) {
                    Some(i) => i,
                    None if ctx.columns.names.is_empty() => {
                        return Err(EvalError::fatal(
                            "InvalidColumnName",
                            format!(
                                "The column '{name}' cannot be found: the data has no header row (HasHeaders is false); use _1, _2, ..."
                            ),
                        ))
                    }
                    None => return Err(EvalError::fatal("InvalidColumnName", format!("The column '{name}' is not in the header row."))),
                },
                Step::Index(_) => return Ok(Value::Missing),
            };
            rest = tail;
            match fields.get(idx) {
                Some(f) => Value::Str(f.clone()),
                None => return Err(missing_column()),
            }
        }
        Row::Json(v) => return Ok(walk(v, steps).cloned().unwrap_or(Value::Missing)),
    };
    // a field of a delimited record is text; going further into it finds nothing
    Ok(if rest.is_empty() { cur } else { Value::Missing })
}

/// Three-valued truth: `None` is unknown (NULL or MISSING).
fn truth(v: &Value) -> EvalResult<Option<bool>> {
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null | Value::Missing => Ok(None),
        Value::Str(s) => parse_bool(s).map(Some).ok_or_else(|| EvalError::new("CastFailed", format!("'{s}' is not a boolean."))),
        other => Err(EvalError::new("CastFailed", format!("A {} is not a boolean.", other.type_name()))),
    }
}

pub fn is_true(v: &Value) -> EvalResult<bool> {
    Ok(truth(v)? == Some(true))
}

fn tri(b: Option<bool>) -> Value {
    b.map(Value::Bool).unwrap_or(Value::Null)
}

fn text(v: &Value) -> String {
    v.to_text()
}

fn ts_arg(v: &Value) -> EvalResult<Ts> {
    match cast(v, CastType::Timestamp)? {
        Value::Ts(t) => Ok(t),
        _ => Err(EvalError::new("CastFailed", "A timestamp is needed.")),
    }
}

fn int_arg(v: &Value) -> EvalResult<i64> {
    match cast(&to_number(v)?, CastType::Int)? {
        Value::Int(i) => Ok(i),
        _ => Err(EvalError::new("CastFailed", "A whole number is needed.")),
    }
}

fn overflow() -> EvalError {
    EvalError::new("ArithmeticOverflow", "The result does not fit in a 64-bit integer.")
}

fn arith(op: BinOp, a: &Value, b: &Value) -> EvalResult<Value> {
    if a.is_nullish() || b.is_nullish() {
        return Ok(Value::Null);
    }
    if op == BinOp::Concat {
        let (x, y) = (text(a), text(b));
        if x.len() + y.len() > MAX_STRING {
            return Err(EvalError::new("ValueTooLarge", format!("A string built by || is longer than {MAX_STRING} bytes.")));
        }
        return Ok(Value::Str(x + &y));
    }
    // a number of days added to or taken from a timestamp
    if let (Value::Ts(t), true) = (a, matches!(op, BinOp::Add | BinOp::Sub)) {
        let n = int_arg(b)?;
        let n = if op == BinOp::Sub { n.checked_neg().ok_or_else(overflow)? } else { n };
        return datetime::add(Part::Day, n, t)
            .map(Value::Ts)
            .ok_or_else(|| EvalError::new("ArithmeticOverflow", "The date is out of range."));
    }
    let (x, y) = (to_number(a)?, to_number(b)?);
    if let (Value::Int(x), Value::Int(y)) = (&x, &y) {
        let (x, y) = (*x, *y);
        return match op {
            BinOp::Add => x.checked_add(y).map(Value::Int).ok_or_else(overflow),
            BinOp::Sub => x.checked_sub(y).map(Value::Int).ok_or_else(overflow),
            BinOp::Mul => x.checked_mul(y).map(Value::Int).ok_or_else(overflow),
            BinOp::Div if y == 0 => Err(EvalError::new("DivisionByZero", "Division by zero.")),
            BinOp::Div => x.checked_div(y).map(Value::Int).ok_or_else(overflow),
            BinOp::Mod if y == 0 => Err(EvalError::new("DivisionByZero", "Division by zero.")),
            BinOp::Mod => x.checked_rem(y).map(Value::Int).ok_or_else(overflow),
            _ => Err(EvalError::new("InvalidOperation", "Not an arithmetic operator.")),
        };
    }
    let (x, y) = (as_f64(&x), as_f64(&y));
    let r = match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        BinOp::Div if y == 0.0 => return Err(EvalError::new("DivisionByZero", "Division by zero.")),
        BinOp::Div => x / y,
        BinOp::Mod if y == 0.0 => return Err(EvalError::new("DivisionByZero", "Division by zero.")),
        BinOp::Mod => x % y,
        _ => return Err(EvalError::new("InvalidOperation", "Not an arithmetic operator.")),
    };
    Ok(Value::Float(r))
}

fn cmp_op(op: BinOp, a: &Value, b: &Value) -> EvalResult<Value> {
    let Some(o) = compare(a, b)? else { return Ok(Value::Null) };
    Ok(Value::Bool(match op {
        BinOp::Eq => o == Ordering::Equal,
        BinOp::Ne => o != Ordering::Equal,
        BinOp::Lt => o == Ordering::Less,
        BinOp::Le => o != Ordering::Greater,
        BinOp::Gt => o == Ordering::Greater,
        _ => o != Ordering::Less,
    }))
}

/// The longest string an expression may build (concatenation).
const MAX_STRING: usize = 16 * 1024 * 1024;

fn trim(mode: TrimMode, chars: &str, s: &str) -> String {
    let set: std::collections::HashSet<char> = chars.chars().collect();
    let f = |c: char| set.contains(&c);
    match mode {
        TrimMode::Both => s.trim_matches(f),
        TrimMode::Leading => s.trim_start_matches(f),
        TrimMode::Trailing => s.trim_end_matches(f),
    }
    .to_string()
}

fn func(id: FuncId, part: Option<Part>, args: &[Expr], ctx: &Ctx) -> EvalResult<Value> {
    // COALESCE and NULLIF look at their arguments one at a time
    match id {
        FuncId::Coalesce => {
            for a in args {
                let v = eval(a, ctx)?;
                if !v.is_nullish() {
                    return Ok(v);
                }
            }
            return Ok(Value::Null);
        }
        FuncId::UtcNow => return Ok(Value::Ts(ctx.now)),
        _ => {}
    }
    let vals: Vec<Value> = args.iter().map(|a| eval(a, ctx)).collect::<EvalResult<_>>()?;
    if id == FuncId::NullIf {
        return Ok(if compare(&vals[0], &vals[1])? == Some(Ordering::Equal) { Value::Null } else { vals[0].clone() });
    }
    if vals.iter().any(Value::is_nullish) {
        return Ok(Value::Null);
    }
    let range = || EvalError::new("ArithmeticOverflow", "The date is out of range.");
    Ok(match id {
        FuncId::CharLength => Value::Int(text(&vals[0]).chars().count() as i64),
        FuncId::Lower => Value::Str(text(&vals[0]).to_lowercase()),
        FuncId::Upper => Value::Str(text(&vals[0]).to_uppercase()),
        FuncId::Substring => {
            // Microsoft's example is SUBSTRING('123456789', 1, 5) = '23456': the position counts from 0
            let s: Vec<char> = text(&vals[0]).chars().collect();
            let start = int_arg(&vals[1])?.max(0) as usize;
            let len = match vals.get(2) {
                Some(l) => int_arg(l)?.max(0) as usize,
                None => usize::MAX,
            };
            let from = start.min(s.len());
            let to = from.saturating_add(len).min(s.len());
            Value::Str(s[from..to].iter().collect())
        }
        FuncId::ToString => {
            let t = ts_arg(&vals[0])?;
            Value::Str(datetime::to_string(&t, &text(&vals[1])).map_err(|m| EvalError::new("InvalidFormat", m))?)
        }
        FuncId::ToTimestamp => Value::Ts(ts_arg(&vals[0])?),
        FuncId::DateAdd => {
            let (Some(p), n, t) = (part, int_arg(&vals[0])?, ts_arg(&vals[1])?) else { return Err(range()) };
            Value::Ts(datetime::add(p, n, &t).ok_or_else(range)?)
        }
        FuncId::DateDiff => {
            let (Some(p), a, b) = (part, ts_arg(&vals[0])?, ts_arg(&vals[1])?) else { return Err(range()) };
            Value::Int(datetime::diff(p, &a, &b).ok_or_else(range)?)
        }
        FuncId::Coalesce | FuncId::NullIf | FuncId::UtcNow => Value::Null,
    })
}

/// Evaluate one expression. Every call counts as a unit of work, and a text it produces counts for its size, so the
/// time limit is looked at inside a long select list and while big values are copied, compared and converted.
pub fn eval(e: &Expr, ctx: &Ctx) -> EvalResult<Value> {
    ctx.budget.work(1)?;
    let v = eval_node(e, ctx)?;
    if let Value::Str(s) = &v {
        ctx.budget.bytes(s.len())?;
    }
    Ok(v)
}

fn eval_node(e: &Expr, ctx: &Ctx) -> EvalResult<Value> {
    Ok(match e {
        Expr::Lit(v) => v.clone(),
        Expr::Ref(steps) => eval_ref(steps, ctx)?,
        Expr::Neg(x) => {
            let v = eval(x, ctx)?;
            if v.is_nullish() {
                Value::Null
            } else {
                match to_number(&v)? {
                    Value::Int(i) => Value::Int(i.checked_neg().ok_or_else(overflow)?),
                    Value::Float(f) => Value::Float(-f),
                    _ => Value::Null,
                }
            }
        }
        Expr::Not(x) => tri(truth(&eval(x, ctx)?)?.map(|b| !b)),
        Expr::Bin(BinOp::And, a, b) => {
            let l = truth(&eval(a, ctx)?)?;
            if l == Some(false) {
                return Ok(Value::Bool(false));
            }
            let r = truth(&eval(b, ctx)?)?;
            tri(match (l, r) {
                (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            })
        }
        Expr::Bin(BinOp::Or, a, b) => {
            let l = truth(&eval(a, ctx)?)?;
            if l == Some(true) {
                return Ok(Value::Bool(true));
            }
            let r = truth(&eval(b, ctx)?)?;
            tri(match (l, r) {
                (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            })
        }
        Expr::Bin(op, a, b) => {
            let (l, r) = (eval(a, ctx)?, eval(b, ctx)?);
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Concat => arith(*op, &l, &r)?,
                _ => cmp_op(*op, &l, &r)?,
            }
        }
        Expr::IsNull { e, neg } => Value::Bool(eval(e, ctx)?.is_nullish() != *neg),
        Expr::IsMissing { e, neg } => Value::Bool(matches!(eval(e, ctx)?, Value::Missing) != *neg),
        Expr::Between { e, lo, hi, neg } => {
            let v = eval(e, ctx)?;
            let ge = cmp_op(BinOp::Ge, &v, &eval(lo, ctx)?)?;
            let le = cmp_op(BinOp::Le, &v, &eval(hi, ctx)?)?;
            let r = match (truth(&ge)?, truth(&le)?) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            };
            tri(r.map(|b| b != *neg))
        }
        Expr::In { e, list, neg } => {
            let v = eval(e, ctx)?;
            if v.is_nullish() {
                return Ok(Value::Null);
            }
            let mut unknown = false;
            for x in list {
                match compare(&v, &eval(x, ctx)?)? {
                    Some(Ordering::Equal) => return Ok(Value::Bool(!*neg)),
                    Some(_) => {}
                    None => unknown = true,
                }
            }
            if unknown {
                Value::Null
            } else {
                Value::Bool(*neg)
            }
        }
        Expr::Like { e, pat, esc, neg } => {
            let (v, p) = (eval(e, ctx)?, eval(pat, ctx)?);
            let esc = match esc {
                Some(x) => {
                    let v = eval(x, ctx)?;
                    let t = text(&v);
                    let mut c = t.chars();
                    match (c.next(), c.next()) {
                        (Some(ch), None) => Some(ch),
                        _ => return Err(EvalError::new("InvalidLikePattern", "ESCAPE takes a single character.")),
                    }
                }
                None => None,
            };
            if v.is_nullish() || p.is_nullish() {
                return Ok(Value::Null);
            }
            let pattern = ctx.budget.like_pattern(&text(&p), esc)?;
            Value::Bool(pattern.matches(&text(&v), ctx.budget)? != *neg)
        }
        Expr::Cast(x, ty) => cast(&eval(x, ctx)?, *ty)?,
        Expr::Func(id, part, args) => func(*id, *part, args, ctx)?,
        Expr::Extract(p, x) => {
            let v = eval(x, ctx)?;
            if v.is_nullish() {
                Value::Null
            } else {
                Value::Int(datetime::extract(*p, &ts_arg(&v)?))
            }
        }
        Expr::Trim { mode, chars, e } => {
            let v = eval(e, ctx)?;
            let c = match chars {
                Some(c) => eval(c, ctx)?,
                None => Value::Str(" ".into()),
            };
            if v.is_nullish() || c.is_nullish() {
                Value::Null
            } else {
                Value::Str(trim(*mode, &text(&c), &text(&v)))
            }
        }
        Expr::Agg(..) => return Err(EvalError::fatal("InvalidQuery", "An aggregate function is not allowed here.")),
    })
}

/// Running state of one aggregate select item.
pub enum Acc {
    Count(i64),
    Sum { int: Option<i64>, float: f64, any: bool },
    Avg { sum: f64, n: u64 },
    Min(Option<Value>),
    Max(Option<Value>),
}

impl Acc {
    pub fn new(f: AggFn) -> Acc {
        match f {
            AggFn::Count => Acc::Count(0),
            AggFn::Sum => Acc::Sum { int: Some(0), float: 0.0, any: false },
            AggFn::Avg => Acc::Avg { sum: 0.0, n: 0 },
            AggFn::Min => Acc::Min(None),
            AggFn::Max => Acc::Max(None),
        }
    }

    /// Add one row. `arg` is `None` for `COUNT(*)`; NULL and MISSING arguments are left out.
    pub fn add(&mut self, arg: Option<&Value>) -> EvalResult<()> {
        let Some(v) = arg else {
            if let Acc::Count(n) = self {
                *n += 1;
            }
            return Ok(());
        };
        if v.is_nullish() {
            return Ok(());
        }
        match self {
            Acc::Count(n) => *n += 1,
            Acc::Sum { int, float, any } => {
                let n = to_number(v)?;
                *any = true;
                match (&n, *int) {
                    (Value::Int(i), Some(acc)) => match acc.checked_add(*i) {
                        Some(s) => *int = Some(s),
                        None => {
                            *float = acc as f64 + *i as f64;
                            *int = None;
                        }
                    },
                    (Value::Int(i), None) => *float += *i as f64,
                    (other, Some(acc)) => {
                        *float = acc as f64 + as_f64(other);
                        *int = None;
                    }
                    (other, None) => *float += as_f64(other),
                }
            }
            Acc::Avg { sum, n } => {
                *sum += as_f64(&to_number(v)?);
                *n += 1;
            }
            Acc::Min(cur) => {
                if cur.as_ref().map(|c| compare(v, c)).transpose()?.flatten() == Some(Ordering::Less) || cur.is_none() {
                    *cur = Some(v.clone());
                }
            }
            Acc::Max(cur) => {
                if cur.as_ref().map(|c| compare(v, c)).transpose()?.flatten() == Some(Ordering::Greater) || cur.is_none() {
                    *cur = Some(v.clone());
                }
            }
        }
        Ok(())
    }

    pub fn result(&self) -> Value {
        match self {
            Acc::Count(n) => Value::Int(*n),
            Acc::Sum { any: false, .. } => Value::Null,
            Acc::Sum { int: Some(i), .. } => Value::Int(*i),
            Acc::Sum { float, .. } => Value::Float(*float),
            Acc::Avg { n: 0, .. } => Value::Null,
            Acc::Avg { sum, n } => Value::Float(sum / *n as f64),
            Acc::Min(v) | Acc::Max(v) => v.clone().unwrap_or(Value::Null),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::sql;
    use super::*;

    fn run(expr: &str, fields: &[&str], headers: &[&str]) -> EvalResult<Value> {
        let q = sql::parse(&format!("SELECT {expr} FROM BlobStorage")).unwrap();
        let sql::Projection::List(items) = q.projection else { panic!() };
        let row = Row::Csv(fields.iter().map(|s| s.to_string()).collect());
        let cols = Columns::new(headers.iter().map(|s| s.to_string()).collect());
        eval(&items[0].expr, &Ctx { row: &row, columns: &cols, now: Ts { secs: 0, nanos: 0, offset_min: 0 }, budget: &Budget::unlimited() })
    }

    fn v(expr: &str) -> Value {
        run(expr, &[], &[]).unwrap_or_else(|e| panic!("{expr}: {e:?}"))
    }

    #[test]
    fn documented_string_examples() {
        assert_eq!(v("CHARACTER_LENGTH('abcdefg')"), Value::Int(7));
        assert_eq!(v("LOWER('AbCdEfG')"), Value::Str("abcdefg".into()));
        assert_eq!(v("UPPER('AbCdEfG')"), Value::Str("ABCDEFG".into()));
        assert_eq!(v("SUBSTRING('123456789', 1, 5)"), Value::Str("23456".into()));
        assert_eq!(v("TRIM(BOTH '123' FROM '1112211Microsoft22211122')"), Value::Str("Microsoft".into()));
        assert_eq!(v("TRIM(LEADING '1' FROM '1121')"), Value::Str("21".into()));
        assert_eq!(v("TRIM(TRAILING '1' FROM '1121')"), Value::Str("112".into()));
        assert_eq!(v("TRIM('  x ')"), Value::Str("x".into()));
        assert_eq!(v("'a' || 1 || 'b'"), Value::Str("a1b".into()));
        assert_eq!(run("CHAR_LENGTH(_1)", &["x"], &[]).unwrap(), Value::Int(1));
    }

    #[test]
    fn documented_date_examples() {
        assert_eq!(v("DATE_ADD('minute', 1, CAST('2017-01-02T03:04:05.006Z' AS TIMESTAMP))").to_text(), "2017-01-02T03:05:05.006Z");
        assert_eq!(v("DATE_DIFF('hour','2018-11-09T00:00+05:30','2018-11-09T01:00:23-08:00')"), Value::Int(14));
        assert_eq!(v("EXTRACT(YEAR FROM '2010-01-01T')"), Value::Int(2010));
        assert_eq!(v("TO_STRING(CAST('1969-07-20T20:18Z' AS TIMESTAMP),  'MMMM d, y')"), Value::Str("July 20, 1969".into()));
        assert_eq!(v("TO_TIMESTAMP('2007T')").to_text(), "2007-01-01T00:00:00Z");
        assert_eq!(v("UTCNOW()").to_text(), "1970-01-01T00:00:00Z");
        assert_eq!(v("TO_TIMESTAMP('2020-01-01') + 2").to_text(), "2020-01-03T00:00:00Z");
        assert_eq!(v("EXTRACT(timezone_hour FROM '2020-01-01T00:00+05:30')"), Value::Int(5));
    }

    #[test]
    fn arithmetic_and_comparison_with_implicit_casts() {
        assert_eq!(v("1 + 2 * 3 - 4 % 3"), Value::Int(6));
        assert_eq!(v("7 / 2"), Value::Int(3));
        assert_eq!(v("7 / 2.0"), Value::Float(3.5));
        assert_eq!(v("-(2 + 3)"), Value::Int(-5));
        assert_eq!(run("_1 > 250", &["300"], &[]).unwrap(), Value::Bool(true));
        assert_eq!(run("_1 + _2", &["1", "2.5"], &[]).unwrap(), Value::Float(3.5));
        assert_eq!(run("CAST(_1 AS INT) + 1", &["41"], &[]).unwrap(), Value::Int(42));
        assert_eq!(run("repopath", &["a", "b"], &["Service", "RepoPath"]).unwrap(), Value::Str("b".into()));
        assert_eq!(run("\"Service\"", &["a"], &["Service"]).unwrap(), Value::Str("a".into()));
        assert!(run("\"service\"", &["a"], &["Service"]).unwrap_err().fatal, "a quoted name is case sensitive");
        assert!(run("9223372036854775807 + 1", &[], &[]).is_err());
        assert!(run("1 / 0", &[], &[]).is_err());
        assert!(run("_1 > 1", &["abc"], &[]).is_err());
    }

    #[test]
    fn logic_with_unknowns() {
        assert_eq!(v("NULL IS NULL"), Value::Bool(true));
        assert_eq!(v("1 IS NOT NULL"), Value::Bool(true));
        assert_eq!(v("NULL = 1"), Value::Null);
        assert_eq!(v("false AND NULL"), Value::Bool(false));
        assert_eq!(v("true AND NULL"), Value::Null);
        assert_eq!(v("true OR NULL"), Value::Bool(true));
        assert_eq!(v("NOT NULL"), Value::Null);
        assert_eq!(v("2 BETWEEN 1 AND 3"), Value::Bool(true));
        assert_eq!(v("2 NOT BETWEEN 1 AND 3"), Value::Bool(false));
        assert_eq!(v("2 IN (1, 2)"), Value::Bool(true));
        assert_eq!(v("3 IN (1, 2)"), Value::Bool(false));
        assert_eq!(v("3 IN (1, NULL)"), Value::Null);
        assert_eq!(v("3 NOT IN (1, 2)"), Value::Bool(true));
        assert_eq!(v("COALESCE(NULL, 2, 3)"), Value::Int(2));
        assert_eq!(v("NULLIF(2, 2)"), Value::Null);
        assert_eq!(v("NULLIF(2, 3)"), Value::Int(2));
    }

    #[test]
    fn like_patterns() {
        assert_eq!(v("'abc' LIKE 'a%'"), Value::Bool(true));
        assert_eq!(v("'abc' LIKE '_b_'"), Value::Bool(true));
        assert_eq!(v("'abc' LIKE 'a_'"), Value::Bool(false));
        assert_eq!(v("'abc' NOT LIKE '%x%'"), Value::Bool(true));
        assert_eq!(v("'a%c' LIKE 'a\\%c' ESCAPE '\\'"), Value::Bool(true));
        assert_eq!(v("'abc' LIKE 'a\\%c' ESCAPE '\\'"), Value::Bool(false));
        assert_eq!(v("'' LIKE '%'"), Value::Bool(true));
        // a pattern that used to need a quadratic number of steps (and was refused) is answered at once
        let pat = format!("%{}b", "a".repeat(3000));
        let text = "a".repeat(30_000);
        assert_eq!(v(&format!("'{text}' LIKE '{pat}'")), Value::Bool(false));
        // no exponential backtracking
        let long = "a".repeat(5000);
        assert_eq!(v(&format!("'{long}' LIKE '%a%a%a%a%a%b'")), Value::Bool(false));
    }

    #[test]
    fn json_rows() {
        let row = Row::Json(super::super::json::parse(br#"{"Name":"mouse","tags":["a","b"],"dim":{"w":2},"n":null}"#).unwrap());
        let cols = Columns::default();
        let budget = Budget::unlimited();
        let ctx = Ctx { row: &row, columns: &cols, now: Ts { secs: 0, nanos: 0, offset_min: 0 }, budget: &budget };
        let q = |s: &str| {
            let sel = sql::parse(&format!("SELECT {s} FROM BlobStorage")).unwrap();
            let sql::Projection::List(items) = sel.projection else { panic!() };
            eval(&items[0].expr, &ctx).unwrap()
        };
        assert_eq!(q("name"), Value::Str("mouse".into()));
        assert_eq!(q("\"name\""), Value::Missing);
        assert_eq!(q("tags[1]"), Value::Str("b".into()));
        assert_eq!(q("dim.w"), Value::Int(2));
        assert_eq!(q("nope"), Value::Missing);
        assert_eq!(q("nope IS MISSING"), Value::Bool(true));
        assert_eq!(q("n IS MISSING"), Value::Bool(false));
        assert_eq!(q("n IS NULL"), Value::Bool(true));
        assert_eq!(q("tags[5] IS MISSING"), Value::Bool(true));
    }

    #[test]
    fn aggregates() {
        let mut c = Acc::new(AggFn::Count);
        let mut s = Acc::new(AggFn::Sum);
        let mut a = Acc::new(AggFn::Avg);
        let mut mn = Acc::new(AggFn::Min);
        let mut mx = Acc::new(AggFn::Max);
        for x in [Value::Int(3), Value::Null, Value::Int(1), Value::Str("2".into())] {
            c.add(Some(&x)).unwrap();
            s.add(Some(&x)).unwrap();
            a.add(Some(&x)).unwrap();
            mn.add(Some(&Value::Int(match &x {
                Value::Int(i) => *i,
                _ => 9,
            })))
            .unwrap();
            mx.add(Some(&x)).ok();
        }
        assert_eq!(c.result(), Value::Int(3));
        assert_eq!(s.result(), Value::Int(6));
        assert_eq!(a.result(), Value::Float(2.0));
        assert_eq!(mn.result(), Value::Int(1));
        assert_eq!(Acc::new(AggFn::Sum).result(), Value::Null);
        assert_eq!(Acc::new(AggFn::Count).result(), Value::Int(0));
        let _ = mx;
    }
}
