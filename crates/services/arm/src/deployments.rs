//! `Microsoft.Resources/deployments`: ARM templates.
//!
//! A deployment runs synchronously: when the PUT answers, every resource is stored (through
//! the same code as a direct PUT) and the state is `Succeeded` or `Failed`. Only incremental
//! mode is built. See `template.rs` for the expression language.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use azemu_core::Response;
use serde_json::{json, Map, Value};

use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::groups::{canonical_group_name_for, group_json, put_group_body};
use crate::ids::resource_id;
use crate::limits::{self, Budget, Memo};
use crate::resources::put_by_type;
use crate::state::{key, Stored};
use crate::template::{eval_value, Env, EvalError};
use crate::util;

/// Resources inside resources (`resources` in a resource) may go this deep.
const MAX_CHILD_LEVELS: usize = 8;

const KEEP_OUT: &[&str] =
    &["type", "apiVersion", "name", "dependsOn", "condition", "copy", "resources", "scope", "comments", "resourceGroup", "subscriptionId"];

pub async fn handle(cx: &Cx<'_>, sub: &str, rg: Option<&str>, tail: &[String]) -> ArmResult<Response> {
    let method = cx.req.method.as_str();
    let tail_lc: Vec<String> = tail.iter().map(|t| t.to_ascii_lowercase()).collect();
    let t: Vec<&str> = tail_lc.iter().map(String::as_str).collect();
    let rg_name = match rg {
        Some(r) => canonical_group_name_for(cx, sub, r)?,
        None => String::new(),
    };
    match (method, t.as_slice()) {
        ("GET", []) => list(cx, sub, &rg_name),
        ("PUT", [_]) => create(cx, sub, &rg_name, &tail[0], false).await,
        ("GET", [_]) => get(cx, sub, &rg_name, &tail[0]),
        ("HEAD", [_]) => match get(cx, sub, &rg_name, &tail[0]) {
            Ok(_) => Ok(util::empty(204)),
            Err(e) if e.status == 404 => Ok(util::empty(404)),
            Err(e) => Err(e),
        },
        ("DELETE", [_]) => {
            let id = resource_id(sub, &rg_name, "Microsoft.Resources", &["deployments".into()], &[tail[0].clone()]);
            cx.inner.state.lock().resources.remove(&key(&id));
            Ok(util::empty(204))
        }
        ("POST", [_, "validate"]) => create(cx, sub, &rg_name, &tail[0], true).await,
        ("POST", [_, "cancel"]) => {
            let d = find(cx, sub, &rg_name, &tail[0])?;
            Err(ArmError::conflict(
                "DeploymentCannotBeCancelled",
                format!(
                    "The deployment '{}' cannot be cancelled because it is in state '{}'.",
                    tail[0],
                    d.prop_str("provisioningState").unwrap_or("Succeeded")
                ),
            ))
        }
        ("POST", [_, "exporttemplate"]) => {
            let d = find(cx, sub, &rg_name, &tail[0])?;
            Ok(util::json_ok(200, &json!({"template": d.extra.get("template").cloned().unwrap_or(json!({}))})))
        }
        ("GET", [_, "operations"]) => {
            let d = find(cx, sub, &rg_name, &tail[0])?;
            Ok(util::list(d.extra.get("operations").and_then(Value::as_array).cloned().unwrap_or_default()))
        }
        ("GET", [_, "operations", _]) => {
            let d = find(cx, sub, &rg_name, &tail[0])?;
            let ops = d.extra.get("operations").and_then(Value::as_array).cloned().unwrap_or_default();
            match ops.into_iter().find(|o| o["operationId"].as_str().map(|x| x.eq_ignore_ascii_case(&tail[2])).unwrap_or(false)) {
                Some(o) => Ok(util::json_ok(200, &o)),
                None => Err(ArmError::not_found(
                    "DeploymentOperationNotFound",
                    format!("The deployment operation '{}' was not found.", tail[2]),
                )),
            }
        }
        _ => Err(ArmError::not_implemented(format!(
            "{method} {} (deployments: what-if and template links are not built)",
            cx.req.decoded_path()
        ))),
    }
}

fn find(cx: &Cx<'_>, sub: &str, rg: &str, name: &str) -> ArmResult<Stored> {
    let id = resource_id(sub, rg, "Microsoft.Resources", &["deployments".into()], &[name.to_string()]);
    cx.inner
        .state
        .lock()
        .resources
        .get(&key(&id))
        .cloned()
        .ok_or_else(|| ArmError::not_found("DeploymentNotFound", format!("Deployment '{name}' could not be found.")))
}

fn get(cx: &Cx<'_>, sub: &str, rg: &str, name: &str) -> ArmResult<Response> {
    Ok(util::json_ok(200, &find(cx, sub, rg, name)?.body))
}

fn list(cx: &Cx<'_>, sub: &str, rg: &str) -> ArmResult<Response> {
    let st = cx.inner.state.lock();
    let prefix = if rg.is_empty() {
        format!("/subscriptions/{}/providers/microsoft.resources/deployments/", sub.to_ascii_lowercase())
    } else {
        format!("{}/providers/microsoft.resources/deployments/", crate::state::group_key(sub, rg))
    };
    let items = st
        .resources
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .filter(|(k, _)| !k[prefix.len()..].contains('/'))
        .map(|(_, s)| s.body.clone())
        .collect();
    Ok(util::list(items))
}

// ---- running a deployment ---------------------------------------------------

struct Item {
    /// Shared by the copies of one resource: a copy loop does not copy the definition.
    raw: Arc<Value>,
    copy: Option<(String, i64)>,
    full_type: String,
    names: Vec<String>,
    target_rg: String,
    parent_id: Option<String>,
}

struct Outcome {
    template: Value,
    params_out: Map<String, Value>,
    outputs: Map<String, Value>,
    deployed_ids: Vec<String>,
    ops: Vec<Value>,
    providers: Vec<(String, String, String)>,
    failure: Option<ArmError>,
    validated: Vec<String>,
}

/// Everything needed to evaluate expressions, owned so no borrow crosses an `.await`.
struct Scope {
    sub: String,
    rg: String,
    params: Map<String, Value>,
    variables: Map<String, Value>,
    rg_info: Value,
    sub_info: Value,
    tenant: String,
    deployment: Value,
    /// Work counters of the whole request, shared with nested deployments.
    budget: Arc<Budget>,
    /// Variables of this template that are already evaluated.
    memo: Arc<Memo>,
}

impl Scope {
    fn with_env<R>(&self, cx: &Cx<'_>, copy: &[(String, i64)], f: impl FnOnce(&Env<'_>) -> R) -> R {
        let look = |arg: &str, full: bool| lookup_resource(cx, &self.sub, &self.rg, arg, full);
        let mut env = Env::new(
            &self.params,
            &self.variables,
            &self.sub,
            &self.rg,
            &self.rg_info,
            &self.sub_info,
            &self.tenant,
            &self.deployment,
            &look,
        )
        .with_shared(self.budget.clone(), self.memo.clone());
        env.copy = copy.to_vec();
        f(&env)
    }

    fn eval(&self, cx: &Cx<'_>, copy: &[(String, i64)], v: &Value) -> Result<Value, EvalError> {
        self.with_env(cx, copy, |e| eval_value(v, e))
    }
}

fn lookup_resource(cx: &Cx<'_>, sub: &str, rg: &str, arg: &str, full: bool) -> Result<Value, EvalError> {
    let st = cx.inner.state.lock();
    let found: Option<Stored> = if arg.starts_with("/subscriptions") {
        st.resources.get(&key(arg)).cloned()
    } else {
        let prefix = if rg.is_empty() {
            format!("/subscriptions/{}/", sub.to_ascii_lowercase())
        } else {
            format!("{}/", crate::state::group_key(sub, rg))
        };
        let want = arg.to_ascii_lowercase();
        st.resources
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(_, s)| s)
            .filter(|s| {
                s.names.join("/").to_ascii_lowercase() == want
                    || s.name().eq_ignore_ascii_case(arg)
                    || format!("{}/{}", s.type_string(), s.names.join("/")).eq_ignore_ascii_case(arg)
            })
            .min_by_key(|s| s.types.len())
            .cloned()
    };
    let Some(s) = found else {
        return Err(EvalError::Defer(format!("The Resource '{arg}' is not deployed yet or does not exist.")));
    };
    drop(st);
    let rendered = match cx.inner.provider(&s.ns) {
        Some(p) => p.render(cx, &s),
        None => s.body.clone(),
    };
    if full {
        Ok(rendered)
    } else {
        Ok(rendered.get("properties").cloned().unwrap_or(json!({})))
    }
}

fn titlecase(t: &str) -> String {
    let mut c = t.chars();
    match c.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
        None => String::new(),
    }
}

fn invalid_template(msg: impl Into<String>) -> ArmError {
    ArmError::bad_request("InvalidTemplate", format!("Deployment template validation failed: {}", msg.into()))
}

fn template_hash(t: &Value) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(t.to_string().as_bytes());
    let mut n = [0u8; 8];
    n.copy_from_slice(&d[..8]);
    (u64::from_be_bytes(n) % 10_000_000_000_000_000_000).to_string()
}

fn bind_parameters(cx: &Cx<'_>, scope: &Scope, template: &Value, given: &Value) -> ArmResult<(Map<String, Value>, Map<String, Value>)> {
    let declared = template.get("parameters").and_then(Value::as_object).cloned().unwrap_or_default();
    let given_map = given.as_object().cloned().unwrap_or_default();
    for k in given_map.keys() {
        if !declared.keys().any(|d| d.eq_ignore_ascii_case(k)) {
            return Err(ArmError::bad_request(
                "InvalidTemplate",
                format!("Deployment template validation failed: The template parameters '{k}' in the parameters file are not valid; they are not present in the original template and can therefore not be provided at deployment time."),
            ));
        }
    }
    let mut bound = Map::new();
    let mut shown = Map::new();
    for (name, decl) in &declared {
        let ty = decl.get("type").and_then(Value::as_str).unwrap_or("string").to_ascii_lowercase();
        let secure = ty.starts_with("secure");
        let provided = given_map.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v);
        let raw = match provided {
            Some(p) if p.get("reference").is_some() => {
                return Err(ArmError::not_implemented(format!("parameter '{name}' uses a Key Vault reference; the emulator cannot read secrets from a deployment parameters object")));
            }
            Some(p) => p.get("value").cloned().ok_or_else(|| invalid_template(format!("The value for the template parameter '{name}' is not provided.")))?,
            None => match decl.get("defaultValue") {
                Some(d) => scope.eval(cx, &[], d).map_err(|e| invalid_template(e.text()))?,
                None => {
                    return Err(invalid_template(format!(
                        "The template parameter '{name}' is not found in the parameters file and has no default value. Please see https://aka.ms/arm-deploy/#parameter-file for usage details."
                    )))
                }
            },
        };
        let v = coerce(&ty, raw)
            .map_err(|m| invalid_template(format!("The provided value for the template parameter '{name}' is not valid: {m}")))?;
        if let Some(allowed) = decl.get("allowedValues").and_then(Value::as_array) {
            if !allowed.iter().any(|a| {
                a == &v
                    || (a.is_string()
                        && v.is_string()
                        && a.as_str().map(|x| x.eq_ignore_ascii_case(v.as_str().unwrap_or(""))).unwrap_or(false))
            }) {
                return Err(invalid_template(format!(
                    "The provided value for the template parameter '{name}' is not valid. The value must be one of {}.",
                    Value::Array(allowed.clone())
                )));
            }
        }
        if let Some(n) = v.as_i64() {
            if decl.get("minValue").and_then(Value::as_i64).map(|m| n < m).unwrap_or(false)
                || decl.get("maxValue").and_then(Value::as_i64).map(|m| n > m).unwrap_or(false)
            {
                return Err(invalid_template(format!("The provided value for the template parameter '{name}' is out of range.")));
            }
        }
        if let Some(t) = v.as_str() {
            let len = t.chars().count() as i64;
            if decl.get("minLength").and_then(Value::as_i64).map(|m| len < m).unwrap_or(false)
                || decl.get("maxLength").and_then(Value::as_i64).map(|m| len > m).unwrap_or(false)
            {
                return Err(invalid_template(format!("The provided value for the template parameter '{name}' is not the right length.")));
            }
        }
        let mut entry = json!({"type": titlecase(&ty).replace("Securestring", "SecureString").replace("Secureobject", "SecureObject")});
        if !secure {
            entry["value"] = v.clone();
        }
        shown.insert(name.clone(), entry);
        bound.insert(name.clone(), v);
    }
    Ok((bound, shown))
}

fn coerce(ty: &str, v: Value) -> Result<Value, String> {
    match (ty, &v) {
        ("string" | "securestring", Value::String(_)) => Ok(v),
        ("string" | "securestring", Value::Number(n)) => Ok(Value::String(n.to_string())),
        ("int", Value::Number(_)) => Ok(v),
        ("int", Value::String(t)) => t.trim().parse::<i64>().map(|n| json!(n)).map_err(|_| format!("'{t}' is not an integer")),
        ("bool", Value::Bool(_)) => Ok(v),
        ("bool", Value::String(t)) if t.eq_ignore_ascii_case("true") => Ok(json!(true)),
        ("bool", Value::String(t)) if t.eq_ignore_ascii_case("false") => Ok(json!(false)),
        ("array", Value::Array(_)) => Ok(v),
        ("object" | "secureobject", Value::Object(_)) => Ok(v),
        (t, other) => Err(format!(
            "expected {t}, got {}",
            match other {
                Value::String(_) => "string",
                Value::Number(_) => "number",
                Value::Bool(_) => "bool",
                Value::Array(_) => "array",
                Value::Object(_) => "object",
                Value::Null => "null",
            }
        )),
    }
}

/// Expand conditions, copy loops and nested resources into a flat list. At most `MAX_RESOURCES` items come out, and
/// child resources nest at most `MAX_CHILD_LEVELS` deep.
fn expand(
    cx: &Cx<'_>,
    scope: &Scope,
    resources: &[Value],
    parent: Option<(&str, &[String])>,
    default_rg: &str,
    level: usize,
    out: &mut Vec<Item>,
) -> Result<(), ArmError> {
    if level > MAX_CHILD_LEVELS {
        return Err(invalid_template(format!("resources are nested in each other more than {MAX_CHILD_LEVELS} levels deep.")));
    }
    for raw in resources {
        let raw_shared = Arc::new(raw.clone());
        let count = match raw.get("copy") {
            Some(c) => {
                let n = scope.eval(cx, &[], c.get("count").unwrap_or(&json!(1))).map_err(|e| invalid_template(e.text()))?;
                let n = n.as_i64().or_else(|| n.as_str().and_then(|t| t.parse().ok())).unwrap_or(1);
                if !(0..=limits::MAX_COPY_RESOURCES).contains(&n) {
                    return Err(invalid_template(format!(
                        "the copy count {n} is not valid: it must be between 0 and {}.",
                        limits::MAX_COPY_RESOURCES
                    )));
                }
                Some((c.get("name").and_then(Value::as_str).unwrap_or("copy").to_string(), n))
            }
            None => None,
        };
        let indexes: Vec<Option<(String, i64)>> = match &count {
            Some((n, c)) => (0..*c).map(|i| Some((n.clone(), i))).collect(),
            None => vec![None],
        };
        for idx in indexes {
            if out.len() >= limits::MAX_RESOURCES {
                return Err(invalid_template(format!(
                    "the template has more than {} resources (a copy loop counts every copy).",
                    limits::MAX_RESOURCES
                )));
            }
            let copy_stack: Vec<(String, i64)> = idx.iter().cloned().collect();
            if let Some(cond) = raw.get("condition") {
                let v = scope.eval(cx, &copy_stack, cond).map_err(|e| invalid_template(e.text()))?;
                let on = match &v {
                    Value::Bool(b) => *b,
                    Value::String(t) => t.eq_ignore_ascii_case("true"),
                    _ => true,
                };
                if !on {
                    continue;
                }
            }
            let ty = scope.eval(cx, &copy_stack, raw.get("type").unwrap_or(&Value::Null)).map_err(|e| invalid_template(e.text()))?;
            let name = scope.eval(cx, &copy_stack, raw.get("name").unwrap_or(&Value::Null)).map_err(|e| invalid_template(e.text()))?;
            let (Some(ty), Some(name)) = (ty.as_str(), name.as_str()) else {
                return Err(invalid_template("every resource needs a type and a name."));
            };
            let (full_type, names) = match parent {
                Some((pt, pn)) => {
                    let full = if ty.contains('.') && ty.to_ascii_lowercase().starts_with(&pt.to_ascii_lowercase()) {
                        ty.to_string()
                    } else {
                        format!("{pt}/{ty}")
                    };
                    let want = full.split('/').count() - 1;
                    let own: Vec<String> = name.split('/').map(String::from).collect();
                    let names = if own.len() == want { own } else { pn.iter().cloned().chain(own).collect() };
                    (full, names)
                }
                None => (ty.to_string(), name.split('/').map(String::from).collect()),
            };
            let target_rg = match raw.get("resourceGroup") {
                Some(r) => {
                    scope.eval(cx, &copy_stack, r).map_err(|e| invalid_template(e.text()))?.as_str().unwrap_or(default_rg).to_string()
                }
                None => default_rg.to_string(),
            };
            let parent_id = parent.map(|(pt, pn)| {
                let (ns, rest) = pt.split_once('/').unwrap_or((pt, ""));
                let types: Vec<String> = rest.split('/').map(String::from).collect();
                resource_id(&scope.sub, &target_rg, ns, &types, pn)
            });
            let item = Item {
                raw: raw_shared.clone(),
                copy: idx.clone(),
                full_type: full_type.clone(),
                names: names.clone(),
                target_rg,
                parent_id,
            };
            out.push(item);
            if let Some(children) = raw.get("resources").and_then(Value::as_array) {
                expand(cx, scope, children, Some((&full_type, &names)), default_rg, level + 1, out)?;
            }
        }
    }
    Ok(())
}

fn item_id(scope: &Scope, it: &Item) -> String {
    let (ns, rest) = it.full_type.split_once('/').unwrap_or((&it.full_type, ""));
    let types: Vec<String> = rest.split('/').map(String::from).collect();
    resource_id(&scope.sub, &it.target_rg, ns, &types, &it.names)
}

fn error_of(e: &ArmError) -> Value {
    json!({"code": e.code, "message": e.message})
}

/// Where a deployment stands in its request: how deep it is nested and the counters shared by every deployment of the request.
#[derive(Clone)]
struct Level {
    depth: usize,
    budget: Arc<Budget>,
}

impl Level {
    fn top() -> Level {
        Level { depth: 0, budget: Arc::new(Budget::default()) }
    }
    fn inner(&self) -> Level {
        Level { depth: self.depth + 1, budget: self.budget.clone() }
    }
}

fn run<'a>(
    cx: &'a Cx<'a>,
    sub: &'a str,
    rg: &'a str,
    name: &'a str,
    props: &'a Value,
    validate_only: bool,
    level: &'a Level,
) -> Pin<Box<dyn Future<Output = ArmResult<Outcome>> + Send + 'a>> {
    Box::pin(async move {
        if level.depth >= limits::MAX_NESTED_DEPLOYMENTS {
            return Err(invalid_template(format!(
                "deployments are nested in each other more than {} levels deep.",
                limits::MAX_NESTED_DEPLOYMENTS
            )));
        }
        let mode = props.get("mode").and_then(Value::as_str).unwrap_or("Incremental");
        if mode.eq_ignore_ascii_case("Complete") {
            return Err(ArmError::not_implemented("deployment mode Complete is not built; use Incremental"));
        }
        if props.get("templateLink").is_some() || props.get("parametersLink").is_some() {
            return Err(ArmError::not_implemented("templateLink and parametersLink are not built: the emulator makes no outbound calls. Send the template in properties.template"));
        }
        let template =
            props.get("template").cloned().ok_or_else(|| invalid_template("the deployment has no template (properties.template)."))?;
        let template = if let Value::String(t) = &template {
            serde_json::from_str(t).map_err(|e| invalid_template(format!("the template is not valid JSON: {e}")))?
        } else {
            template
        };
        if !template.is_object()
            || template.get("resources").is_none() && template.get("$schema").is_none() && template.get("outputs").is_none()
        {
            return Err(invalid_template("the template must be a JSON object with a 'resources' array."));
        }
        for (what, n, max) in [
            ("parameters", template.get("parameters").and_then(Value::as_object).map(|o| o.len()), limits::MAX_PARAMETERS),
            ("variables", template.get("variables").and_then(Value::as_object).map(|o| o.len()), limits::MAX_VARIABLES),
            ("outputs", template.get("outputs").and_then(Value::as_object).map(|o| o.len()), limits::MAX_OUTPUTS),
        ] {
            if n.unwrap_or(0) > max {
                return Err(invalid_template(format!("the template has more than {max} {what}.")));
            }
        }
        let rg_info = if rg.is_empty() {
            json!({})
        } else {
            let st = cx.inner.state.lock();
            match st.group(sub, rg) {
                Some(g) => group_json(g),
                None => return Err(ArmError::group_not_found(rg)),
            }
        };
        let tenant = crate::basics::default_tenant();
        let sub_info =
            json!({"id": format!("/subscriptions/{sub}"), "subscriptionId": sub, "tenantId": tenant, "displayName": "azemu subscription"});
        let mut scope = Scope {
            sub: sub.to_string(),
            rg: rg.to_string(),
            params: Map::new(),
            variables: template.get("variables").and_then(Value::as_object).cloned().unwrap_or_default(),
            rg_info,
            sub_info,
            tenant,
            deployment: json!({"name": name, "properties": {"template": template.clone(), "mode": mode}}),
            budget: level.budget.clone(),
            memo: Arc::new(Memo::default()),
        };
        let (bound, shown) = bind_parameters(cx, &scope, &template, props.get("parameters").unwrap_or(&json!({})))?;
        scope.params = bound;
        // `variables` with `copy` entries are expanded on use by eval_value; nothing to do here.
        let resources = template.get("resources").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut items = Vec::new();
        expand(cx, &scope, &resources, None, rg, 0, &mut items)?;
        if level.budget.resources.fetch_add(items.len(), Ordering::Relaxed) + items.len() > limits::MAX_TOTAL_RESOURCES {
            return Err(invalid_template(format!(
                "the request deploys more than {} resources in all (nested deployments included).",
                limits::MAX_TOTAL_RESOURCES
            )));
        }

        let mut out = Outcome {
            template: template.clone(),
            params_out: shown,
            outputs: Map::new(),
            deployed_ids: vec![],
            ops: vec![],
            providers: vec![],
            failure: None,
            validated: vec![],
        };
        // Order: an item waits until every item it depends on is done. `remaining[i]` counts those still open.
        let graph = dependency_graph(cx, &scope, &items);
        let mut remaining: Vec<usize> = graph.iter().map(BTreeSet::len).collect();
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); items.len()];
        for (i, deps) in graph.iter().enumerate() {
            for &j in deps {
                dependents[j].push(i);
            }
        }
        let mut slots: Vec<Option<Item>> = items.into_iter().map(Some).collect();
        let mut pending: Vec<usize> = (0..slots.len()).collect();
        while !pending.is_empty() {
            let mut progressed = false;
            let waiting: Vec<usize> = std::mem::take(&mut pending);
            let (ready, blocked): (Vec<usize>, Vec<usize>) = waiting.into_iter().partition(|&i| remaining[i] == 0);
            let mut deferred: Vec<usize> = blocked;
            let mut done: Vec<usize> = Vec::new();
            for i in ready {
                let Some(it) = slots[i].take() else { continue };
                let copy_stack: Vec<(String, i64)> = it.copy.iter().cloned().collect();
                // Evaluate the resource body in a synchronous block (no Env across an await).
                let body = scope.with_env(cx, &copy_stack, |env| {
                    let mut o = Map::new();
                    let is_nested = it.full_type.eq_ignore_ascii_case("Microsoft.Resources/deployments");
                    if let Some(raw) = it.raw.as_object() {
                        for (k, v) in raw {
                            if KEEP_OUT.iter().any(|x| x.eq_ignore_ascii_case(k)) {
                                continue;
                            }
                            if is_nested && k == "properties" {
                                // Parameters of a nested deployment are evaluated here; its template runs on its own.
                                let mut p = v.as_object().cloned().unwrap_or_default();
                                if let Some(params) = p.get("parameters").cloned() {
                                    match eval_value(&params, env) {
                                        Ok(x) => {
                                            p.insert("parameters".into(), x);
                                        }
                                        Err(e) => return Err(e),
                                    }
                                }
                                o.insert(k.clone(), Value::Object(p));
                                continue;
                            }
                            match eval_value(v, env) {
                                Ok(x) => {
                                    o.insert(k.clone(), x);
                                }
                                Err(e) => return Err(e),
                            }
                        }
                    }
                    Ok(Value::Object(o))
                });
                let body = match body {
                    Ok(b) => b,
                    Err(EvalError::Defer(_)) => {
                        slots[i] = Some(it);
                        deferred.push(i);
                        continue;
                    }
                    // A broken limit or impossible arithmetic makes the whole template invalid, wherever it shows up.
                    Err(EvalError::Invalid(m)) => return Err(invalid_template(m)),
                    Err(EvalError::Msg(m)) => {
                        let e = ArmError::bad_request("InvalidTemplate", format!("Deployment template validation failed: {m}"));
                        if validate_only {
                            return Err(e);
                        }
                        out.failure = Some(e);
                        continue;
                    }
                };
                // A resource definition is at most 1 MB and the whole expanded template at most 4 MB (Azure).
                let size = limits::weight(&body).unwrap_or(u64::MAX);
                if size > 1024 * 1024 {
                    return Err(invalid_template(
                        "a resource definition is larger than 1 MB after its expressions and copy loops are evaluated.",
                    ));
                }
                if level.budget.expanded_bytes.fetch_add(size, Ordering::Relaxed) + size > limits::MAX_EXPANDED_BYTES {
                    return Err(invalid_template("the template is larger than 4 MB after its expressions and copy loops are evaluated."));
                }
                let id = item_id(&scope, &it);
                if validate_only {
                    out.validated.push(id);
                    progressed = true;
                    done.push(i);
                    continue;
                }
                let started = Instant::now();
                let res = put_one(cx, sub, name, &it, body, level).await;
                let elapsed = started.elapsed();
                let (ns, rest) = it.full_type.split_once('/').unwrap_or((&it.full_type, ""));
                match res {
                    Ok(rendered) => {
                        let rid = rendered.get("id").and_then(Value::as_str).unwrap_or(&id).to_string();
                        let loc = rendered.get("location").and_then(Value::as_str).unwrap_or("").to_string();
                        out.deployed_ids.push(rid.clone());
                        if !out.providers.iter().any(|(n, t, l)| n == ns && t == rest && l == &loc) {
                            out.providers.push((ns.to_string(), rest.to_string(), loc));
                        }
                        out.ops.push(operation(sub, rg, name, &it, &rid, "Succeeded", "OK", None, elapsed.as_secs_f64(), cx));
                        progressed = true;
                        done.push(i);
                    }
                    Err(e) => {
                        out.ops.push(operation(sub, rg, name, &it, &id, "Failed", &e.code, Some(&e), elapsed.as_secs_f64(), cx));
                        if out.failure.is_none() {
                            out.failure = Some(e);
                        }
                        progressed = true;
                    }
                }
            }
            for i in done {
                for &k in &dependents[i] {
                    remaining[k] = remaining[k].saturating_sub(1);
                }
            }
            pending = deferred;
            if out.failure.is_some() {
                break;
            }
            if !progressed && !pending.is_empty() {
                let names: Vec<String> =
                    pending.iter().filter_map(|&i| slots[i].as_ref()).map(|i| format!("{}/{}", i.full_type, i.names.join("/"))).collect();
                return Err(ArmError::bad_request(
                    "InvalidTemplate",
                    format!("Deployment template validation failed: Circular dependency or a reference() to a resource that is not part of the template: {}.", names.join(", ")),
                ));
            }
        }

        if out.failure.is_none() {
            // outputs
            if let Some(outputs) = template.get("outputs").and_then(Value::as_object) {
                for (n, o) in outputs {
                    let ty = o.get("type").and_then(Value::as_str).unwrap_or("string");
                    match scope.eval(cx, &[], o.get("value").unwrap_or(&Value::Null)) {
                        Ok(v) => {
                            out.outputs.insert(n.clone(), json!({"type": titlecase(ty), "value": v}));
                        }
                        Err(EvalError::Invalid(m)) => return Err(invalid_template(format!("The template output '{n}' is not valid: {m}"))),
                        Err(EvalError::Defer(m)) | Err(EvalError::Msg(m)) => {
                            if validate_only {
                                continue;
                            }
                            return Err(invalid_template(format!("The template output '{n}' is not valid: {m}")));
                        }
                    }
                }
            }
        }
        Ok(out)
    })
}

/// For each item, the items it has to wait for: those named in its `dependsOn` and its parent. Built once, with
/// the names of all items in a table, so that a template of 800 resources with long `dependsOn` lists stays cheap.
fn dependency_graph(cx: &Cx<'_>, scope: &Scope, items: &[Item]) -> Vec<BTreeSet<usize>> {
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        let joined = it.names.join("/");
        for key in [item_id(scope, it), joined.clone(), format!("{}/{}", it.full_type, joined)] {
            by_name.entry(key.to_ascii_lowercase()).or_default().push(i);
        }
    }
    items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let copy_stack: Vec<(String, i64)> = it.copy.iter().cloned().collect();
            let mut deps: Vec<String> = it
                .raw
                .get("dependsOn")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|d| scope.eval(cx, &copy_stack, d).ok().and_then(|v| v.as_str().map(String::from))).collect())
                .unwrap_or_default();
            if let Some(p) = &it.parent_id {
                deps.push(p.clone());
            }
            let mut targets = BTreeSet::new();
            for d in deps {
                for &j in by_name.get(&d.to_ascii_lowercase()).into_iter().flatten() {
                    if j != i {
                        targets.insert(j);
                    }
                }
            }
            targets
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn operation(
    sub: &str,
    rg: &str,
    dep: &str,
    it: &Item,
    rid: &str,
    state: &str,
    status: &str,
    error: Option<&ArmError>,
    secs: f64,
    cx: &Cx<'_>,
) -> Value {
    let opid = uuid::Uuid::new_v4().simple().to_string().to_ascii_uppercase();
    let base = if rg.is_empty() { format!("/subscriptions/{sub}") } else { format!("/subscriptions/{sub}/resourceGroups/{rg}") };
    let mut props = json!({
        "provisioningOperation": "Create",
        "provisioningState": state,
        "timestamp": cx.now_iso(),
        "duration": format!("PT{:.3}S", secs),
        "serviceRequestId": uuid::Uuid::new_v4().to_string(),
        "statusCode": status,
        "targetResource": {"id": rid, "resourceType": it.full_type, "resourceName": it.names.join("/")},
    });
    if let Some(e) = error {
        props["statusMessage"] = json!({"error": error_of(e)});
    }
    json!({"id": format!("{base}/providers/Microsoft.Resources/deployments/{dep}/operations/{opid}"), "operationId": opid, "properties": props})
}

/// Create or update one resource of a template (a direct PUT, or a group, or a nested deployment).
async fn put_one(cx: &Cx<'_>, sub: &str, dep_name: &str, it: &Item, mut body: Value, level: &Level) -> ArmResult<Value> {
    let lt = it.full_type.to_ascii_lowercase();
    if lt == "microsoft.resources/resourcegroups" {
        let name = it.names.first().cloned().unwrap_or_default();
        let (_, v) = put_group_body(cx, sub, &name, &body)?;
        return Ok(v);
    }
    if lt == "microsoft.resources/deployments" {
        let name = it.names.first().cloned().unwrap_or_default();
        let inner_rg = it.target_rg.clone();
        let props = body.get("properties").cloned().unwrap_or(json!({}));
        let (_, v) = store_deployment(cx, sub, &inner_rg, &name, &props, &body, &level.inner()).await?;
        if v["properties"]["provisioningState"] == "Failed" {
            return Err(ArmError::new(
                400,
                "DeploymentFailed",
                format!(
                    "The nested deployment '{name}' of '{dep_name}' failed: {}",
                    v["properties"]["error"]["details"][0]["message"]
                        .as_str()
                        .or(v["properties"]["error"]["message"].as_str())
                        .unwrap_or("see its operations")
                ),
            ));
        }
        return Ok(v);
    }
    if let Some(o) = body.as_object_mut() {
        o.remove("apiVersion");
    }
    put_by_type(cx, sub, &it.target_rg, &it.full_type, &it.names, body).await
}

/// Run a deployment and store its record. Returns the HTTP status and the deployment JSON.
async fn store_deployment(
    cx: &Cx<'_>,
    sub: &str,
    rg: &str,
    name: &str,
    props: &Value,
    whole: &Value,
    level: &Level,
) -> ArmResult<(u16, Value)> {
    let started = Instant::now();
    let outcome = run(cx, sub, rg, name, props, false, level).await?;
    let id = resource_id(sub, rg, "Microsoft.Resources", &["deployments".into()], &[name.to_string()]);
    let failed = outcome.failure.is_some();
    let mut providers: Vec<Value> = Vec::new();
    for (ns, ty, loc) in &outcome.providers {
        if let Some(p) = providers.iter_mut().find(|p| p["namespace"] == *ns) {
            if let Some(a) = p["resourceTypes"].as_array_mut() {
                if let Some(t) = a.iter_mut().find(|t| t["resourceType"] == *ty) {
                    if let Some(l) = t["locations"].as_array_mut() {
                        if !l.contains(&json!(loc)) {
                            l.push(json!(loc));
                        }
                    }
                } else {
                    a.push(json!({"resourceType": ty, "locations": [loc]}));
                }
            }
        } else {
            providers.push(json!({"namespace": ns, "resourceTypes": [{"resourceType": ty, "locations": [loc]}]}));
        }
    }
    let mut p = json!({
        "templateHash": template_hash(&outcome.template),
        "parameters": outcome.params_out,
        "mode": "Incremental",
        "provisioningState": if failed { "Failed" } else { "Succeeded" },
        "timestamp": cx.now_iso(),
        "duration": format!("PT{:.3}S", started.elapsed().as_secs_f64()),
        "correlationId": cx.req.ctx.request_id,
        "providers": providers,
        "dependencies": [],
        "outputs": outcome.outputs,
        "outputResources": outcome.deployed_ids.iter().map(|i| json!({"id": i})).collect::<Vec<_>>(),
    });
    if let Some(e) = &outcome.failure {
        p["error"] = json!({
            "code": "DeploymentFailed",
            "message": "At least one resource deployment operation failed. Please list deployment operations for details. Please see https://aka.ms/arm-deployment-operations for usage details.",
            "details": [error_of(e)],
        });
    }
    let mut body = json!({"id": id, "name": name, "type": "Microsoft.Resources/deployments", "properties": p});
    if rg.is_empty() {
        body["location"] = whole.get("location").cloned().unwrap_or(json!("eastus"));
    }
    if let Some(t) = whole.get("tags") {
        body["tags"] = t.clone();
    }
    let types = vec!["deployments".to_string()];
    let names = vec![name.to_string()];
    // the record holds no port (outputs such as `reference(...).serviceBusEndpoint` have one); answers fill in the request's
    let mut stored_body = body.clone();
    crate::util::without_port(&mut stored_body, &cx.port_suffix());
    let stored = Stored {
        id: id.clone(),
        sub: sub.to_string(),
        rg: rg.to_string(),
        ns: "Microsoft.Resources".into(),
        types,
        names,
        body: stored_body,
        extra: json!({"template": outcome.template, "operations": outcome.ops}),
        created: cx.now,
    };
    let _writing = cx.inner.begin_write(sub, rg)?;
    let existed = {
        let mut st = cx.inner.state.lock();
        let k = key(&id);
        let existed = st.resources.insert(k.clone(), stored).is_some();
        // at most 800 deployments per group, and a bounded number of bytes of history per subscription: the oldest go
        crate::history::trim(&mut st, sub, rg, &k, cx.inner.history_bytes);
        existed
    };
    Ok((if existed { 200 } else { 201 }, body))
}

async fn create(cx: &Cx<'_>, sub: &str, rg: &str, name: &str, validate_only: bool) -> ArmResult<Response> {
    let body = util::body_object(&cx.req.body)?;
    let props = body.get("properties").cloned().unwrap_or(json!({}));
    if validate_only {
        let level = Level::top();
        let outcome = match run(cx, sub, rg, name, &props, true, &level).await {
            Ok(o) => o,
            Err(e) if e.status == 400 => {
                let mut err = ArmError::bad_request(
                    "InvalidTemplateDeployment",
                    format!("The template deployment '{name}' is not valid according to the validation procedure. The tracking id is '{}'. See inner errors for details.", cx.req.ctx.request_id),
                );
                err.details.push(error_of(&e));
                return Err(err);
            }
            Err(e) => return Err(e),
        };
        let id = resource_id(sub, rg, "Microsoft.Resources", &["deployments".into()], &[name.to_string()]);
        return Ok(util::json_ok(
            200,
            &json!({"id": id, "name": name, "type": "Microsoft.Resources/deployments", "properties": {
                "templateHash": template_hash(&outcome.template),
                "parameters": outcome.params_out,
                "mode": "Incremental",
                "provisioningState": "Succeeded",
                "validatedResources": outcome.validated.iter().map(|i| json!({"id": i})).collect::<Vec<_>>(),
                "outputs": outcome.outputs,
            }}),
        ));
    }
    let (status, v) = store_deployment(cx, sub, rg, name, &props, &body, &Level::top()).await?;
    // Real Azure answers a PUT with "Accepted" and a status URL. The deployment here is already
    // finished, so the URL answers its final state at once. The SDK pollers need it to raise
    // when the state is Failed (a body that is already final is not polled at all).
    let base = if rg.is_empty() { format!("/subscriptions/{sub}") } else { format!("/subscriptions/{sub}/resourcegroups/{rg}") };
    let api = cx.req.query.get("api-version").unwrap_or("2024-03-01");
    let url = format!(
        "{}{base}/providers/Microsoft.Resources/deployments/{name}/operationStatuses/{}?api-version={api}",
        cx.arm_base(),
        uuid::Uuid::new_v4().simple()
    );
    Ok(azemu_core::resp::Resp::new(status).json(&v).header("Azure-AsyncOperation", url).build())
}

/// `.../deployments/{name}/operationStatuses/{id}`: the final state of the deployment.
pub fn operation_status(cx: &Cx<'_>, sub: &str, rg: &str, name: &str) -> ArmResult<Response> {
    let rg_name = if rg.is_empty() { String::new() } else { canonical_group_name_for(cx, sub, rg)? };
    let d = find(cx, sub, &rg_name, name)?;
    let state = d.prop_str("provisioningState").unwrap_or("Succeeded").to_string();
    let mut v = json!({"status": state});
    if state == "Failed" {
        if let Some(e) = d.body.get("properties").and_then(|p| p.get("error")) {
            v["error"] = e.clone();
        }
    }
    Ok(util::json_ok(200, &v))
}
