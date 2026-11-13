//! Admin API under `/_azemu/eventgrid`: declare topics and subscriptions (on Azure this is
//! ARM's job), list them and read the state of a subscription.
//!
//! | Request | Result |
//! |---|---|
//! | `GET /` | summary of every topic and the service settings |
//! | `GET /topics` | list of topics |
//! | `PUT /topics/{name}[?namespace=N\|domain=D]` | create or update a topic (body `{"kind":"custom\|domain\|namespace","inputSchema":…}`) |
//! | `GET /topics/{name}` / `DELETE /topics/{name}` | read / delete |
//! | `GET /topics/{name}/subscriptions` | list |
//! | `PUT /topics/{name}/subscriptions/{sub}` | create or replace a subscription |
//! | `GET` / `DELETE` the same path | read (with live counters) / delete |
//! | `GET …/{sub}/deliveries` | push delivery log, oldest first |
//! | `GET …/{sub}/deadletters` | dead-lettered events |
//! | `GET …/{sub}/events` | events still queued (pull) |
//! | `POST …/{sub}/validate` | run the webhook handshake again |
//! | `GET /validate/{code}` | the manual validation URL handed to webhooks |
//!
//! A topic is found by `?namespace=N` (namespace topic), `?domain=D` (domain topic), else
//! by the host of that name (custom topic or domain), else as a topic of the namespace
//! `default`.

use std::sync::Arc;

use azemu_core::resp::Resp;
use azemu_core::{Request, Response};
use serde_json::{json, Value};

use crate::model::{InputSchema, TopicConfig, TopicKind};
use crate::state::{key, Host, State, Topic};
use crate::{admin_subs, Shared};

pub const DEFAULT_NAMESPACE: &str = "default";

/// A resolved address: lower-case keys plus the kind of the owning host.
#[derive(Clone, Debug)]
pub struct Scope {
    pub host: String,
    pub topic: Option<String>,
    pub kind: TopicKind,
}

pub fn json_resp(status: u16, v: &Value) -> Response {
    Resp::new(status).json(v).build()
}

pub fn admin_error(status: u16, code: &str, message: &str) -> Response {
    Resp::new(status).json(&json!({ "error": { "code": code, "message": message } })).build()
}

pub async fn handle(shared: &Arc<Shared>, req: &Request) -> Response {
    let segs = req.segments();
    let rest: Vec<&str> = segs.iter().skip(2).map(String::as_str).collect();
    let method = req.method.as_str();
    match (method, rest.as_slice()) {
        ("GET", []) | ("GET", ["status"]) => status(shared),
        ("GET", ["topics"]) => json_resp(200, &json!({ "value": shared.state.lock().topic_summaries() })),
        ("PUT", ["topics", t]) => put_topic(shared, req, t),
        ("GET", ["topics", t]) => get_topic(shared, req, t),
        ("DELETE", ["topics", t]) => delete_topic(shared, req, t),
        ("GET", ["validate", code]) => admin_subs::manual_validate(shared, code),
        ("GET", ["sources"]) => json_resp(200, &json!({ "value": shared.state.lock().source_summaries() })),
        ("GET", ["sources", kind, name, "subscriptions"]) => with_source(kind, name, |k| admin_subs::list(shared, req, k)),
        ("PUT", ["sources", kind, name, "subscriptions", s]) => match source_key(kind, name) {
            Ok(src) => {
                {
                    let mut st = shared.state.lock();
                    crate::sources::ensure(&mut st, &src);
                }
                admin_subs::put(shared, req, src.key(), s).await
            }
            Err(r) => r,
        },
        ("GET", ["sources", kind, name, "subscriptions", s]) => with_source(kind, name, |k| admin_subs::get(shared, req, k, s)),
        ("DELETE", ["sources", kind, name, "subscriptions", s]) => with_source(kind, name, |k| admin_subs::delete(shared, req, k, s)),
        ("GET", ["sources", kind, name, "subscriptions", s, what @ ("deliveries" | "deadletters")]) => {
            with_source(kind, name, |k| admin_subs::inspect(shared, req, k, s, what))
        }
        ("POST", ["sources", kind, name, "subscriptions", s, "validate"]) => match source_key(kind, name) {
            Ok(src) => admin_subs::revalidate(shared, req, src.key(), s).await,
            Err(r) => r,
        },
        ("GET", ["topics", t, "subscriptions"]) => admin_subs::list(shared, req, t),
        ("PUT", ["topics", t, "subscriptions", s]) => admin_subs::put(shared, req, t, s).await,
        ("GET", ["topics", t, "subscriptions", s]) => admin_subs::get(shared, req, t, s),
        ("DELETE", ["topics", t, "subscriptions", s]) => admin_subs::delete(shared, req, t, s),
        ("GET", ["topics", t, "subscriptions", s, what @ ("deliveries" | "deadletters" | "events")]) => {
            admin_subs::inspect(shared, req, t, s, what)
        }
        ("POST", ["topics", t, "subscriptions", s, "validate"]) => admin_subs::revalidate(shared, req, t, s).await,
        _ => errors_for(method, &req.decoded_path()),
    }
}

/// `storage` and an account name name the events of that storage account.
fn source_key(kind: &str, name: &str) -> Result<crate::SourceRef, Response> {
    match kind.to_ascii_lowercase().as_str() {
        "storage" | "storageaccount" | "storageaccounts" => Ok(crate::SourceRef::storage_account(name)),
        "keyvault" | "vault" | "vaults" => Ok(crate::SourceRef::key_vault(name)),
        "appconfig" | "appconfiguration" | "configurationstores" => Ok(crate::SourceRef::app_configuration_store(name)),
        _ => Err(admin_error(
            404,
            "NotFound",
            &format!("Unknown event source kind '{kind}'. Built: storage (a storage account), keyvault (a vault), appconfig (an App Configuration store)."),
        )),
    }
}

fn with_source(kind: &str, name: &str, f: impl FnOnce(&str) -> Response) -> Response {
    match source_key(kind, name) {
        Ok(src) => f(src.key()),
        Err(r) => r,
    }
}

fn errors_for(method: &str, path: &str) -> Response {
    admin_error(501, "NotImplemented", &format!("eventgrid admin: {method} {path}"))
}

fn status(shared: &Arc<Shared>) -> Response {
    let st = shared.state.lock();
    let o = &shared.opts;
    json_resp(
        200,
        &json!({
            "autocreate": o.autocreate,
            "configFile": o.config_path.as_ref().map(|p| p.display().to_string()),
            "retryBaseMs": o.retry_base_ms,
            "retryMaxMs": o.retry_max_ms,
            "deliveryTimeoutMs": o.delivery_timeout_ms,
            "topics": st.topic_summaries(),
        }),
    )
}

/// Find an existing topic or domain by name and the query parameters.
pub fn resolve(st: &State, name: &str, req: &Request) -> Result<Scope, Response> {
    let not_found = |what: String| admin_error(404, "NotFound", &what);
    if let Some(ns) = req.query.get("namespace") {
        let h = st.host(ns).filter(|h| h.kind == TopicKind::Namespace);
        return match h {
            Some(h) if h.topics.contains_key(&key(name)) => Ok(Scope { host: key(ns), topic: Some(key(name)), kind: TopicKind::Namespace }),
            Some(_) => Err(not_found(format!("The topic '{name}' was not found in the namespace '{ns}'."))),
            None => Err(not_found(format!("The namespace '{ns}' was not found."))),
        };
    }
    if let Some(d) = req.query.get("domain") {
        let h = st.host(d).filter(|h| h.kind == TopicKind::Domain);
        return match h {
            Some(h) if h.topics.contains_key(&key(name)) => Ok(Scope { host: key(d), topic: Some(key(name)), kind: TopicKind::Domain }),
            Some(_) => Err(not_found(format!("The domain topic '{name}' was not found in the domain '{d}'."))),
            None => Err(not_found(format!("The domain '{d}' was not found."))),
        };
    }
    if let Some(h) = st.host(name) {
        if h.kind != TopicKind::Namespace {
            return Ok(Scope { host: key(name), topic: None, kind: h.kind });
        }
    }
    if let Some(h) = st.host(DEFAULT_NAMESPACE).filter(|h| h.kind == TopicKind::Namespace) {
        if h.topics.contains_key(&key(name)) {
            return Ok(Scope { host: key(DEFAULT_NAMESPACE), topic: Some(key(name)), kind: TopicKind::Namespace });
        }
    }
    Err(not_found(format!("The topic '{name}' was not found.")))
}

fn parse_body<T: serde::de::DeserializeOwned + Default>(req: &Request) -> Result<T, Response> {
    if req.body.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(T::default());
    }
    serde_json::from_slice(&req.body).map_err(|e| admin_error(400, "BadRequest", &format!("Invalid JSON body: {e}")))
}

pub fn topic_view(st: &State, s: &Scope) -> Value {
    let Some(h) = st.hosts.get(&s.host) else { return Value::Null };
    match &s.topic {
        None => json!({
            "name": h.name,
            "kind": h.kind.label(),
            "inputSchema": h.input_schema,
            "endpoint": format!("{}.eventgrid", h.name),
            "subscriptions": h.subs.values().map(|x| x.name.clone()).collect::<Vec<_>>(),
            "topics": h.topics.values().map(|t| t.name.clone()).collect::<Vec<_>>(),
            "published": h.published,
        }),
        Some(t) => {
            let topic = h.topics.get(t);
            json!({
                "name": topic.map(|t| t.name.clone()),
                "kind": if h.kind == TopicKind::Namespace { "namespaceTopic" } else { "domainTopic" },
                "inputSchema": topic.map(|t| t.input_schema),
                "namespace": (h.kind == TopicKind::Namespace).then(|| h.name.clone()),
                "domain": (h.kind == TopicKind::Domain).then(|| h.name.clone()),
                "subscriptions": topic.map(|t| t.subs.values().map(|x| x.name.clone()).collect::<Vec<_>>()),
                "published": topic.map(|t| t.published),
            })
        }
    }
}

fn put_topic(shared: &Arc<Shared>, req: &Request, name: &str) -> Response {
    let cfg: TopicConfig = match parse_body(req) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if name.trim().is_empty() || name.contains('/') {
        return admin_error(400, "BadRequest", "The topic name must not be empty or contain '/'.");
    }
    let mut st = shared.state.lock();
    let namespace = req.query.get("namespace").map(str::to_string);
    let domain = req.query.get("domain").map(str::to_string);
    let (host_name, host_kind, topic): (String, TopicKind, Option<&str>) = if namespace.is_some() || cfg.kind == Some(TopicKind::Namespace)
    {
        (namespace.unwrap_or_else(|| DEFAULT_NAMESPACE.to_string()), TopicKind::Namespace, Some(name))
    } else if let Some(d) = domain {
        (d, TopicKind::Domain, Some(name))
    } else {
        (name.to_string(), cfg.kind.unwrap_or(TopicKind::Custom), None)
    };
    let given_resource_id = cfg.resource_id.clone().filter(|_| topic.is_none() && host_kind == TopicKind::Custom);
    let mut created = false;
    match st.host(&host_name).map(|h| h.kind) {
        Some(k) if k != host_kind => {
            return admin_error(
                409,
                "Conflict",
                &format!("'{host_name}' already exists as a {} and cannot be used as a {}.", k.label(), host_kind.label()),
            )
        }
        Some(_) => {}
        None => {
            let input = if host_kind == TopicKind::Namespace { InputSchema::CloudEvents } else { cfg.input_schema };
            st.hosts.insert(key(&host_name), Host::new(&host_name, host_kind, input));
            created = topic.is_none();
        }
    }
    if let Some(t) = topic {
        if let Some(h) = st.host_mut(&host_name) {
            if let std::collections::btree_map::Entry::Vacant(v) = h.topics.entry(key(t)) {
                v.insert(Topic { name: t.to_string(), input_schema: cfg.input_schema, ..Topic::default() });
                created = true;
            }
        }
    } else if let Some(h) = st.host_mut(&host_name) {
        if !created && host_kind != TopicKind::Namespace {
            h.input_schema = cfg.input_schema;
        }
    }
    if let (Some(id), Some(h)) = (given_resource_id, st.host_mut(&host_name)) {
        h.resource_id = Some(id);
    }
    let scope = Scope { host: key(&host_name), topic: topic.map(key), kind: host_kind };
    json_resp(if created { 201 } else { 200 }, &topic_view(&st, &scope))
}

fn get_topic(shared: &Arc<Shared>, req: &Request, name: &str) -> Response {
    let st = shared.state.lock();
    match resolve(&st, name, req) {
        Ok(s) => json_resp(200, &topic_view(&st, &s)),
        Err(r) => r,
    }
}

fn delete_topic(shared: &Arc<Shared>, req: &Request, name: &str) -> Response {
    let mut st = shared.state.lock();
    let scope = match resolve(&st, name, req) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match &scope.topic {
        Some(t) => {
            if let Some(h) = st.hosts.get_mut(&scope.host) {
                h.topics.remove(t);
            }
        }
        None => {
            st.hosts.remove(&scope.host);
        }
    }
    drop(st);
    shared.notify.notify_waiters();
    Resp::new(204u16).build()
}
