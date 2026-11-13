//! Expression evaluator.

use crate::ast::{BinaryOp, Expr, ExprKind, Literal, UnaryOp};
use crate::error::SqlError;
use crate::ops;
use crate::value::{json_index, values_equal, Val};
use crate::{EvalOptions, Params};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::SystemTime;

pub(crate) mod query;

/// Per-execution state.
pub(crate) struct Ctx {
    params: HashMap<String, Val>,
    fixed_now: bool,
    static_now: i64,
    rng: Cell<u64>,
    budget: Cell<usize>,
    /// Bytes the query may hold at once (intermediate results, groups, the sorted rows) and what it holds now.
    bytes_max: usize,
    bytes_used: Cell<usize>,
    /// Set by a function whose result would be larger than one value may be (`value_too_big`).
    overflow: Cell<bool>,
    pub order_by_excludes_undefined: bool,
    regexes: RefCell<HashMap<(String, String), Option<regex::Regex>>>,
    udf: Option<Box<dyn crate::UdfHost>>,
}

fn system_ticks() -> i64 {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => (d.as_nanos() / 100) as i64,
        Err(e) => -((e.duration().as_nanos() / 100) as i64),
    }
}

impl Ctx {
    pub fn new(params: &Params, opts: &EvalOptions) -> Ctx {
        let static_now = match opts.now {
            Some(t) => match t.duration_since(SystemTime::UNIX_EPOCH) {
                Ok(d) => (d.as_nanos() / 100) as i64,
                Err(e) => -((e.duration().as_nanos() / 100) as i64),
            },
            None => system_ticks(),
        };
        let seed = opts.rand_seed.unwrap_or(static_now as u64 ^ 0x9E37_79B9_7F4A_7C15);
        Ctx {
            params: params.iter().map(|(k, v)| (k.to_string(), Val::from_json(v))).collect(),
            fixed_now: opts.now.is_some(),
            static_now,
            rng: Cell::new(seed | 1),
            budget: Cell::new(opts.max_rows),
            bytes_max: opts.max_bytes,
            bytes_used: Cell::new(0),
            overflow: Cell::new(false),
            order_by_excludes_undefined: opts.order_by_excludes_undefined,
            regexes: RefCell::new(HashMap::new()),
            udf: opts.udf.as_ref().map(|f| f.make()),
        }
    }

    pub fn now_ticks(&self, is_static: bool) -> i64 {
        if is_static || self.fixed_now {
            self.static_now
        } else {
            system_ticks()
        }
    }

    /// xorshift64*, scaled to [0, 1).
    pub fn rand(&self) -> f64 {
        let mut x = self.rng.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng.set(x);
        let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (r >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn charge(&self, n: usize) -> Result<(), SqlError> {
        let left = self.budget.get();
        if n > left {
            return Err(SqlError::limit("The query produced too many intermediate rows.".to_string()));
        }
        self.budget.set(left - n);
        Ok(())
    }

    /// Counts `n` bytes against the memory budget of the query.
    pub fn charge_bytes(&self, n: usize) -> Result<(), SqlError> {
        let used = self.bytes_used.get().saturating_add(n);
        if used > self.bytes_max {
            return Err(SqlError::limit(format!(
                "The query needs more than {} MB of memory for its intermediate results, which is more than the emulator allows for one query. \
                 Narrow it with a WHERE clause or a smaller TOP, or avoid a large ORDER BY, GROUP BY, DISTINCT or JOIN.",
                self.bytes_max / (1024 * 1024)
            )));
        }
        self.bytes_used.set(used);
        Ok(())
    }

    pub fn release_bytes(&self, n: usize) {
        self.bytes_used.set(self.bytes_used.get().saturating_sub(n));
    }

    pub fn bytes_used(&self) -> usize {
        self.bytes_used.get()
    }

    /// Gives back what a subquery held (its result is consumed by the caller, who counts what it keeps).
    pub fn reset_bytes(&self, to: usize) {
        self.bytes_used.set(to);
    }

    /// Bytes still available, as a cap for measuring a value.
    pub fn bytes_left(&self) -> usize {
        self.bytes_max.saturating_sub(self.bytes_used.get())
    }

    /// An expression that builds a value out of parts (an array or object literal, a select list) calls this with
    /// the size of the parts so far: reading the same big property many times in one expression makes a copy each
    /// time, so the sum is checked as it grows, before the memory is used.
    pub fn check_room(&self, size: usize) -> Result<(), SqlError> {
        if size > self.bytes_left() {
            return Err(SqlError::limit(format!(
                "The query builds a value larger than {} MB, which is more than the emulator allows for one query.",
                self.bytes_max / (1024 * 1024)
            )));
        }
        Ok(())
    }

    /// A function was asked to build a value larger than `MAX_VALUE_BYTES`. It returns undefined and the
    /// evaluator turns the flag into an error, so the query fails instead of quietly dropping the value.
    pub fn value_too_big(&self) {
        self.overflow.set(true);
    }

    /// Compiled regular expression (cached). `None` if the pattern or modifiers are invalid.
    pub fn regex(&self, pattern: &str, mods: &str) -> Option<regex::Regex> {
        let key = (pattern.to_string(), mods.to_string());
        if let Some(r) = self.regexes.borrow().get(&key) {
            return r.clone();
        }
        let mut b = regex::RegexBuilder::new(pattern);
        b.size_limit(1 << 20);
        let mut ok = true;
        for m in mods.chars() {
            match m {
                'i' => {
                    b.case_insensitive(true);
                }
                'm' => {
                    b.multi_line(true);
                }
                's' => {
                    b.dot_matches_new_line(true);
                }
                'x' => {
                    b.ignore_whitespace(true);
                }
                _ => ok = false,
            }
        }
        let re = if ok { b.build().ok() } else { None };
        self.regexes.borrow_mut().insert(key, re.clone());
        re
    }
}

/// Variable scope: aliases bound to values, with the enclosing query's scope as parent.
pub(crate) struct Env<'a> {
    pub parent: Option<&'a Env<'a>>,
    pub aliases: &'a [String],
    pub vals: &'a [Val],
}

impl<'a> Env<'a> {
    pub fn empty() -> Env<'static> {
        Env { parent: None, aliases: &[], vals: &[] }
    }
    pub fn lookup(&self, name: &str) -> Option<&Val> {
        let n = self.vals.len().min(self.aliases.len());
        for i in (0..n).rev() {
            if self.aliases[i] == name {
                return Some(&self.vals[i]);
            }
        }
        self.parent.and_then(|p| p.lookup(name))
    }
}

/// The aggregates of one group, already computed (by node: its address in the syntax tree).
pub(crate) struct GroupCtx<'a> {
    pub results: &'a HashMap<usize, Val>,
}

/// Largest total size (`Val::est_bytes`, which counts a shared array once per place it appears) of the arguments of
/// one function call. A document is at most 2 MB, so only an expression that repeats the same big value many times
/// gets past it (`TOSTRING([c.s, c.s, ... 20000 times])`).
pub(crate) const MAX_CALL_BYTES: usize = 48 * 1024 * 1024;

/// Largest value a function may build (a Cosmos document is at most 2 MB; a string or array in a result
/// cannot be larger than what could be stored).
pub(crate) const MAX_VALUE_BYTES: usize = 4 * 1024 * 1024;

fn literal(l: &Literal) -> Val {
    match l {
        Literal::Undefined => Val::Undefined,
        Literal::Null => Val::Null,
        Literal::Bool(b) => Val::Bool(*b),
        Literal::Int(i) => Val::int(*i),
        Literal::Float(f) => Val::num(*f),
        Literal::Str(s) => Val::str(s),
    }
}

impl Ctx {
    pub fn eval(&self, e: &Expr, env: &Env, grp: Option<&GroupCtx>) -> Result<Val, SqlError> {
        match &e.kind {
            ExprKind::Literal(l) => Ok(literal(l)),
            ExprKind::Param(name) => match self.params.get(name) {
                Some(v) => Ok(v.clone()),
                None => Err(SqlError::eval(format!("Parameter '@{name}' is not defined."))),
            },
            ExprKind::Ident(name) => match env.lookup(name) {
                Some(v) => Ok(v.forced()),
                None => Err(SqlError::semantic("SC2001", format!("Identifier '{name}' could not be resolved."), e.span.start, e.span.end)),
            },
            ExprKind::Property(b, n) => {
                if let Some(found) = self.nav_root(e, env, grp)? {
                    return Ok(found.map(Val::from_json).unwrap_or(Val::Undefined));
                }
                Ok(self.eval(b, env, grp)?.get_prop(n))
            }
            ExprKind::Index(b, i) => {
                if let Some(found) = self.nav_root(e, env, grp)? {
                    return Ok(found.map(Val::from_json).unwrap_or(Val::Undefined));
                }
                let base = self.eval(b, env, grp)?;
                let idx = self.eval(i, env, grp)?;
                Ok(base.get_index(&idx))
            }
            ExprKind::Unary(op, x) => {
                let v = self.eval(x, env, grp)?;
                Ok(match op {
                    UnaryOp::Not => match v {
                        Val::Bool(b) => Val::Bool(!b),
                        _ => Val::Undefined,
                    },
                    UnaryOp::Neg => ops::negate(&v),
                    UnaryOp::Plus => match v {
                        Val::Num(_) => v,
                        _ => Val::Undefined,
                    },
                    UnaryOp::BitNot => ops::bit_not(&v),
                })
            }
            ExprKind::Binary(op, a, b) => self.eval_binary(*op, a, b, env, grp),
            ExprKind::And(items) => {
                let mut undef = false;
                for it in items {
                    match self.eval(it, env, grp)? {
                        Val::Bool(false) => return Ok(Val::Bool(false)),
                        Val::Bool(true) => {}
                        _ => undef = true,
                    }
                }
                Ok(if undef { Val::Undefined } else { Val::Bool(true) })
            }
            ExprKind::Or(items) => {
                let mut undef = false;
                for it in items {
                    match self.eval(it, env, grp)? {
                        Val::Bool(true) => return Ok(Val::Bool(true)),
                        Val::Bool(false) => {}
                        _ => undef = true,
                    }
                }
                Ok(if undef { Val::Undefined } else { Val::Bool(false) })
            }
            ExprKind::Between { expr, low, high, negated } => {
                let x = self.eval(expr, env, grp)?;
                let lo = self.eval(low, env, grp)?;
                let hi = self.eval(high, env, grp)?;
                let ge = ops::tri(&ops::compare(BinaryOp::Ge, &x, &lo));
                let le = ops::tri(&ops::compare(BinaryOp::Le, &x, &hi));
                let r = ops::tri_and(ge, le);
                Ok(ops::from_tri(r.map(|b| b != *negated)))
            }
            ExprKind::In { expr, list, negated } => {
                let x = self.eval(expr, env, grp)?;
                if x.is_undefined() {
                    return Ok(Val::Undefined);
                }
                let mut found = false;
                for c in list {
                    let v = self.eval(c, env, grp)?;
                    if !v.is_undefined() && values_equal(&x, &v) {
                        found = true;
                        break;
                    }
                }
                Ok(Val::Bool(found != *negated))
            }
            ExprKind::Like { expr, pattern, escape, negated } => {
                let x = self.eval(expr, env, grp)?;
                let p = self.eval(pattern, env, grp)?;
                let esc = match escape {
                    None => None,
                    Some(e) => match self.eval(e, env, grp)? {
                        Val::Str(s) if s.chars().count() == 1 => s.chars().next(),
                        _ => return Ok(Val::Undefined),
                    },
                };
                Ok(match (&x, &p) {
                    (Val::Str(s), Val::Str(p)) => Val::Bool(crate::like::like(s, p, esc) != *negated),
                    _ => Val::Undefined,
                })
            }
            ExprKind::Ternary(c, a, b) => match self.eval(c, env, grp)? {
                Val::Undefined => Ok(Val::Undefined),
                Val::Bool(true) => self.eval(a, env, grp),
                _ => self.eval(b, env, grp),
            },
            ExprKind::Call { func, args, .. } => {
                let mut vals = Vec::with_capacity(args.len());
                let mut size = 0usize;
                for a in args {
                    let v = self.eval(a, env, grp)?;
                    size = size.saturating_add(v.est_bytes(MAX_CALL_BYTES));
                    if size > MAX_CALL_BYTES {
                        return Err(SqlError::limit(format!(
                            "The arguments of {} are larger than {} MB together, which is more than a document can hold.",
                            func.0.name,
                            MAX_CALL_BYTES / (1024 * 1024)
                        )));
                    }
                    vals.push(v);
                }
                let r = crate::functions::call(func.0, self, &vals);
                if self.overflow.replace(false) {
                    return Err(SqlError::limit(format!(
                        "{} would build a value larger than {} MB, which is more than a document can hold.",
                        func.0.name,
                        MAX_VALUE_BYTES / (1024 * 1024)
                    )));
                }
                Ok(r)
            }
            ExprKind::Udf { name, args } => {
                let mut vals: Vec<Option<Value>> = Vec::with_capacity(args.len());
                let mut size = 0usize;
                for a in args {
                    let v = self.eval(a, env, grp)?;
                    size = size.saturating_add(v.est_bytes(MAX_CALL_BYTES));
                    if size > MAX_CALL_BYTES {
                        return Err(SqlError::limit(format!(
                            "The arguments of udf.{name} are larger than {} MB together, which is more than a document can hold.",
                            MAX_CALL_BYTES / (1024 * 1024)
                        )));
                    }
                    vals.push(v.to_json());
                }
                let Some(host) = &self.udf else {
                    // the span is `udf.name`, as it was when the parser refused it
                    return Err(SqlError::not_supported(
                        &format!("User-defined function 'udf.{name}'"),
                        e.span.start,
                        e.span.start + 4 + name.chars().count(),
                    ));
                };
                match host.call(name, &vals).map_err(SqlError::udf)? {
                    Some(v) => Ok(Val::from_json(&v)),
                    None => Ok(Val::Undefined),
                }
            }
            ExprKind::Aggregate { kind, .. } => match grp {
                None => Err(SqlError::semantic(
                    "SC2102",
                    format!("Aggregate function {} is not allowed here.", kind.sql_name()),
                    e.span.start,
                    e.span.end,
                )),
                Some(g) => match g.results.get(&(e as *const Expr as usize)) {
                    Some(v) => Ok(v.clone()),
                    None => Err(SqlError::eval("An aggregate function was not prepared for this query.".to_string())),
                },
            },
            ExprKind::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                let mut size = 0usize;
                for it in items {
                    let v = self.eval(it, env, grp)?;
                    if !v.is_undefined() {
                        size = size.saturating_add(v.est_bytes(self.bytes_left()));
                        self.check_room(size)?;
                        out.push(v);
                    }
                }
                Ok(Val::arr(out))
            }
            ExprKind::Object(pairs) => {
                let mut out = Vec::with_capacity(pairs.len());
                let mut size = 0usize;
                for (k, v) in pairs {
                    let v = self.eval(v, env, grp)?;
                    size = size.saturating_add(v.est_bytes(self.bytes_left()));
                    self.check_room(size)?;
                    out.push((std::sync::Arc::from(k.as_str()), v));
                }
                Ok(Val::object(out))
            }
            ExprKind::Subquery(q) => {
                // only the first row is used: the query may stop after it
                let rows = query::run(self, q, query::Scope::Correlated(env), Some(1))?;
                Ok(rows.into_iter().next().unwrap_or(Val::Undefined))
            }
            ExprKind::Exists(q) => {
                let rows = query::run(self, q, query::Scope::Correlated(env), Some(1))?;
                Ok(Val::Bool(!rows.is_empty()))
            }
            ExprKind::ArraySubquery(q) => Ok(Val::arr(query::run(self, q, query::Scope::Correlated(env), None)?)),
        }
    }

    /// A property path (`c.a.b[0]`) whose root alias is a shared document: read it straight from the JSON and
    /// convert only the value it ends at. `None` when the path does not start at such an alias (then the
    /// caller evaluates it the ordinary way); `Some(None)` is undefined.
    fn nav_root<'v>(&self, e: &Expr, env: &'v Env<'_>, grp: Option<&GroupCtx>) -> Result<Option<Option<&'v Value>>, SqlError> {
        let mut cur = e;
        let name = loop {
            match &cur.kind {
                ExprKind::Property(b, _) | ExprKind::Index(b, _) => cur = b,
                ExprKind::Ident(n) => break n,
                _ => return Ok(None),
            }
        };
        let Some(Val::Root(doc)) = env.lookup(name) else { return Ok(None) };
        let mut steps: Vec<&Expr> = Vec::new();
        let mut cur = e;
        while let ExprKind::Property(b, _) | ExprKind::Index(b, _) = &cur.kind {
            steps.push(cur);
            cur = b;
        }
        let mut v: Option<&'v Value> = Some(&**doc);
        for step in steps.iter().rev() {
            v = match &step.kind {
                ExprKind::Property(_, n) => v.and_then(|x| x.as_object()).and_then(|m| m.get(n.as_str())),
                ExprKind::Index(_, i) => {
                    let idx = self.eval(i, env, grp)?;
                    v.and_then(|x| json_index(x, &idx))
                }
                _ => None,
            };
        }
        Ok(Some(v))
    }

    fn eval_binary(&self, op: BinaryOp, a: &Expr, b: &Expr, env: &Env, grp: Option<&GroupCtx>) -> Result<Val, SqlError> {
        use BinaryOp::*;
        let x = self.eval(a, env, grp)?;
        if op == Coalesce {
            return if x.is_undefined() { self.eval(b, env, grp) } else { Ok(x) };
        }
        let y = self.eval(b, env, grp)?;
        if x.is_undefined() || y.is_undefined() {
            return Ok(Val::Undefined);
        }
        if op == Concat {
            if let (Val::Str(p), Val::Str(q)) = (&x, &y) {
                if p.len() + q.len() > MAX_VALUE_BYTES {
                    return Err(SqlError::limit(format!(
                        "The result of || would be larger than {} MB, which is more than a document can hold.",
                        MAX_VALUE_BYTES / (1024 * 1024)
                    )));
                }
            }
        }
        Ok(match op {
            Eq | Ne | Lt | Le | Gt | Ge => ops::compare(op, &x, &y),
            Add | Sub | Mul | Div | Mod => ops::arith(op, &x, &y),
            BitAnd | BitOr | BitXor | Shl | Shr | UShr => ops::bitwise(op, &x, &y),
            Concat => ops::concat(&x, &y),
            Coalesce => Val::Undefined,
        })
    }
}
