//! What a deployment needs to evaluate expressions: the template's parameters and variables, the resources of the
//! request (for `reference`), the symbolic names of a `languageVersion` 2.0 template and the answers of `list*` calls.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use crate::cx::Cx;
use crate::limits::{Budget, Memo};
use crate::provider::ActionTarget;
use crate::state::{key, Stored};
use crate::template::dotnet::Dt;
use crate::template::userfn::Functions;
use crate::template::{cloud_environment, eval_value, Env, EvalError, ListCall, RemoteError};

/// How many `list*` calls one expression may make (each runs a provider action once, and is then kept).
const MAX_LIST_ROUNDS: usize = 512;

/// The resource or the copies of a resource that a symbolic name stands for, as resource ids.
pub(super) struct Symbol {
    pub ids: Vec<String>,
    pub looped: bool,
}

/// Everything needed to evaluate expressions, owned so no borrow crosses an `.await`.
pub(super) struct Scope {
    pub sub: String,
    pub rg: String,
    pub params: Map<String, Value>,
    pub variables: Map<String, Value>,
    pub rg_info: Value,
    pub sub_info: Value,
    pub tenant: String,
    pub deployment: Value,
    /// Work counters of the whole request, shared with nested deployments.
    pub budget: Arc<Budget>,
    /// Variables of this template that are already evaluated.
    pub memo: Arc<Memo>,
    pub funcs: Functions,
    /// The value of `environment()`.
    pub cloud: Value,
    /// Lower case symbolic name to its resources (`languageVersion` 2.0).
    pub symbols: HashMap<String, Symbol>,
    /// Answers of `list*` calls that already ran, by call (or the error text).
    actions: Mutex<HashMap<String, Result<Value, RemoteError>>>,
    /// Lower case ids, names and symbolic names of the resources this template deploys: a `list*` call on one of them waits
    /// for it, on anything else it asks the service at once.
    pub declared: HashSet<String>,
}

impl Scope {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cx: &Cx<'_>,
        sub: &str,
        rg: &str,
        variables: Map<String, Value>,
        rg_info: Value,
        sub_info: Value,
        tenant: String,
        deployment: Value,
        budget: Arc<Budget>,
        funcs: Functions,
    ) -> Scope {
        Scope {
            sub: sub.to_string(),
            rg: rg.to_string(),
            params: Map::new(),
            variables,
            rg_info,
            sub_info,
            tenant,
            deployment,
            budget,
            memo: Arc::new(Memo::default()),
            funcs,
            cloud: cloud_environment(&cx.req.ctx.endpoint.authority),
            symbols: HashMap::new(),
            actions: Mutex::new(HashMap::new()),
            declared: HashSet::new(),
        }
    }

    fn with_env_mode<R>(&self, cx: &Cx<'_>, copy: &[(String, i64)], defaults: bool, f: impl FnOnce(&Env<'_>) -> R) -> R {
        let look = |arg: &str, full: bool| self.lookup(cx, arg, full);
        let list = |call: &ListCall| self.list(cx, call);
        let syms = |name: &str| self.symbols.get(&name.to_ascii_lowercase()).map(|s| (s.ids.clone(), s.looped));
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
        .with_shared(self.budget.clone(), self.memo.clone())
        .with_funcs(&self.funcs)
        .with_cloud(&self.cloud)
        .with_list(&list)
        .with_symbols(&syms)
        .with_now(Dt { secs: cx.now.unix_timestamp(), ticks: cx.now.nanosecond() / 100 });
        if defaults {
            env = env.for_defaults();
        }
        env.copy = copy.to_vec();
        f(&env)
    }

    pub fn with_env<R>(&self, cx: &Cx<'_>, copy: &[(String, i64)], f: impl FnOnce(&Env<'_>) -> R) -> R {
        self.with_env_mode(cx, copy, false, f)
    }

    pub fn eval(&self, cx: &Cx<'_>, copy: &[(String, i64)], v: &Value) -> Result<Value, EvalError> {
        self.with_env(cx, copy, |e| eval_value(v, e))
    }

    /// The default value of a parameter: the one place `utcNow` and `newGuid` are allowed.
    pub fn eval_default(&self, cx: &Cx<'_>, v: &Value) -> Result<Value, EvalError> {
        self.with_env_mode(cx, &[], true, |e| eval_value(v, e))
    }

    /// Evaluate with `f`, running the `list*` actions it asks for and trying again, until it has an answer.
    pub async fn settle<T>(&self, cx: &Cx<'_>, mut f: impl FnMut() -> Result<T, EvalError>) -> Result<T, EvalError> {
        for _ in 0..MAX_LIST_ROUNDS {
            match f() {
                Err(EvalError::Action(call)) => self.run_action(cx, &call).await,
                other => return other,
            }
        }
        Err(EvalError::Invalid(format!("An expression makes more than {MAX_LIST_ROUNDS} different list calls.")))
    }

    /// The stored resource an expression names: a symbolic name, a resource id or a resource name.
    fn find(&self, cx: &Cx<'_>, arg: &str) -> Result<Option<Stored>, EvalError> {
        let arg = match self.symbols.get(&arg.to_ascii_lowercase()) {
            Some(s) if s.looped => {
                return Err(EvalError::Msg(format!(
                    "'{arg}' is a resource with a copy loop: use references('{arg}') for all of them, or its name or id for one."
                )))
            }
            Some(s) => s.ids.first().cloned().unwrap_or_else(|| arg.to_string()),
            None => arg.to_string(),
        };
        let st = cx.inner.state.lock();
        if arg.starts_with("/subscriptions") {
            return Ok(st.resources.get(&key(&arg)).cloned());
        }
        let prefix = if self.rg.is_empty() {
            format!("/subscriptions/{}/", self.sub.to_ascii_lowercase())
        } else {
            format!("{}/", crate::state::group_key(&self.sub, &self.rg))
        };
        let want = arg.to_ascii_lowercase();
        Ok(st
            .resources
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(_, s)| s)
            .filter(|s| {
                s.names.join("/").to_ascii_lowercase() == want
                    || s.name().eq_ignore_ascii_case(&arg)
                    || format!("{}/{}", s.type_string(), s.names.join("/")).eq_ignore_ascii_case(&arg)
            })
            .min_by_key(|s| s.types.len())
            .cloned())
    }

    /// How an error names what an expression asked for: a symbolic name comes with the id it stands for.
    fn named(&self, arg: &str) -> String {
        match self.symbols.get(&arg.to_ascii_lowercase()).and_then(|s| s.ids.first()) {
            Some(id) => format!("{arg}' ('{id}"),
            None => arg.to_string(),
        }
    }

    /// `reference(...)`.
    fn lookup(&self, cx: &Cx<'_>, arg: &str, full: bool) -> Result<Value, EvalError> {
        let Some(s) = self.find(cx, arg)? else {
            return Err(EvalError::Defer(format!("The Resource '{}' is not deployed yet or does not exist.", self.named(arg))));
        };
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

    fn cache_key(call: &ListCall) -> String {
        format!("{}|{}|{}|{}", call.function.to_ascii_lowercase(), call.target.to_ascii_lowercase(), call.api_version, call.body)
    }

    /// `listKeys(...)` and the like: the answer if the action already ran, otherwise a request to run it.
    fn list(&self, cx: &Cx<'_>, call: &ListCall) -> Result<Value, EvalError> {
        let Some(stored) = self.find(cx, &call.target)? else {
            let t = call.target.to_ascii_lowercase();
            // a resource of the template, or one that the provider makes with it (the `RootManageSharedAccessKey` rule of a namespace)
            let child_of_declared =
                self.declared.iter().any(|d| d.starts_with('/') && t.strip_prefix(d.as_str()).is_some_and(|rest| rest.starts_with('/')));
            if self.declared.contains(&t) || self.symbols.contains_key(&t) || child_of_declared {
                return Err(EvalError::Defer(format!(
                    "The Resource '{}' is not deployed yet or does not exist.",
                    self.named(&call.target)
                )));
            }
            // not part of the template and not there: Azure makes the POST and reports what came back
            return Err(EvalError::Remote(Box::new(self.unreachable(cx, call))));
        };
        let resolved = ListCall { target: stored.id.clone(), ..call.clone() };
        match self.actions.lock().get(&Self::cache_key(&resolved)) {
            Some(Ok(v)) => Ok(v.clone()),
            Some(Err(e)) => Err(EvalError::Remote(Box::new(e.clone()))),
            None => Err(EvalError::Action(Box::new(resolved))),
        }
    }

    /// The error of a `list*` call on something that is not a stored resource: a resource group or subscription (no such
    /// action: Azure's `NotFound` for the URL), or a resource that does not exist (`ResourceNotFound`).
    fn unreachable(&self, cx: &Cx<'_>, call: &ListCall) -> RemoteError {
        let id = call.target.trim_end_matches('/');
        let parts: Vec<&str> = id.trim_start_matches('/').split('/').collect();
        let is_group =
            parts.len() == 4 && parts[0].eq_ignore_ascii_case("subscriptions") && parts[2].eq_ignore_ascii_case("resourceGroups");
        let is_sub = parts.len() == 2 && parts[0].eq_ignore_ascii_case("subscriptions");
        if is_group || is_sub {
            let shown = if is_group {
                format!("/subscriptions/{}/providers/Microsoft.Resources/resourceGroups/{}", parts[1], parts[3])
            } else {
                id.to_string()
            };
            return no_action_error(cx, call, &shown);
        }
        let message = match parts.iter().position(|p| p.eq_ignore_ascii_case("providers")) {
            Some(at) if parts.len() > at + 3 => {
                let rg = if parts.len() > 3 && parts[2].eq_ignore_ascii_case("resourceGroups") { parts[3] } else { "" };
                format!(
                    "The Resource '{}' under resource group '{rg}' was not found. For more details please go to https://aka.ms/ARMResourceNotFoundFix",
                    parts[at + 1..].join("/")
                )
            }
            _ => {
                format!("The Resource '{}' was not found. For more details please go to https://aka.ms/ARMResourceNotFoundFix", call.target)
            }
        };
        RemoteError { code: "ResourceNotFound".into(), target: None, message }
    }

    /// Run the action a `list*` call stands for, with the code of the direct `POST .../{action}` request, and keep the answer.
    async fn run_action(&self, cx: &Cx<'_>, call: &ListCall) {
        let answer = self.action_answer(cx, call).await;
        self.actions.lock().insert(Self::cache_key(call), answer);
    }

    async fn action_answer(&self, cx: &Cx<'_>, call: &ListCall) -> Result<Value, RemoteError> {
        let stored = cx.inner.state.lock().resources.get(&key(&call.target)).cloned();
        let Some(stored) = stored else {
            return Err(self.unreachable(cx, call));
        };
        let action = call.function.to_ascii_lowercase();
        let Some(p) = cx.inner.provider(&stored.ns) else {
            return Err(no_action_error(cx, call, &stored.id));
        };
        match p.action(cx, ActionTarget::Resource(&stored), &stored.type_path(), &action, &call.body).await {
            Ok(Some(resp)) => {
                let (status, body) = (resp.status().as_u16(), resp.into_body());
                let plain = |m: String| RemoteError { code: "InternalServerError".into(), target: None, message: m };
                if !(200..300).contains(&status) {
                    return Err(plain(format!("The function '{}' answered {status}.", call.function)));
                }
                serde_json::from_slice(&body)
                    .map_err(|_| plain(format!("The function '{}' answered something that is not JSON.", call.function)))
            }
            Ok(None) => Err(no_action_error(cx, call, &stored.id)),
            Err(e) if e.status == 501 => Err(no_action_error(cx, call, &stored.id)),
            Err(e) => Err(RemoteError { code: e.code, target: Some(stored.id.clone()), message: e.message }),
        }
    }
}

/// What Azure answers for a `list*` function on a resource that has no such action: it makes the POST, and the service
/// says there is no HTTP resource at that address (code `NotFound`, the answer's text as the message).
fn no_action_error(cx: &Cx<'_>, call: &ListCall, id: &str) -> RemoteError {
    RemoteError {
        code: "NotFound".into(),
        target: Some(id.to_string()),
        message: format!(
            "{{\r\n  \"message\": \"No HTTP resource was found that matches the request URI '{}{id}/{}?api-version={}'.\"\r\n}}",
            cx.arm_base(),
            call.function,
            call.api_version
        ),
    }
}
