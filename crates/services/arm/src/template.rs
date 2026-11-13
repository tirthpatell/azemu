//! ARM template expressions: `"[concat(parameters('a'), '-x')]"`.
//!
//! The language is the one in Microsoft's "ARM template functions" documentation. Built:
//! parameters, variables, string functions (concat, format, toLower, toUpper, substring,
//! replace, split, trim, contains, startsWith, endsWith, indexOf, length, string, base64, padLeft),
//! logic (if, equals, not, and, or, coalesce, empty, less/greater), numbers (int, add, sub,
//! mul, div, mod, min, max, range), arrays and objects (createArray, createObject, union,
//! first, last, take, skip, json), resource functions
//! (resourceGroup, subscription, tenant, deployment, resourceId, subscriptionResourceId,
//! reference), uniqueString, guid, copyIndex. Not built (they answer a clear error): list*
//! (listKeys and the like), utcNow, dateTimeAdd, environment, pickZones, and the user defined
//! and lambda functions.
//!
//! Everything a template can make big or deep is limited (`limits.rs`): how deep expressions and values nest, how
//! large one value is, how much work a request may do in total, `range` and `copy` counts, integer arithmetic is
//! checked. A broken limit is `EvalError::Invalid`: the request answers `InvalidTemplate` (400). Each variable is
//! evaluated once per template (and copy index) and then reused.

use std::cell::Cell;
use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::limits::{self, Budget, Memo};
use crate::template_parse::{Ast, Parser};

pub enum EvalError {
    Msg(String),
    /// A `reference()` to something that is not deployed yet: try again after the others.
    Defer(String),
    /// The template breaks a limit or does arithmetic that cannot be done (too deep, too big, too much work,
    /// integer overflow, division by zero). The whole request is `InvalidTemplate`, not a failed resource.
    Invalid(String),
}

impl EvalError {
    pub fn text(&self) -> String {
        match self {
            EvalError::Msg(m) | EvalError::Defer(m) | EvalError::Invalid(m) => m.clone(),
        }
    }
}

fn err<T>(m: impl Into<String>) -> Result<T, EvalError> {
    Err(EvalError::Msg(m.into()))
}

fn invalid<T>(m: impl Into<String>) -> Result<T, EvalError> {
    Err(EvalError::Invalid(m.into()))
}

pub type Lookup<'a> = &'a dyn Fn(&str, bool) -> Result<Value, EvalError>;

pub struct Env<'a> {
    pub params: &'a Map<String, Value>,
    /// The raw `variables` object of the template.
    pub variables: &'a Map<String, Value>,
    pub sub: &'a str,
    pub rg: &'a str,
    pub rg_info: &'a Value,
    pub sub_info: &'a Value,
    pub tenant: &'a str,
    pub deployment: &'a Value,
    /// Active `copy` loops, innermost last: (name, index).
    pub copy: Vec<(String, i64)>,
    pub lookup: Lookup<'a>,
    /// Work counters of the whole request (nested deployments share it).
    pub budget: Arc<Budget>,
    /// Variables of this template that are already evaluated.
    pub memo: Arc<Memo>,
    depth: Cell<usize>,
}

impl<'a> Env<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        params: &'a Map<String, Value>,
        variables: &'a Map<String, Value>,
        sub: &'a str,
        rg: &'a str,
        rg_info: &'a Value,
        sub_info: &'a Value,
        tenant: &'a str,
        deployment: &'a Value,
        lookup: Lookup<'a>,
    ) -> Env<'a> {
        Env {
            params,
            variables,
            sub,
            rg,
            rg_info,
            sub_info,
            tenant,
            deployment,
            copy: Vec::new(),
            lookup,
            budget: Arc::new(Budget::default()),
            memo: Arc::new(Memo::default()),
            depth: Cell::new(0),
        }
    }

    /// Use the counters and the variable cache of the deployment instead of fresh ones.
    pub fn with_shared(mut self, budget: Arc<Budget>, memo: Arc<Memo>) -> Env<'a> {
        self.budget = budget;
        self.memo = memo;
        self
    }

    fn charge(&self, units: u64) -> Result<(), EvalError> {
        self.budget.charge(units).map_err(EvalError::Invalid)
    }

    /// Charge for a value that was just made or copied, and refuse one that is too big or too deep.
    fn charge_value(&self, v: &Value) -> Result<(), EvalError> {
        let w = limits::weight(v).map_err(|m| EvalError::Invalid(format!("An expression made a value that is not allowed: {m}")))?;
        self.charge(w)
    }

    /// Run `f` one level deeper; refuses to go past `MAX_EVAL_DEPTH` (the thread stack is not infinite).
    fn nested<T>(&self, f: impl FnOnce() -> Result<T, EvalError>) -> Result<T, EvalError> {
        let d = self.depth.get() + 1;
        if d > limits::MAX_EVAL_DEPTH {
            return invalid(format!(
                "The template nests expressions, variables and values more than {} levels deep.",
                limits::MAX_EVAL_DEPTH
            ));
        }
        self.depth.set(d);
        let r = f();
        self.depth.set(d - 1);
        r
    }
}

/// Is this string an expression? `[[` at the start is an escaped literal `[`.
fn expression_body(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('[') && t.ends_with(']') && !t.starts_with("[[") {
        Some(&t[1..t.len() - 1])
    } else {
        None
    }
}

/// Evaluate every expression inside a JSON value.
pub fn eval_value(v: &Value, env: &Env<'_>) -> Result<Value, EvalError> {
    env.nested(|| eval_value_inner(v, env))
}

fn eval_value_inner(v: &Value, env: &Env<'_>) -> Result<Value, EvalError> {
    match v {
        Value::String(s) => {
            env.charge(16 + s.len() as u64)?;
            if let Some(body) = expression_body(s) {
                eval_expr(body, env)
            } else if s.starts_with("[[") {
                Ok(Value::String(s[1..].to_string()))
            } else {
                Ok(v.clone())
            }
        }
        Value::Array(a) => {
            env.charge(16)?;
            a.iter().map(|x| eval_value(x, env)).collect::<Result<Vec<_>, _>>().map(Value::Array)
        }
        Value::Object(o) => {
            env.charge(16)?;
            // `copy` loops in properties: [{"name": "x", "count": 3, "input": ...}] produce arrays.
            let mut out = Map::new();
            for (k, x) in o {
                if k == "copy" {
                    if let Value::Array(loops) = x {
                        for l in loops {
                            let name = eval_value(l.get("name").unwrap_or(&Value::Null), env)?;
                            let count = eval_value(l.get("count").unwrap_or(&Value::Null), env)?.as_i64().unwrap_or(0);
                            if !(0..=limits::MAX_COPY_PROPERTIES).contains(&count) {
                                return invalid(format!(
                                    "The copy count {count} is not valid: it must be between 0 and {}.",
                                    limits::MAX_COPY_PROPERTIES
                                ));
                            }
                            let name = name.as_str().unwrap_or("").to_string();
                            let mut items = Vec::new();
                            let mut size = 0u64;
                            for i in 0..count {
                                let mut inner = clone_env(env);
                                inner.copy.push((name.clone(), i));
                                let item = eval_value(l.get("input").unwrap_or(&Value::Null), &inner)?;
                                size += limits::weight(&item)
                                    .map_err(|m| EvalError::Invalid(format!("A copy loop made a value that is not allowed: {m}")))?;
                                if size > limits::MAX_VALUE_BYTES {
                                    return invalid(format!(
                                        "The copy loop '{name}' makes more than {} MB.",
                                        limits::MAX_VALUE_BYTES / (1024 * 1024)
                                    ));
                                }
                                items.push(item);
                            }
                            out.insert(name, Value::Array(items));
                        }
                        continue;
                    }
                }
                out.insert(k.clone(), eval_value(x, env)?);
            }
            Ok(Value::Object(out))
        }
        _ => {
            env.charge(16)?;
            Ok(v.clone())
        }
    }
}

fn clone_env<'a>(e: &Env<'a>) -> Env<'a> {
    Env {
        params: e.params,
        variables: e.variables,
        sub: e.sub,
        rg: e.rg,
        rg_info: e.rg_info,
        sub_info: e.sub_info,
        tenant: e.tenant,
        deployment: e.deployment,
        copy: e.copy.clone(),
        lookup: e.lookup,
        budget: e.budget.clone(),
        memo: e.memo.clone(),
        depth: Cell::new(e.depth.get()),
    }
}

pub fn eval_expr(body: &str, env: &Env<'_>) -> Result<Value, EvalError> {
    let chars = body.chars().count();
    if chars > limits::MAX_EXPRESSION_CHARS {
        return invalid(format!(
            "Unable to parse the template expression: it has {chars} characters and the limit is {}.",
            limits::MAX_EXPRESSION_CHARS
        ));
    }
    env.charge(body.len() as u64)?;
    let mut p = Parser::new(body);
    let ast = match p.expr() {
        Ok(a) => a,
        Err(e) if e.starts_with(limits::TOO_DEEP) => return invalid(format!("Unable to parse the template expression: {e}.")),
        Err(e) => return err(format!("Unable to parse the template expression '[{body}]': {e}.")),
    };
    if p.peek().is_some() {
        return err(format!("Unable to parse the template expression '[{body}]': unexpected text after the expression."));
    }
    eval(&ast, env)
}

// ---- evaluation --------------------------------------------------------------

fn eval(ast: &Ast, env: &Env<'_>) -> Result<Value, EvalError> {
    match ast {
        Ast::Lit(v) => {
            env.charge(16 + v.as_str().map(|t| t.len() as u64).unwrap_or(0))?;
            Ok(v.clone())
        }
        Ast::Prop(base, name) => {
            let b = env.nested(|| eval(base, env))?;
            match b.as_object().and_then(|o| o.iter().find(|(k, _)| k.eq_ignore_ascii_case(name))) {
                Some((_, v)) => {
                    env.charge_value(v)?;
                    Ok(v.clone())
                }
                None => err(format!(
                    "The language expression property '{name}' doesn't exist, available properties are '{}'.",
                    b.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default()
                )),
            }
        }
        Ast::Index(base, idx) => {
            let b = env.nested(|| eval(base, env))?;
            let i = env.nested(|| eval(idx, env))?;
            let found = match (&b, &i) {
                (Value::Array(a), Value::Number(n)) => n
                    .as_i64()
                    .and_then(|n| usize::try_from(n).ok())
                    .and_then(|n| a.get(n))
                    .cloned()
                    .ok_or_else(|| EvalError::Msg(format!("The index {n} is out of range."))),
                (Value::Object(o), Value::String(k)) => o
                    .iter()
                    .find(|(x, _)| x.eq_ignore_ascii_case(k))
                    .map(|(_, v)| v.clone())
                    .ok_or_else(|| EvalError::Msg(format!("The language expression property '{k}' doesn't exist."))),
                _ => err("The index expression is not valid for this value."),
            }?;
            env.charge_value(&found)?;
            Ok(found)
        }
        Ast::Call(name, args) => env.nested(|| call(name, args, env)),
    }
}

fn s(v: &Value) -> String {
    match v {
        Value::String(x) => x.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => String::new(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn int(v: &Value) -> Result<i64, EvalError> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).ok_or_else(|| EvalError::Msg("not an integer".into())),
        Value::String(t) => t.trim().parse().map_err(|_| EvalError::Msg(format!("'{t}' is not an integer"))),
        _ => err("An integer was expected."),
    }
}

fn truthy(v: &Value) -> Result<bool, EvalError> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::String(t) if t.eq_ignore_ascii_case("true") => Ok(true),
        Value::String(t) if t.eq_ignore_ascii_case("false") => Ok(false),
        _ => err("A boolean was expected."),
    }
}

fn eq_values(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.eq_ignore_ascii_case(y),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| eq_values(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).map(|w| eq_values(v, w)).unwrap_or(false))
        }
        _ => a == b,
    }
}

fn cmp(a: &Value, b: &Value) -> Result<std::cmp::Ordering, EvalError> {
    match (a, b) {
        (Value::Number(_), Value::Number(_)) => Ok(a.as_f64().partial_cmp(&b.as_f64()).unwrap_or(std::cmp::Ordering::Equal)),
        (Value::String(x), Value::String(y)) => Ok(x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase())),
        _ => err("The values cannot be compared."),
    }
}

/// `resourceId` and friends: `[sub], [rg], type, name1, name2...`
fn resource_id(env: &Env<'_>, args: &[Value], subscription_level: bool) -> Result<Value, EvalError> {
    let strs: Vec<String> = args.iter().map(s).collect();
    // The type is the first argument that contains a '/' and starts with a namespace.
    let ty_at = strs
        .iter()
        .position(|a| a.contains('/') && a.contains('.'))
        .ok_or_else(|| EvalError::Msg("resourceId needs a resource type such as 'Microsoft.Storage/storageAccounts'.".into()))?;
    let ty = &strs[ty_at];
    let names = &strs[ty_at + 1..];
    let segs = ty.split('/').count() - 1;
    if names.len() != segs {
        return err(format!("The resource type '{ty}' needs {segs} name segment(s) but {} were given.", names.len()));
    }
    let before = &strs[..ty_at];
    let (sub, rg) = match before.len() {
        0 => (env.sub.to_string(), if subscription_level { String::new() } else { env.rg.to_string() }),
        // One extra argument is the resource group for resourceId and the subscription for
        // subscriptionResourceId.
        1 if subscription_level => (before[0].clone(), String::new()),
        1 => (env.sub.to_string(), before[0].clone()),
        _ => (before[0].clone(), before[1].clone()),
    };
    let (ns, types) = ty.split_once('/').unwrap_or((ty, ""));
    let mut id = format!("/subscriptions/{sub}");
    if !rg.is_empty() {
        id.push_str(&format!("/resourceGroups/{rg}"));
    }
    id.push_str(&format!("/providers/{ns}"));
    for (t, n) in types.split('/').zip(names) {
        id.push_str(&format!("/{t}/{n}"));
    }
    Ok(Value::String(id))
}

fn call(name: &str, args: &[Ast], env: &Env<'_>) -> Result<Value, EvalError> {
    let lname = name.to_ascii_lowercase();
    // Lazy functions: they evaluate (and charge for) only the arguments they use.
    match lname.as_str() {
        "if" => {
            if args.len() != 3 {
                return err("The function 'if' needs three arguments.");
            }
            let c = truthy(&eval(&args[0], env)?)?;
            return eval(&args[if c { 1 } else { 2 }], env);
        }
        "and" | "or" => {
            let want_all = lname == "and";
            for a in args {
                let v = truthy(&eval(a, env)?)?;
                if want_all && !v {
                    return Ok(json!(false));
                }
                if !want_all && v {
                    return Ok(json!(true));
                }
            }
            return Ok(json!(want_all));
        }
        "coalesce" => {
            for a in args {
                let v = eval(a, env)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            return Ok(Value::Null);
        }
        _ => {}
    }
    let mut v: Vec<Value> = Vec::with_capacity(args.len());
    for a in args {
        v.push(eval(a, env)?);
    }
    env.charge(64)?;
    // These two recurse into evaluation; they are kept out of `call_strict` so its large stack frame is not part
    // of the recursion.
    let r = match lname.as_str() {
        "variables" => variable(name, &v, env)?,
        "reference" => {
            if v.is_empty() {
                return err(format!("The function '{name}' needs at least 1 argument(s)."));
            }
            env.charge(limits::REFERENCE_COST)?;
            let full = v.iter().skip(1).any(|x| x.as_str().map(|t| t.eq_ignore_ascii_case("full")).unwrap_or(false));
            (env.lookup)(&s(&v[0]), full)?
        }
        _ => call_strict(name, &lname, v, env)?,
    };
    env.charge_value(&r)?;
    Ok(r)
}

/// `variables('name')`: evaluated once per template and copy index, then reused.
fn variable(name: &str, v: &[Value], env: &Env<'_>) -> Result<Value, EvalError> {
    if v.is_empty() {
        return err(format!("The function '{name}' needs at least 1 argument(s)."));
    }
    let n = s(&v[0]);
    let key = format!("{}|{:?}", n.to_ascii_lowercase(), env.copy);
    if let Some(hit) = env.memo.get(&key) {
        return Ok(hit);
    }
    let raw = env.variables.iter().find(|(k, _)| k.eq_ignore_ascii_case(&n)).map(|(_, x)| x);
    let Some(raw) = raw else {
        return err(format!(
            "The template variable '{n}' is not found. Please see https://aka.ms/arm-template/#variables for usage details."
        ));
    };
    if !env.memo.enter(&key) {
        return invalid(format!("The template variable '{n}' refers to itself."));
    }
    let r = eval_value(raw, env);
    env.memo.leave(&key);
    if let Ok(val) = &r {
        if let Ok(w) = limits::weight(val) {
            env.memo.put(key, val, w);
        }
    }
    r
}

fn arith(name: &str, a: Option<i64>) -> Result<Value, EvalError> {
    match a {
        Some(n) => Ok(json!(n)),
        None => invalid(format!("{name}: the result does not fit in a 64-bit integer.")),
    }
}

#[inline(never)]
fn call_strict(name: &str, lname: &str, v: Vec<Value>, env: &Env<'_>) -> Result<Value, EvalError> {
    let need = |n: usize| -> Result<(), EvalError> {
        if v.len() < n {
            err(format!("The function '{name}' needs at least {n} argument(s)."))
        } else {
            Ok(())
        }
    };
    match lname {
        "parameters" => {
            need(1)?;
            let n = s(&v[0]);
            env.params.iter().find(|(k, _)| k.eq_ignore_ascii_case(&n)).map(|(_, x)| x.clone()).ok_or_else(|| EvalError::Msg(format!("The template parameter '{n}' is not found. Please see https://aka.ms/arm-template/#parameters for usage details.")))
        }
        "concat" => {
            if !v.is_empty() && v.iter().all(Value::is_array) {
                Ok(Value::Array(v.iter().flat_map(|x| x.as_array().cloned().unwrap_or_default()).collect()))
            } else {
                Ok(Value::String(v.iter().map(s).collect()))
            }
        }
        "format" => {
            need(1)?;
            let f = s(&v[0]);
            let mut out = String::new();
            let b = f.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'{' && b.get(i + 1) == Some(&b'{') {
                    out.push('{');
                    i += 2;
                } else if b[i] == b'}' && b.get(i + 1) == Some(&b'}') {
                    out.push('}');
                    i += 2;
                } else if b[i] == b'{' {
                    let end = f[i..].find('}').ok_or_else(|| EvalError::Msg("format: unmatched '{'".into()))? + i;
                    let spec = &f[i + 1..end];
                    let (idx, fmt) = spec.split_once(':').unwrap_or((spec, ""));
                    let n: usize = idx.trim().parse().map_err(|_| EvalError::Msg(format!("format: '{idx}' is not an index")))?;
                    let arg = n.checked_add(1).and_then(|k| v.get(k)).ok_or_else(|| EvalError::Msg(format!("format: no argument {n}")))?;
                    out.push_str(&format_one(arg, fmt)?);
                    i = end + 1;
                } else {
                    let ch = f[i..].chars().next().unwrap_or(' ');
                    out.push(ch);
                    i += ch.len_utf8();
                }
                if out.len() as u64 > limits::MAX_VALUE_BYTES {
                    return invalid(format!("format: the result is larger than {} MB.", limits::MAX_VALUE_BYTES / (1024 * 1024)));
                }
            }
            Ok(Value::String(out))
        }
        "padleft" => {
            need(2)?;
            let t = s(&v[0]);
            let total = int(&v[1])?;
            if total > limits::MAX_VALUE_BYTES as i64 {
                return invalid(format!("padLeft: the total length {total} is larger than {} MB.", limits::MAX_VALUE_BYTES / (1024 * 1024)));
            }
            let pad = if v.len() > 2 { s(&v[2]).chars().next().unwrap_or(' ') } else { ' ' };
            let have = t.chars().count() as i64;
            let missing = (total - have).max(0) as usize;
            Ok(Value::String(std::iter::repeat_n(pad, missing).chain(t.chars()).collect()))
        }
        "tolower" => need(1).map(|_| Value::String(s(&v[0]).to_lowercase())),
        "toupper" => need(1).map(|_| Value::String(s(&v[0]).to_uppercase())),
        "trim" => need(1).map(|_| Value::String(s(&v[0]).trim().to_string())),
        "string" => need(1).map(|_| Value::String(if v[0].is_string() { s(&v[0]) } else if v[0].is_boolean() { s(&v[0]).to_lowercase() } else { v[0].to_string() })),
        "int" => need(1).and_then(|_| int(&v[0]).map(|n| json!(n))),
        "bool" => need(1).and_then(|_| truthy(&v[0]).map(|b| json!(b))),
        "length" => {
            need(1)?;
            Ok(json!(match &v[0] {
                Value::String(x) => x.chars().count(),
                Value::Array(a) => a.len(),
                Value::Object(o) => o.len(),
                _ => return err("length needs a string, array or object."),
            }))
        }
        "substring" => {
            need(2)?;
            let t: Vec<char> = s(&v[0]).chars().collect();
            let start = int(&v[1])?.max(0) as usize;
            let len = if v.len() > 2 { int(&v[2])?.max(0) as usize } else { t.len().saturating_sub(start) };
            if start > t.len() || start.checked_add(len).map(|end| end > t.len()).unwrap_or(true) {
                return err("substring: the index and length are out of range.");
            }
            Ok(Value::String(t[start..start + len].iter().collect()))
        }
        "replace" => {
            need(3)?;
            let (text, from, to) = (s(&v[0]), s(&v[1]), s(&v[2]));
            if from.is_empty() {
                return err("replace: the string to replace must not be empty.");
            }
            // Size of the answer, before it is built: a long `to` for every match could be far larger than any limit.
            let hits = text.matches(from.as_str()).count() as u64;
            let size = (text.len() as u64).saturating_sub(hits * from.len() as u64).saturating_add(hits.saturating_mul(to.len() as u64));
            if size > limits::MAX_VALUE_BYTES {
                return invalid(format!("replace: the result would be larger than {} MB.", limits::MAX_VALUE_BYTES / (1024 * 1024)));
            }
            Ok(Value::String(text.replace(&from, &to)))
        }
        "split" => {
            need(2)?;
            let text = s(&v[0]);
            let seps: Vec<String> = match &v[1] {
                Value::Array(a) => a.iter().map(s).collect(),
                other => vec![s(other)],
            };
            let mut parts = vec![text];
            for sep in seps.iter().filter(|x| !x.is_empty()) {
                let mut next: Vec<String> = Vec::new();
                let mut size = 0u64;
                for p in &parts {
                    for piece in p.split(sep.as_str()) {
                        size += 16 + piece.len() as u64;
                        if size > limits::MAX_VALUE_BYTES {
                            return invalid(format!("split: the result is larger than {} MB.", limits::MAX_VALUE_BYTES / (1024 * 1024)));
                        }
                        next.push(piece.to_string());
                    }
                }
                parts = next;
            }
            Ok(Value::Array(parts.into_iter().map(Value::String).collect()))
        }
        "contains" => {
            need(2)?;
            Ok(json!(match &v[0] {
                Value::String(t) => t.to_lowercase().contains(&s(&v[1]).to_lowercase()),
                Value::Array(a) => a.iter().any(|x| eq_values(x, &v[1])),
                Value::Object(o) => o.keys().any(|k| k.eq_ignore_ascii_case(&s(&v[1]))),
                _ => false,
            }))
        }
        "startswith" => need(2).map(|_| json!(s(&v[0]).to_lowercase().starts_with(&s(&v[1]).to_lowercase()))),
        "endswith" => need(2).map(|_| json!(s(&v[0]).to_lowercase().ends_with(&s(&v[1]).to_lowercase()))),
        "indexof" => need(2).map(|_| json!(s(&v[0]).to_lowercase().find(&s(&v[1]).to_lowercase()).map(|i| i as i64).unwrap_or(-1))),
        "base64" => {
            use base64::Engine;
            need(1).map(|_| Value::String(base64::engine::general_purpose::STANDARD.encode(s(&v[0]))))
        }
        "base64tostring" => {
            use base64::Engine;
            need(1)?;
            let bytes = base64::engine::general_purpose::STANDARD.decode(s(&v[0])).map_err(|_| EvalError::Msg("base64ToString: not valid base64".into()))?;
            Ok(Value::String(String::from_utf8_lossy(&bytes).to_string()))
        }
        "uniquestring" => {
            need(1)?;
            Ok(Value::String(crate::util::short_hash(&v.iter().map(s).collect::<Vec<_>>().join("-"), 13)))
        }
        "guid" => {
            need(1)?;
            let h = crate::util::short_hash(&v.iter().map(s).collect::<Vec<_>>().join("-"), 32);
            let hex: String = crate::util::derive_key(&h, 0).bytes().filter(|b| b.is_ascii_alphanumeric()).take(32).map(|b| format!("{:x}", b % 16)).collect();
            Ok(Value::String(format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])))
        }
        "equals" => need(2).map(|_| json!(eq_values(&v[0], &v[1]))),
        "not" => need(1).and_then(|_| truthy(&v[0]).map(|b| json!(!b))),
        "empty" => {
            need(1)?;
            Ok(json!(match &v[0] {
                Value::Null => true,
                Value::String(t) => t.is_empty(),
                Value::Array(a) => a.is_empty(),
                Value::Object(o) => o.is_empty(),
                _ => false,
            }))
        }
        "less" => need(2).and_then(|_| cmp(&v[0], &v[1]).map(|o| json!(o.is_lt()))),
        "lessorequals" => need(2).and_then(|_| cmp(&v[0], &v[1]).map(|o| json!(o.is_le()))),
        "greater" => need(2).and_then(|_| cmp(&v[0], &v[1]).map(|o| json!(o.is_gt()))),
        "greaterorequals" => need(2).and_then(|_| cmp(&v[0], &v[1]).map(|o| json!(o.is_ge()))),
        "add" => {
            need(2)?;
            arith(name, int(&v[0])?.checked_add(int(&v[1])?))
        }
        "sub" => {
            need(2)?;
            arith(name, int(&v[0])?.checked_sub(int(&v[1])?))
        }
        "mul" => {
            need(2)?;
            arith(name, int(&v[0])?.checked_mul(int(&v[1])?))
        }
        "div" => {
            need(2)?;
            let d = int(&v[1])?;
            if d == 0 {
                return invalid("div: division by zero.");
            }
            arith(name, int(&v[0])?.checked_div(d))
        }
        "mod" => {
            need(2)?;
            let d = int(&v[1])?;
            if d == 0 {
                return invalid("mod: division by zero.");
            }
            arith(name, int(&v[0])?.checked_rem(d))
        }
        "min" | "max" => {
            let nums: Vec<i64> = if v.len() == 1 && v[0].is_array() { v[0].as_array().map(|a| a.iter().filter_map(|x| int(x).ok()).collect()).unwrap_or_default() } else { v.iter().filter_map(|x| int(x).ok()).collect() };
            let r = if lname == "min" { nums.iter().min() } else { nums.iter().max() };
            r.map(|n| json!(n)).ok_or_else(|| EvalError::Msg(format!("{name}: no numbers given.")))
        }
        "range" => {
            need(2)?;
            let start = int(&v[0])?;
            let n = int(&v[1])?;
            if !(0..=limits::MAX_RANGE_COUNT).contains(&n) {
                return invalid(format!("range: the count {n} is not valid; it must be a non-negative integer up to {}.", limits::MAX_RANGE_COUNT));
            }
            match start.checked_add(n) {
                Some(end) if end <= limits::MAX_RANGE_END => {}
                _ => return invalid(format!("range: the sum of the start index {start} and the count {n} must not be greater than {}.", limits::MAX_RANGE_END)),
            }
            Ok(Value::Array((0..n).map(|i| json!(start + i)).collect()))
        }
        "createarray" => Ok(Value::Array(v)),
        "createobject" => {
            let mut o = Map::new();
            for pair in v.chunks(2) {
                if pair.len() == 2 {
                    o.insert(s(&pair[0]), pair[1].clone());
                }
            }
            Ok(Value::Object(o))
        }
        "json" => need(1).and_then(|_| serde_json::from_str(&s(&v[0])).map_err(|e| EvalError::Msg(format!("json: {e}")))),
        "union" => {
            need(2)?;
            if v.iter().all(Value::is_array) {
                // Equal values (strings compared without case) are kept once; a set of canonical texts keeps it linear.
                let mut seen = std::collections::HashSet::new();
                let mut out: Vec<Value> = Vec::new();
                for a in v.iter().flat_map(|x| x.as_array().into_iter().flatten()) {
                    if seen.insert(limits::canonical(a)) {
                        out.push(a.clone());
                    }
                }
                Ok(Value::Array(out))
            } else {
                let mut out = Map::new();
                for o in &v {
                    if let Some(m) = o.as_object() {
                        for (k, x) in m {
                            out.insert(k.clone(), x.clone());
                        }
                    }
                }
                Ok(Value::Object(out))
            }
        }
        "first" => need(1).map(|_| match &v[0] {
            Value::Array(a) => a.first().cloned().unwrap_or(Value::Null),
            Value::String(t) => t.chars().next().map(|c| Value::String(c.to_string())).unwrap_or(Value::Null),
            _ => Value::Null,
        }),
        "last" => need(1).map(|_| match &v[0] {
            Value::Array(a) => a.last().cloned().unwrap_or(Value::Null),
            Value::String(t) => t.chars().last().map(|c| Value::String(c.to_string())).unwrap_or(Value::Null),
            _ => Value::Null,
        }),
        "take" | "skip" => {
            need(2)?;
            let n = usize::try_from(int(&v[1])?.max(0)).unwrap_or(usize::MAX);
            let take = lname == "take";
            Ok(match &v[0] {
                Value::Array(a) => Value::Array(if take { a.iter().take(n).cloned().collect() } else { a.iter().skip(n).cloned().collect() }),
                other => {
                    let t = s(other);
                    Value::String(if take { t.chars().take(n).collect() } else { t.chars().skip(n).collect() })
                }
            })
        }
        "copyindex" => {
            let (name_arg, offset) = match v.len() {
                0 => (None, 0),
                1 if v[0].is_number() => (None, int(&v[0])?),
                1 => (Some(s(&v[0])), 0),
                _ => (Some(s(&v[0])), int(&v[1])?),
            };
            let cur = match &name_arg {
                Some(n) => env.copy.iter().rev().find(|(c, _)| c.eq_ignore_ascii_case(n)),
                None => env.copy.last(),
            };
            match cur {
                Some((_, i)) => arith(name, i.checked_add(offset)),
                None => err("copyIndex() was used outside a copy loop."),
            }
        }
        "resourcegroup" => Ok(env.rg_info.clone()),
        "subscription" => Ok(env.sub_info.clone()),
        "tenant" => Ok(json!({"tenantId": env.tenant, "id": format!("/tenants/{}", env.tenant)})),
        "deployment" => Ok(env.deployment.clone()),
        "resourceid" => resource_id(env, &v, false),
        "subscriptionresourceid" => resource_id(env, &v, true),
        other if other.starts_with("list") => err(format!("The template function '{name}' is not built in azemu (the list* functions would need to run a provider action while the template is evaluated).")),
        _ => err(format!("The template function '{name}' is not built in azemu.")),
    }
}

fn format_one(v: &Value, fmt: &str) -> Result<String, EvalError> {
    if let (Some(f), Some(spec)) = (fmt.chars().next(), fmt.get(1..)) {
        let digits: usize = match spec.parse() {
            Ok(d) => d,
            // digits that do not fit a number are certainly too wide
            Err(_) if !spec.is_empty() && spec.bytes().all(|b| b.is_ascii_digit()) => usize::MAX,
            Err(_) => 0,
        };
        if digits > limits::MAX_FORMAT_WIDTH {
            return invalid(format!("format: the width or precision {digits} is larger than {}.", limits::MAX_FORMAT_WIDTH));
        }
        match (f.to_ascii_uppercase(), v) {
            ('N' | 'F', Value::Number(n)) => {
                return Ok(format!("{:.*}", if spec.is_empty() { 2 } else { digits }, n.as_f64().unwrap_or(0.0)))
            }
            ('D', Value::Number(n)) => return Ok(format!("{:0>width$}", n.as_i64().unwrap_or(0), width = digits)),
            ('X', Value::Number(n)) => return Ok(format!("{:X}", n.as_i64().unwrap_or(0))),
            _ => {}
        }
    }
    Ok(s(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(expr: &str, params: Value, vars: Value) -> Result<Value, String> {
        let p = params.as_object().cloned().unwrap_or_default();
        let vv = vars.as_object().cloned().unwrap_or_default();
        let rg = json!({"id": "/subscriptions/s/resourceGroups/rg", "name": "rg", "location": "eastus", "tags": {}});
        let sub = json!({"subscriptionId": "s", "id": "/subscriptions/s"});
        let dep = json!({"name": "d"});
        let look = |n: &str, _full: bool| -> Result<Value, EvalError> {
            if n == "later" {
                Err(EvalError::Defer("not yet".into()))
            } else {
                Ok(json!({"endpoint": format!("https://{n}/")}))
            }
        };
        let env = Env::new(&p, &vv, "s", "rg", &rg, &sub, "t", &dep, &look);
        eval_value(&Value::String(expr.to_string()), &env).map_err(|e| e.text())
    }

    #[test]
    fn strings_and_parameters() {
        let p = json!({"name": "abc", "n": 3});
        assert_eq!(run("[concat(parameters('name'), '-', 'x')]", p.clone(), json!({})).unwrap(), json!("abc-x"));
        assert_eq!(run("[toLower('ABC')]", p.clone(), json!({})).unwrap(), json!("abc"));
        assert_eq!(run("[format('{0}-{1}-{0}', 'a', 7)]", p.clone(), json!({})).unwrap(), json!("a-7-a"));
        assert_eq!(run("[substring('abcdef', 1, 3)]", p.clone(), json!({})).unwrap(), json!("bcd"));
        assert_eq!(run("[add(parameters('n'), 4)]", p.clone(), json!({})).unwrap(), json!(7));
        assert_eq!(run("[concat('it''s')]", p, json!({})).unwrap(), json!("it's"));
    }

    #[test]
    fn escapes_and_plain_strings() {
        assert_eq!(run("[[literal]", json!({}), json!({})).unwrap(), json!("[literal]"));
        assert_eq!(run("plain", json!({}), json!({})).unwrap(), json!("plain"));
    }

    #[test]
    fn variables_refer_to_each_other() {
        let vars = json!({"a": "[concat(parameters('p'), '1')]", "b": "[concat(variables('a'), '2')]"});
        assert_eq!(run("[variables('b')]", json!({"p": "x"}), vars).unwrap(), json!("x12"));
        let looping = json!({"a": "[variables('a')]"});
        assert!(run("[variables('a')]", json!({}), looping).is_err());
    }

    #[test]
    fn resource_functions() {
        assert_eq!(run("[resourceGroup().location]", json!({}), json!({})).unwrap(), json!("eastus"));
        assert_eq!(run("[subscription().subscriptionId]", json!({}), json!({})).unwrap(), json!("s"));
        assert_eq!(
            run("[resourceId('Microsoft.Storage/storageAccounts', 'acct')]", json!({}), json!({})).unwrap(),
            json!("/subscriptions/s/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/acct")
        );
        assert_eq!(
            run(
                "[resourceId('other-rg', 'Microsoft.Storage/storageAccounts/blobServices/containers', 'a', 'default', 'c')]",
                json!({}),
                json!({})
            )
            .unwrap(),
            json!(
                "/subscriptions/s/resourceGroups/other-rg/providers/Microsoft.Storage/storageAccounts/a/blobServices/default/containers/c"
            )
        );
        assert_eq!(run("[reference('acct').endpoint]", json!({}), json!({})).unwrap(), json!("https://acct/"));
    }

    #[test]
    fn unique_string_is_stable_and_13_long() {
        let a = run("[uniqueString(resourceGroup().id)]", json!({}), json!({})).unwrap();
        let b = run("[uniqueString(resourceGroup().id)]", json!({}), json!({})).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.as_str().unwrap().len(), 13);
    }

    #[test]
    fn logic_and_indexing() {
        assert_eq!(run("[if(equals('A','a'), 'yes', 'no')]", json!({}), json!({})).unwrap(), json!("yes"));
        assert_eq!(run("[createArray(1,2,3)[1]]", json!({}), json!({})).unwrap(), json!(2));
        assert_eq!(run("[createObject('k', 'v').k]", json!({}), json!({})).unwrap(), json!("v"));
        assert_eq!(run("[concat(createArray(1), createArray(2))]", json!({}), json!({})).unwrap(), json!([1, 2]));
    }

    #[test]
    fn errors_name_the_problem() {
        let e = run("[parameters('missing')]", json!({}), json!({})).unwrap_err();
        assert!(e.contains("missing"));
        let e = run("[listKeys('x', '2020-01-01')]", json!({}), json!({})).unwrap_err();
        assert!(e.contains("listKeys"));
        let e = run("[bogus()]", json!({}), json!({})).unwrap_err();
        assert!(e.contains("bogus"));
        assert!(run("[concat('a'", json!({}), json!({})).unwrap().is_string()); // not an expression: no closing bracket
    }

    #[test]
    fn deferred_reference() {
        assert!(matches!(
            eval_expr("reference('later')", &{
                let p = Map::new();
                let leak: &'static Map<String, Value> = Box::leak(Box::new(p));
                let rg: &'static Value = Box::leak(Box::new(json!({})));
                let look: &'static dyn Fn(&str, bool) -> Result<Value, EvalError> =
                    Box::leak(Box::new(|_: &str, _: bool| -> Result<Value, EvalError> { Err(EvalError::Defer("x".into())) }));
                Env::new(leak, leak, "s", "r", rg, rg, "t", rg, look)
            }),
            Err(EvalError::Defer(_))
        ));
    }

    // ---- limits ----

    /// A stack overflow kills the whole process. A tokio worker thread has 2 MiB (and the request handler uses some of it
    /// before it gets here); these run in 1 MiB, so the limits keep a margin of at least two.
    fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new().stack_size(1024 * 1024).spawn(f).unwrap().join().unwrap()
    }

    fn invalid(e: Result<Value, String>) -> String {
        e.expect_err("expected an error")
    }

    #[test]
    fn nested_calls_are_limited_not_a_stack_overflow() {
        on_small_stack(|| {
            for depth in [10usize, 64, 65, 200, 2000] {
                let expr = format!("[{}'a'{}]", "concat(".repeat(depth), ")".repeat(depth));
                let r = run(&expr, json!({}), json!({}));
                if depth <= 63 {
                    assert_eq!(r.unwrap(), json!("a"), "depth {depth}");
                } else if depth >= 65 {
                    assert!(invalid(r).contains("levels deep"), "depth {depth}");
                }
            }
            // a long chain of `.a.a.a` or `[0][0]` nests the same way
            let chain = format!("[createObject('a', 1){}]", ".a".repeat(5000));
            assert!(invalid(run(&chain, json!({}), json!({}))).contains("levels deep"));
            let idx = format!("[createArray(1){}]", "[0]".repeat(5000));
            assert!(invalid(run(&idx, json!({}), json!({}))).contains("levels deep"));
            // nested indexes: a[a[a[...]]]
            let idx = format!("[{}0{}]", "createArray(0)[".repeat(1500), "]".repeat(1500));
            assert!(invalid(run(&idx, json!({}), json!({}))).contains("levels deep"));
        });
    }

    #[test]
    fn the_deepest_allowed_nesting_fits_the_stack() {
        on_small_stack(|| {
            // 63 nested `if` and `concat`, and the evaluation depth cap of variables that refer to each other
            let expr = format!("[{}'a'{}]", "if(true, ".repeat(63), ", 'b')".repeat(63));
            assert_eq!(run(&expr, json!({}), json!({})).unwrap(), json!("a"));
            let mut vars = serde_json::Map::new();
            vars.insert("v0".into(), json!("x"));
            for i in 1..=200 {
                vars.insert(format!("v{i}"), json!(format!("[concat(variables('v{}'), 'x')]", i - 1)));
            }
            let r = run("[variables('v200')]", json!({}), Value::Object(vars.clone()));
            assert!(invalid(r).contains("levels deep"));
            let ok = run("[variables('v30')]", json!({}), Value::Object(vars)).unwrap();
            assert_eq!(ok.as_str().unwrap().len(), 31);
        });
    }

    #[test]
    fn variables_holding_objects_and_deep_json_fit_the_stack() {
        on_small_stack(|| {
            // v_i = {"a": "[variables('v_(i-1)')]"}: three levels of evaluation per hop
            let mut vars = serde_json::Map::new();
            vars.insert("v0".into(), json!("x"));
            for i in 1..=100 {
                vars.insert(format!("v{i}"), json!({"a": [format!("[createArray(variables('v{}'))]", i - 1)]}));
            }
            let e = invalid(run("[variables('v100')]", json!({}), Value::Object(vars.clone())));
            assert!(e.contains("levels deep"), "{e}");
            assert!(run("[variables('v12')]", json!({}), Value::Object(vars)).is_ok());
            // a JSON value 120 levels deep (what serde_json accepts) holding an expression at the bottom
            let mut v = json!("[concat('a', 'b')]");
            for _ in 0..120 {
                v = json!({"k": v});
            }
            let p = Map::new();
            let vv = Map::new();
            let rg = json!({});
            let look = |_: &str, _: bool| -> Result<Value, EvalError> { Ok(json!({})) };
            let env = Env::new(&p, &vv, "s", "rg", &rg, &rg, "t", &rg, &look);
            assert!(eval_value(&v, &env).is_ok());
            // values the evaluation builds are at most 100 deep
            let nest = "createArray(".repeat(50) + "1" + &")".repeat(50);
            let vars = json!({"a": format!("[{nest}]"), "b": "[createArray(createArray(variables('a')))]"});
            assert!(run("[createArray(variables('b'), variables('b'))]", json!({}), vars).is_ok());
        });
    }

    #[test]
    fn an_expression_has_at_most_24576_characters() {
        let long = format!("[concat('{}')]", "a".repeat(24_600));
        assert!(invalid(run(&long, json!({}), json!({}))).contains("24576"));
        let fine = format!("[concat('{}')]", "a".repeat(24_000));
        assert_eq!(run(&fine, json!({}), json!({})).unwrap().as_str().unwrap().len(), 24_000);
    }

    #[test]
    fn range_is_bounded() {
        assert_eq!(run("[length(range(0, 10000))]", json!({}), json!({})).unwrap(), json!(10_000));
        assert!(invalid(run("[range(0, 10001)]", json!({}), json!({}))).contains("10000"));
        assert!(invalid(run("[range(0, 2000000000)]", json!({}), json!({}))).contains("range"));
        assert!(invalid(run("[range(0, -1)]", json!({}), json!({}))).contains("range"));
        assert!(invalid(run("[range(2147483647, 3)]", json!({}), json!({}))).contains("2147483647"));
        assert!(invalid(run("[range(9223372036854775807, 3)]", json!({}), json!({}))).contains("range"));
        assert_eq!(run("[range(2147483645, 2)]", json!({}), json!({})).unwrap(), json!([2147483645, 2147483646]));
    }

    #[test]
    fn arithmetic_is_checked() {
        let max = i64::MAX;
        let min = i64::MIN;
        for expr in [
            format!("[add({max}, 1)]"),
            format!("[sub({}, 2)]", min + 1),
            format!("[mul({max}, 2)]"),
            format!("[div({}, -1)]", min),
            format!("[mod({}, -1)]", min),
            "[div(1, 0)]".to_string(),
            "[mod(1, 0)]".to_string(),
        ] {
            // `-9223372036854775808` does not parse as a literal in the expression language: use sub() to reach it
            let expr = expr.replace(&min.to_string(), "sub(-9223372036854775807, 1)");
            let e = invalid(run(&expr, json!({}), json!({})));
            assert!(e.contains("64-bit") || e.contains("division by zero"), "{expr}: {e}");
        }
        assert_eq!(run("[add(2, 3)]", json!({}), json!({})).unwrap(), json!(5));
        assert_eq!(run("[div(7, 2)]", json!({}), json!({})).unwrap(), json!(3));
        assert_eq!(run("[mod(7, 4)]", json!({}), json!({})).unwrap(), json!(3));
        assert!(run("[copyIndex(9223372036854775807)]", json!({}), json!({})).unwrap_err().contains("outside a copy loop"));
    }

    #[test]
    fn format_widths_and_results_are_bounded() {
        assert_eq!(run("[format('{0:D4}', 7)]", json!({}), json!({})).unwrap(), json!("0007"));
        assert!(invalid(run("[format('{0:D4000000000}', 1)]", json!({}), json!({}))).contains("format"));
        assert!(invalid(run("[format('{0:N99999999999999999999}', 1)]", json!({}), json!({}))).contains("format"));
        assert!(invalid(run("[format('{18446744073709551615}', 1)]", json!({}), json!({}))).contains("no argument"));
        // many references to a big argument
        let r = run(&format!("[format('{}', padLeft('x', 1000000, 'x'))]", "{0}".repeat(8000)), json!({}), json!({}));
        assert!(invalid(r).contains("larger"));
    }

    #[test]
    fn strings_and_arrays_made_by_other_functions_are_bounded() {
        let big = "[padLeft('x', 4000000, 'x')]";
        assert_eq!(run(big, json!({}), json!({})).unwrap().as_str().unwrap().len(), 4_000_000);
        assert!(invalid(run("[padLeft('x', 5000000, 'x')]", json!({}), json!({}))).contains("larger"));
        // replace with a long replacement for each of many matches
        let r = run("[replace(padLeft('a', 100000, 'a'), 'a', padLeft('b', 100000, 'b'))]", json!({}), json!({}));
        assert!(invalid(r).contains("larger"));
        assert!(invalid(run("[replace('abc', '', 'x')]", json!({}), json!({}))).contains("empty"));
        // substring with huge numbers does not overflow
        assert!(invalid(run("[substring('abc', 9223372036854775807, 9223372036854775807)]", json!({}), json!({}))).contains("out of range"));
        assert_eq!(run("[take('abc', 9223372036854775807)]", json!({}), json!({})).unwrap(), json!("abc"));
        assert_eq!(run("[skip(createArray(1, 2), 9223372036854775807)]", json!({}), json!({})).unwrap(), json!([]));
    }

    #[test]
    fn variables_are_evaluated_once() {
        on_small_stack(|| {
            // v_i = concat(v_(i-1), v_(i-1)): 2^i evaluations without a cache, a string of 2^i bytes
            let mut vars = serde_json::Map::new();
            vars.insert("v0".into(), json!("x"));
            for i in 1..=30 {
                vars.insert(format!("v{i}"), json!(format!("[concat(variables('v{}'), variables('v{}'))]", i - 1, i - 1)));
            }
            let started = std::time::Instant::now();
            let ok = run("[length(variables('v10'))]", json!({}), Value::Object(vars.clone())).unwrap();
            assert_eq!(ok, json!(1024));
            // 30 levels would be a string of 1 GiB: refused by the size limit after a few steps, quickly
            let e = invalid(run("[length(variables('v30'))]", json!({}), Value::Object(vars)));
            assert!(e.contains("larger") || e.contains("complex"), "{e}");
            assert!(started.elapsed().as_secs() < 10, "took {:?}", started.elapsed());
            // a variable that refers to itself, and two that refer to each other
            let looping = json!({"a": "[variables('b')]", "b": "[variables('a')]"});
            assert!(invalid(run("[variables('a')]", json!({}), looping)).contains("refers to itself"));
        });
    }

    #[test]
    fn a_variable_is_evaluated_one_time_per_use_count() {
        // The evaluation cost of n references to the same variable is n, not the size of its own definition each time.
        let vars = json!({"big": "[concat('a', 'b', 'c')]"});
        let counter = std::cell::Cell::new(0u32);
        let look = |_: &str, _: bool| -> Result<Value, EvalError> {
            counter.set(counter.get() + 1);
            Ok(json!({}))
        };
        let p = Map::new();
        let vv = vars.as_object().cloned().unwrap();
        let rg = json!({});
        let env = Env::new(&p, &vv, "s", "rg", &rg, &rg, "t", &rg, &look);
        for _ in 0..5 {
            assert_eq!(eval_expr("variables('big')", &env).ok(), Some(json!("abc")));
        }
        assert_eq!(env.memo.get("big|[]"), Some(json!("abc")));
    }

    #[test]
    fn the_work_of_one_request_is_budgeted() {
        // 10,000 numbers, referenced again and again through a variable: each use costs its size
        let vars = json!({"r": "[range(0, 10000)]"});
        let mut expr = String::from("[length(concat(");
        let parts: Vec<String> = (0..1000).map(|_| "variables('r')".to_string()).collect();
        expr.push_str(&parts.join(","));
        expr.push_str("))]");
        let started = std::time::Instant::now();
        let e = invalid(run(&expr, json!({}), vars));
        assert!(e.contains("too complex") || e.contains("larger"), "{e}");
        assert!(started.elapsed().as_secs() < 10);
    }

    #[test]
    fn union_is_not_quadratic() {
        let a = "[length(union(range(0, 10000), range(5000, 10000)))]";
        assert_eq!(run(a, json!({}), json!({})).unwrap(), json!(15_000));
        assert_eq!(run("[union(createArray('A', 'b'), createArray('a', 'c'))]", json!({}), json!({})).unwrap(), json!(["A", "b", "c"]));
        assert_eq!(run("[union(createObject('a', 1), createObject('b', 2))]", json!({}), json!({})).unwrap(), json!({"a": 1, "b": 2}));
    }

    #[test]
    fn copy_in_properties_is_bounded() {
        let env_run = |count: i64| {
            let p = Map::new();
            let v = Map::new();
            let rg = json!({});
            let look = |_: &str, _: bool| -> Result<Value, EvalError> { Ok(json!({})) };
            let env = Env::new(&p, &v, "s", "rg", &rg, &rg, "t", &rg, &look);
            eval_value(&json!({"copy": [{"name": "items", "count": count, "input": {"i": "[copyIndex()]"}}]}), &env).map_err(|e| e.text())
        };
        assert_eq!(env_run(3).unwrap()["items"].as_array().unwrap().len(), 3);
        assert!(env_run(2_000_000_000).unwrap_err().contains("copy count"));
        assert!(env_run(-1).unwrap_err().contains("copy count"));
    }
}
