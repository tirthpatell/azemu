//! ARM template expressions: `"[concat(parameters('a'), '-x')]"`.
//!
//! The language is the one in Microsoft's "ARM template functions" documentation. Built:
//! parameters, variables, string functions (concat, format, toLower, toUpper, substring,
//! replace, split, trim, contains, startsWith, endsWith, indexOf, length, string, base64, padLeft),
//! logic (if, equals, not, and, or, coalesce, empty, less/greater), numbers (int, add, sub,
//! mul, div, mod, min, max, range), arrays and objects (createArray, createObject, union,
//! first, last, take, skip, json), resource functions
//! (resourceGroup, subscription, tenant, deployment, resourceId, subscriptionResourceId,
//! reference), uniqueString, guid, copyIndex. The rest of the reference is in the modules below: `colls` (arrays, objects,
//! strings, numbers), `lambda` (map, filter, reduce, sort, toObject, mapValues, groupBy), `dates` and `dotnet` (utcNow,
//! dateTimeAdd, ... and .NET date formats), `cloud` (environment), `scope` (extensionResourceId, tenantResourceId, ...),
//! `userfn` (user-defined functions). `list*` functions run a provider action: the evaluator cannot wait for one, so it
//! stops with `EvalError::Action` and the deployment runs the action and evaluates again (`list_calls.rs`).
//!
//! Everything a template can make big or deep is limited (`limits.rs`): how deep expressions and values nest, how
//! large one value is, how much work a request may do in total, `range` and `copy` counts, integer arithmetic is
//! checked. A broken limit is `EvalError::Invalid`: the request answers `InvalidTemplate` (400). Each variable is
//! evaluated once per template (and copy index) and then reused.

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::limits::{self, Budget, Memo};
use crate::template_parse::{Ast, Parser};

mod builtins;
mod cloud;
pub use cloud::environment as cloud_environment;
mod colls;
mod dates;
pub mod dotnet;
mod lambda;
pub mod schema;
mod scope;
pub mod userfn;

pub enum EvalError {
    Msg(String),
    /// A `reference()` to something that is not deployed yet: try again after the others.
    Defer(String),
    /// The template breaks a limit or does arithmetic that cannot be done (too deep, too big, too much work,
    /// integer overflow, division by zero). The whole request is `InvalidTemplate`, not a failed resource.
    Invalid(String),
    /// A `list*` function that has to run a provider action (which is asynchronous, and the evaluator is not). The
    /// deployment runs it, keeps the answer and evaluates the same expression again.
    Action(Box<ListCall>),
    /// A function that cannot work on the values it was given (a date that is not a date, `string` of a float, `utcNow` in an
    /// output). Azure starts the deployment and fails it while it evaluates the outputs (`DeploymentOutputEvaluationFailed`).
    Runtime(String),
    /// A `list*` action that answered an error: Azure makes the POST and reports what came back (`DeploymentFailed` with this
    /// code and message).
    Remote(Box<RemoteError>),
}

#[derive(Clone, Debug)]
pub struct RemoteError {
    pub code: String,
    pub target: Option<String>,
    pub message: String,
}

/// A call of `listKeys(resource, apiVersion[, body])` and its relatives.
#[derive(Clone, Debug)]
pub struct ListCall {
    /// As written in the template (`listKeys`).
    pub function: String,
    /// The resource, as written (a name, a resource id or a symbolic name) until the deployment resolves it to an id.
    pub target: String,
    pub api_version: String,
    pub body: Value,
}

impl EvalError {
    pub fn text(&self) -> String {
        match self {
            EvalError::Msg(m) | EvalError::Defer(m) | EvalError::Invalid(m) | EvalError::Runtime(m) => m.clone(),
            EvalError::Remote(r) => r.message.clone(),
            EvalError::Action(c) => format!(
                "The template function '{}' cannot be used here: list functions can be used in the properties of a resource and in outputs.",
                c.function
            ),
        }
    }
}

fn err<T>(m: impl Into<String>) -> Result<T, EvalError> {
    Err(EvalError::Msg(m.into()))
}

fn runtime<T>(m: impl Into<String>) -> Result<T, EvalError> {
    Err(EvalError::Runtime(m.into()))
}

fn invalid<T>(m: impl Into<String>) -> Result<T, EvalError> {
    Err(EvalError::Invalid(m.into()))
}

pub type Lookup<'a> = &'a dyn Fn(&str, bool) -> Result<Value, EvalError>;
/// Runs (or asks for) a `list*` call; see `EvalError::Action`.
pub type ListFn<'a> = &'a dyn Fn(&ListCall) -> Result<Value, EvalError>;
/// The resource ids behind a symbolic name of a `languageVersion` 2.0 template, and whether it is a `copy` loop.
pub type SymbolFn<'a> = &'a dyn Fn(&str) -> Option<(Vec<String>, bool)>;

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
    /// Variables of the lambdas that are being evaluated, innermost last.
    lvars: RefCell<Vec<(String, Value)>>,
    /// The user-defined functions of the template.
    pub funcs: Option<&'a userfn::Functions>,
    /// `environment()`; the public cloud's values when not set.
    pub cloud: Option<&'a Value>,
    pub list: Option<ListFn<'a>>,
    pub symbols: Option<SymbolFn<'a>>,
    /// Set while a parameter's default value is evaluated: the only place `utcNow` and `newGuid` are allowed.
    pub defaults: bool,
    /// Inside a user-defined function: no variables, no `reference`, no `list*`, no other user-defined function.
    pub in_function: bool,
    /// The clock for `utcNow` (the system clock when not set).
    pub now: Option<dotnet::Dt>,
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
            lvars: RefCell::new(Vec::new()),
            funcs: None,
            cloud: None,
            list: None,
            symbols: None,
            defaults: false,
            in_function: false,
            now: None,
            depth: Cell::new(0),
        }
    }

    /// The environment inside a user-defined function: its own parameters, no variables, no resources.
    pub(crate) fn function_scope<'b>(&'b self, params: &'b Map<String, Value>, no_variables: &'b Map<String, Value>) -> Env<'b> {
        Env {
            params,
            variables: no_variables,
            sub: self.sub,
            rg: self.rg,
            rg_info: self.rg_info,
            sub_info: self.sub_info,
            tenant: self.tenant,
            deployment: self.deployment,
            copy: Vec::new(),
            lookup: self.lookup,
            budget: self.budget.clone(),
            memo: Arc::new(Memo::default()),
            lvars: RefCell::new(Vec::new()),
            funcs: self.funcs,
            cloud: self.cloud,
            list: None,
            symbols: None,
            defaults: false,
            in_function: true,
            now: self.now,
            depth: Cell::new(self.depth.get()),
        }
    }

    pub fn with_funcs(mut self, f: &'a userfn::Functions) -> Env<'a> {
        self.funcs = Some(f);
        self
    }

    pub fn with_cloud(mut self, c: &'a Value) -> Env<'a> {
        self.cloud = Some(c);
        self
    }

    pub fn with_list(mut self, l: ListFn<'a>) -> Env<'a> {
        self.list = Some(l);
        self
    }

    pub fn with_symbols(mut self, s: SymbolFn<'a>) -> Env<'a> {
        self.symbols = Some(s);
        self
    }

    pub fn for_defaults(mut self) -> Env<'a> {
        self.defaults = true;
        self
    }

    pub fn with_now(mut self, now: dotnet::Dt) -> Env<'a> {
        self.now = Some(now);
        self
    }

    /// Use the counters and the variable cache of the deployment instead of fresh ones.
    pub fn with_shared(mut self, budget: Arc<Budget>, memo: Arc<Memo>) -> Env<'a> {
        self.budget = budget;
        self.memo = memo;
        self
    }

    pub(crate) fn charge(&self, units: u64) -> Result<(), EvalError> {
        self.budget.charge(units).map_err(EvalError::Invalid)
    }

    /// Charge for a value that was just made or copied, and refuse one that is too big or too deep.
    pub(crate) fn charge_value(&self, v: &Value) -> Result<(), EvalError> {
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

/// The name of a function that is not allowed in a variable (`utcNow`, `newGuid`, `list*`) and is called in `text`, if it
/// is an expression. Azure refuses such a template when it validates it, before anything is deployed.
pub fn forbidden_in_variable(text: &str) -> Option<String> {
    let body = expression_body(text)?;
    let ast = Parser::new(body).expr().ok()?;
    let mut stack = vec![&ast];
    while let Some(a) = stack.pop() {
        match a {
            Ast::Lit(_) => {}
            Ast::Prop(b, _) => stack.push(b),
            Ast::Index(b, i) => {
                stack.push(b);
                stack.push(i);
            }
            Ast::Call(n, args) => {
                let l = n.to_ascii_lowercase();
                if l == "utcnow" || l == "newguid" || (l.starts_with("list") && !l.contains('.')) {
                    return Some(n.clone());
                }
                stack.extend(args);
            }
        }
    }
    None
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
        lvars: RefCell::new(e.lvars.borrow().clone()),
        funcs: e.funcs,
        cloud: e.cloud,
        list: e.list,
        symbols: e.symbols,
        defaults: e.defaults,
        in_function: e.in_function,
        now: e.now,
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
        Err(e) if e.starts_with(crate::template_parse::FLOAT_LITERAL) => {
            let at = &e[crate::template_parse::FLOAT_LITERAL.len()..];
            return invalid(format!(
                "The language expression '{body}' at position '{at}' is not valid: float point numbers are not supported."
            ));
        }
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
        (Value::String(x), Value::String(y)) => x == y,
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
        // Lambda arguments are read from the syntax tree and evaluated once per element.
        f if lambda::is_lambda_function(f) => {
            let r = lambda::call(f, name, args, env)?;
            env.charge_value(&r)?;
            return Ok(r);
        }
        "lambda" => return err(
            "The function 'lambda' can only be used directly as an argument of filter, groupBy, map, mapValues, reduce, sort or toObject.",
        ),
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
        "reference" | "references" => reference_call(name, &lname, &v, env)?,
        other if other.starts_with("list") => list_call(name, &v, env)?,
        other if other.contains('.') => user_call(name, other, v, env)?,
        _ => builtins::call_strict(name, &lname, v, env)?,
    };
    env.charge_value(&r)?;
    Ok(r)
}

/// `reference(...)` and `references(...)`, kept out of `call` so that its stack frame stays small (it is part of the recursion).
#[inline(never)]
fn reference_call(name: &str, lname: &str, v: &[Value], env: &Env<'_>) -> Result<Value, EvalError> {
    if v.is_empty() {
        return err(format!("The function '{name}' needs at least 1 argument(s)."));
    }
    if env.in_function {
        return err(format!("The template function '{name}' is not allowed in a user-defined function."));
    }
    let full = v.iter().skip(1).any(|x| x.as_str().map(|t| t.eq_ignore_ascii_case("full")).unwrap_or(false));
    if lname == "reference" {
        env.charge(limits::REFERENCE_COST)?;
        return (env.lookup)(&s(&v[0]), full);
    }
    // languageVersion 2.0: the states of every copy of a resource that is a loop (template-functions-resource#references)
    let Some((ids, _)) = env.symbols.and_then(|f| f(&s(&v[0]))) else {
        return err(format!(
            "The function 'references' needs the symbolic name of a resource of this template, and '{}' is not one.",
            s(&v[0])
        ));
    };
    let mut out = Vec::new();
    for id in ids {
        env.charge(limits::REFERENCE_COST)?;
        out.push((env.lookup)(&id, full)?);
    }
    Ok(Value::Array(out))
}

/// `listKeys(resource, apiVersion[, body])`: see `EvalError::Action`.
#[inline(never)]
fn list_call(name: &str, v: &[Value], env: &Env<'_>) -> Result<Value, EvalError> {
    if v.len() < 2 {
        return err(format!("The function '{name}' needs at least 2 argument(s): the resource and the API version."));
    }
    if env.in_function {
        return err(format!("The template function '{name}' is not allowed in a user-defined function."));
    }
    let Some(list) = env.list else {
        return err(format!("The template function '{name}' cannot be evaluated here: it needs the resource it names to be deployed."));
    };
    env.charge(limits::REFERENCE_COST)?;
    let body = v.get(2).cloned().unwrap_or_else(|| json!({}));
    if !body.is_object() {
        return err(format!("{name}: the third argument must be an object."));
    }
    list(&ListCall { function: name.to_string(), target: s(&v[0]), api_version: s(&v[1]), body })
}

/// `namespace.name(args)`: a user-defined function.
#[inline(never)]
fn user_call(name: &str, lname: &str, v: Vec<Value>, env: &Env<'_>) -> Result<Value, EvalError> {
    if env.in_function {
        return err(format!("The user-defined function '{name}' cannot be called here: a user-defined function cannot call another one."));
    }
    let Some(f) = env.funcs.and_then(|f| f.get(lname)) else {
        return err(format!("The template function '{name}' is not defined or not valid."));
    };
    f.call(name, v, env)
}

/// `variables('name')`: evaluated once per template and copy index, then reused.
#[inline(never)]
fn variable(name: &str, v: &[Value], env: &Env<'_>) -> Result<Value, EvalError> {
    if v.is_empty() {
        return err(format!("The function '{name}' needs at least 1 argument(s)."));
    }
    if env.in_function {
        return err("The template function 'variables' is not allowed in a user-defined function: it can use only its own parameters.");
    }
    // A variable is not inside the lambda that uses it: it cannot see the lambda's variables.
    let saved = env.lvars.take();
    let r = variable_inner(v, env);
    *env.lvars.borrow_mut() = saved;
    r
}

fn variable_inner(v: &[Value], env: &Env<'_>) -> Result<Value, EvalError> {
    let n = s(&v[0]);
    let key = format!("{}|{:?}", n.to_ascii_lowercase(), env.copy);
    if let Some(hit) = env.memo.get(&key) {
        return Ok(hit);
    }
    let raw = env.variables.iter().find(|(k, _)| k.eq_ignore_ascii_case(&n)).map(|(_, x)| x);
    // `"variables": {"copy": [{"name": "x", "count": 3, "input": ...}]}` makes the array variable `x` (copy-variables)
    let looped = env
        .variables
        .get("copy")
        .and_then(Value::as_array)
        .and_then(|l| l.iter().find(|c| c.get("name").and_then(Value::as_str).is_some_and(|x| x.eq_ignore_ascii_case(&n))))
        .map(|c| json!({"copy": [c]}));
    let looped_value;
    let raw = match (raw, &looped) {
        (None, Some(l)) => {
            looped_value = l;
            Some(looped_value)
        }
        (r, _) => r,
    };
    let Some(raw) = raw else {
        return err(format!(
            "The template variable '{n}' is not found. Please see https://aka.ms/arm-template/#variables for usage details."
        ));
    };
    if !env.memo.enter(&key) {
        return invalid(format!("The template variable '{n}' refers to itself."));
    }
    let r = eval_value(raw, env).map(|v| {
        if looped.is_some() {
            v.get(&n).cloned().or_else(|| v.as_object().and_then(|o| o.values().next().cloned())).unwrap_or(Value::Null)
        } else {
            v
        }
    });
    env.memo.leave(&key);
    if let Ok(val) = &r {
        if let Ok(w) = limits::weight(val) {
            env.memo.put(key, val, w);
        }
    }
    r
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_functions;
#[cfg(test)]
mod tests_lambda;
