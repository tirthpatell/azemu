//! Running stored procedures and triggers (design and limits: `docs/services/cosmos-scripts.md`).
//!
//! A run has three steps, so that no script ever runs while the service lock is held:
//!
//! 1. under the lock (`prepare_sproc`, `prepare_doc_op`): find the script, copy the logical partition the request is
//!    scoped to (`Container::snapshot`, documents are shared, not copied) and set the copy and the script aside as a
//!    [`ScriptJob`];
//! 2. on a blocking thread (`ScriptJob::run`): run the script against the copy only. The JavaScript runs in a separate worker
//!    process (`js/proc.rs`); this thread answers the worker's collection calls from the copy and nothing else;
//! 3. under the lock again (`Container::apply_snapshot`, called by `lib.rs`): if nothing else changed the partition since
//!    step 1, apply the script's writes in one go; otherwise run again from step 1 (up to 5 times, then 449 RetryWith).
//!
//! That makes a script atomic (all of its writes or none) and isolated from other requests on its partition (it sees
//! the partition as of its start, and commits only if the partition is still the same), while other containers, and
//! other partitions of the same container, are never held up.

use std::sync::Arc;
use std::time::Instant;

use azemu_core::bytes::Bytes;
use azemu_core::Request;
use serde_json::{json, Value};

use crate::documents::{self, Loc};
use crate::errors::{CosmosError, R};
use crate::js::{self, Failure, Outcome, ScriptSettings};
use crate::out::{Body, Cx, Out};
use crate::pk::PkValue;
use crate::script_host::{HostInit, ScriptHost};
use crate::state::{Container, Snapshot};

/// A document operation that triggers can wrap.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum DocOp {
    Create { upsert: bool },
    Replace,
    Delete,
}

impl DocOp {
    fn name(self) -> &'static str {
        match self {
            DocOp::Create { upsert: false } => "Create",
            DocOp::Create { upsert: true } => "Upsert",
            DocOp::Replace => "Replace",
            DocOp::Delete => "Delete",
        }
    }

    /// Whether a trigger registered for `triggerOperation` may run on this operation.
    fn matches(self, trigger_operation: &str) -> bool {
        let t = trigger_operation.to_ascii_lowercase();
        match self {
            DocOp::Create { upsert: false } => t == "all" || t == "create",
            DocOp::Create { upsert: true } => t == "all" || t == "create" || t == "replace" || t == "update",
            DocOp::Replace => t == "all" || t == "replace" || t == "update",
            DocOp::Delete => t == "all" || t == "delete",
        }
    }
}

#[derive(Clone)]
pub struct ScriptDef {
    pub name: String,
    pub body: String,
}

enum Plan {
    Sproc { def: ScriptDef, params: Value },
    Doc { op: DocOp, id: Option<String>, pre: Vec<ScriptDef>, post: Vec<ScriptDef> },
}

/// Which container a job belongs to (looked up again to apply its writes).
pub struct Target {
    pub account: String,
    pub db: String,
    pub coll: String,
    pub rid: String,
}

pub struct ScriptJob {
    plan: Plan,
    settings: Arc<ScriptSettings>,
    target: Target,
    snap: Snapshot,
    loc: Loc,
    db_name: String,
    base: Request,
    udfs: Vec<(String, String)>,
    log: bool,
    now: i64,
    /// Set by the service for the tests: the worker panics instead of running (`CosmosService::inject_script_panic`).
    pub panic: bool,
}

/// The writes of a finished script, to be applied under the lock.
pub struct Commit {
    pub target: Target,
    pub snap: Snapshot,
    pub now: i64,
}

pub struct Done {
    pub out: R<Out>,
    pub commit: Option<Commit>,
}

fn settings_of(cx: &Cx) -> Arc<ScriptSettings> {
    cx.settings.clone().unwrap_or_default()
}

fn udfs_of(c: &Container) -> Vec<(String, String)> {
    c.udfs.iter().filter_map(|(id, r)| r.props.get("body").and_then(Value::as_str).map(|b| (id.clone(), b.to_string()))).collect()
}

fn definition(r: &crate::state::Resource) -> ScriptDef {
    ScriptDef { name: r.meta.id.clone(), body: r.props.get("body").and_then(Value::as_str).unwrap_or("").to_string() }
}

fn find<'a>(map: &'a std::collections::BTreeMap<String, crate::state::Resource>, key: &str) -> Option<&'a crate::state::Resource> {
    map.get(key).or_else(|| map.values().find(|r| crate::rid::matches(&r.meta.rid, key)))
}

/// Step 1 for `POST .../sprocs/{id}`.
pub fn prepare_sproc(cx: &Cx, c: &mut Container, loc: &Loc, db_name: &str, key: &str) -> R<Out> {
    let def = definition(find(&c.sprocs, key).ok_or_else(CosmosError::not_found_default)?);
    let params = if cx.req.body.is_empty() { Value::Array(Vec::new()) } else { cx.body_json()? };
    if !params.is_array() {
        return Err(CosmosError::bad_request("The parameters of a stored procedure must be a JSON array."));
    }
    c.purge_expired(cx.now);
    let pk = documents::require_pk(cx, c)?;
    let settings = settings_of(cx);
    let snap = c.snapshot(&pk, settings.max_operations as u64 + 8);
    let target = Target { account: cx.req.account.clone(), db: db_name.to_string(), coll: c.meta.id.clone(), rid: c.meta.rid.clone() };
    Ok(Out::script(ScriptJob {
        plan: Plan::Sproc { def, params },
        settings,
        target,
        snap,
        loc: Loc { db_rid: loc.db_rid.clone() },
        db_name: db_name.to_string(),
        base: cx.req.clone(),
        udfs: udfs_of(c),
        log: cx.flag("x-ms-documentdb-script-enable-logging"),
        now: cx.now,
        panic: false,
    }))
}

fn trigger_names(cx: &Cx, header: &str) -> Vec<String> {
    cx.header(header).map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default()
}

fn resolve(c: &Container, names: &[String], want_pre: bool, op: DocOp) -> R<Vec<ScriptDef>> {
    let kind = if want_pre { "pre" } else { "post" };
    let mut out = Vec::new();
    for n in names {
        let r = find(&c.triggers, n)
            .ok_or_else(|| CosmosError::bad_request(format!("The {kind}-trigger '{n}' does not exist in the container.")))?;
        let t = r.props.get("triggerType").and_then(Value::as_str).unwrap_or("");
        if !t.eq_ignore_ascii_case(kind) {
            return Err(CosmosError::bad_request(format!("The trigger '{n}' is a {t} trigger and cannot be used as a {kind}-trigger.")));
        }
        let o = r.props.get("triggerOperation").and_then(Value::as_str).unwrap_or("");
        if !op.matches(o) {
            return Err(CosmosError::bad_request(format!(
                "The trigger '{n}' is registered for the operation '{o}' and cannot run on a {} request.",
                op.name().to_ascii_lowercase()
            )));
        }
        out.push(definition(r));
    }
    Ok(out)
}

/// Step 1 for a document write that names triggers. `None` when it names none (the caller runs the ordinary handler).
pub fn prepare_doc_op(cx: &Cx, loc: &Loc, db_name: &str, c: &mut Container, op: DocOp, id: Option<&str>) -> R<Option<Out>> {
    let pre_names = trigger_names(cx, "x-ms-documentdb-pre-trigger-include");
    let post_names = trigger_names(cx, "x-ms-documentdb-post-trigger-include");
    if pre_names.is_empty() && post_names.is_empty() {
        return Ok(None);
    }
    let pre = resolve(c, &pre_names, true, op)?;
    let post = resolve(c, &post_names, false, op)?;
    c.purge_expired(cx.now);
    let pk: PkValue = match (documents::header_pk(cx, c)?, op) {
        (Some(p), _) => p,
        (None, DocOp::Delete) => documents::locate(cx, c, id.unwrap_or(""))?.pk,
        (None, _) => {
            let body = cx.body_json()?;
            let prepared = documents::prepare(body, c, None)?;
            prepared.pk
        }
    };
    let settings = settings_of(cx);
    let snap = c.snapshot(&pk, settings.max_operations as u64 + 8);
    let target = Target { account: cx.req.account.clone(), db: db_name.to_string(), coll: c.meta.id.clone(), rid: c.meta.rid.clone() };
    Ok(Some(Out::script(ScriptJob {
        plan: Plan::Doc { op, id: id.map(str::to_string), pre, post },
        settings,
        target,
        snap,
        loc: Loc { db_rid: loc.db_rid.clone() },
        db_name: db_name.to_string(),
        base: cx.req.clone(),
        udfs: udfs_of(c),
        log: false,
        now: cx.now,
        panic: false,
    })))
}

/// The answer of a build made without the script engine (`--no-default-features`, or without the `cosmos-scripts` feature of `azemu`).
pub(crate) const NOT_BUILT: &str = "This build of azemu does not include the script engine (it was built without the `cosmos-scripts` feature), so stored procedures, triggers and user-defined functions cannot run. Everything else in Cosmos DB works.";

/// The error a failed script answers with.
fn failure_error(f: Failure, settings: &ScriptSettings) -> CosmosError {
    match f {
        Failure::Thrown(m) | Failure::Aborted(m) => {
            CosmosError::bad_request(format!("Encountered exception while executing function. Exception = {m}\r\nStack trace: {m}"))
        }
        Failure::Limit(m) => CosmosError::bad_request(format!("Encountered exception while executing function. Exception = {m}")),
        Failure::Timeout => CosmosError::new(
            408,
            "RequestTimeout",
            format!(
                "The script ran longer than {} seconds and was stopped; everything it did is rolled back. \
                 A script that does a lot should stop when a collection call answers false and let the client call it again.",
                settings.timeout.as_secs_f64()
            ),
        ),
        Failure::Memory => CosmosError::bad_request(format!(
            "The script used more than {} MB of memory and was stopped; everything it did is rolled back.",
            settings.memory_growth / (1024 * 1024)
        )),
        Failure::Internal(m) => CosmosError::new(500, "InternalServerError", format!("The script engine failed: {m}")),
        Failure::Worker(m) => CosmosError::new(500, "InternalServerError", m),
        Failure::Cancelled => CosmosError::new(500, "InternalServerError", "The request was cancelled while the script ran."),
        Failure::NotBuilt => CosmosError::new(501, "NotImplemented", NOT_BUILT),
    }
}

/// Runs one script (a stored procedure or a trigger) in a worker process; `host` answers its collection calls.
fn exec(
    host: &mut ScriptHost,
    settings: &ScriptSettings,
    deadline: Instant,
    def: &ScriptDef,
    cfg: Value,
    args: &Value,
    panic: bool,
) -> R<Outcome> {
    js::check_source(&def.body).map_err(|m| CosmosError::bad_request(format!("The script '{}' cannot run: {m}", def.name)))?;
    js::proc::run_job(settings, deadline, &def.body, cfg, args, panic, &mut |op, payload| host.call(op, payload))
        .map_err(|f| failure_error(f, settings))
}

fn request_headers(base: &Request) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in base.headers.iter() {
        let name = k.as_str();
        if name == "authorization" {
            continue;
        }
        if let Ok(v) = v.to_str() {
            m.insert(name.to_string(), Value::String(v.to_string()));
        }
    }
    Value::Object(m)
}

/// Percent-encodes script log text the way Azure sends it in `x-ms-documentdb-script-log-results` (at most 1 KB).
fn encode_log(lines: &[String]) -> String {
    let mut text = lines.join("\n");
    if text.len() > 1024 {
        let mut end = 1024;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl ScriptJob {
    /// Step 2: run the script on the copy of the partition.
    pub fn run(self) -> Done {
        let ScriptJob { plan, settings, target, snap, loc, db_name, base, udfs, log, now, panic } = self;
        let deadline = Instant::now() + settings.timeout;
        let pk = snap.pk.clone();
        let expect = snap.expect.clone();
        let host = ScriptHost::new(HostInit {
            scratch: snap.scratch,
            loc,
            db_name: db_name.clone(),
            coll_name: target.coll.clone(),
            base: base.clone(),
            pk: pk.clone(),
            deadline,
            udfs,
            settings: settings.clone(),
        });
        let result = match plan {
            Plan::Sproc { def, params } => run_sproc(host, &settings, deadline, &def, &params, log, &base, panic),
            Plan::Doc { op, id, pre, post } => run_doc(host, &settings, deadline, op, id.as_deref(), &pre, &post, &base, panic),
        };
        match result {
            Err(e) => Done { out: Err(e), commit: None },
            Ok((out, host)) => {
                let commit =
                    if host.wrote() { Some(Commit { target, snap: Snapshot { scratch: host.scratch, expect, pk }, now }) } else { None };
                Done { out: Ok(out), commit }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_sproc(
    mut host: ScriptHost,
    settings: &ScriptSettings,
    deadline: Instant,
    def: &ScriptDef,
    params: &Value,
    log: bool,
    base: &Request,
    panic: bool,
) -> R<(Out, ScriptHost)> {
    let cfg = json!({
        "mode": "sproc",
        "selfLink": host.self_link,
        "altLink": host.alt_link,
        "requestBodyJson": params.to_string(),
        "requestHeaders": request_headers(base),
        "responseHeaders": {},
    });
    let outcome = exec(&mut host, settings, deadline, def, cfg, params, panic)?;
    let mut out = match outcome.body {
        Some(text) => {
            if text.len() > settings.max_response {
                return Err(CosmosError::bad_request(format!(
                    "The response of the script is larger than {} MB.",
                    settings.max_response / (1024 * 1024)
                )));
            }
            let v: Value = serde_json::from_str(&text)
                .map_err(|_| CosmosError::new(500, "InternalServerError", "The script response is not JSON."))?;
            Out::json(200, v)
        }
        None => Out::empty(200),
    };
    out = out.charge(host.charge + 1.0).lsn(host.scratch.lsn);
    if log {
        out = out.header("x-ms-documentdb-script-log-results", encode_log(&host.log));
    }
    Ok((out, host))
}

#[allow(clippy::too_many_arguments)]
fn run_doc(
    mut host: ScriptHost,
    settings: &ScriptSettings,
    deadline: Instant,
    op: DocOp,
    id: Option<&str>,
    pre: &[ScriptDef],
    post: &[ScriptDef],
    base: &Request,
    panic: bool,
) -> R<(Out, ScriptHost)> {
    let mut req = base.clone();
    let has_body = !matches!(op, DocOp::Delete);
    let mut body_text: Option<String> = if has_body { Some(String::from_utf8_lossy(&req.body).into_owned()) } else { None };
    for def in pre {
        let mut cfg = json!({
            "mode": "pre",
            "operation": op.name(),
            "selfLink": host.self_link,
            "altLink": host.alt_link,
            "requestHeaders": request_headers(base),
            "responseHeaders": {},
        });
        if let Some(b) = &body_text {
            cfg["requestBodyJson"] = Value::String(b.clone());
        }
        let outcome = exec(&mut host, settings, deadline, def, cfg, &json!([]), panic)?;
        if outcome.request_body_set {
            let text = outcome
                .request_body
                .ok_or_else(|| CosmosError::bad_request("A pre-trigger set the request body to undefined, which is not a document."))?;
            req.body = Bytes::from(text.clone());
            body_text = Some(text);
        }
    }
    let mut out = host.run_doc_op(&req, op, id)?;
    for def in post {
        let response = match &out.body {
            Body::Json(v) => Some(v.to_string()),
            _ => None,
        };
        let mut cfg = json!({
            "mode": "post",
            "operation": op.name(),
            "selfLink": host.self_link,
            "altLink": host.alt_link,
            "requestHeaders": request_headers(base),
            "responseHeaders": {},
        });
        if let Some(b) = &body_text {
            cfg["requestBodyJson"] = Value::String(b.clone());
        }
        if let Some(r) = response {
            cfg["responseBodyJson"] = Value::String(r);
        }
        let outcome = exec(&mut host, settings, deadline, def, cfg, &json!([]), panic)?;
        if outcome.body_set {
            if let Some(text) = outcome.body {
                let v: Value = serde_json::from_str(&text)
                    .map_err(|_| CosmosError::new(500, "InternalServerError", "The trigger response is not JSON."))?;
                out.body = Body::Json(v);
            }
        }
    }
    out = out.lsn(host.scratch.lsn);
    Ok((out, host))
}
