//! Admin API: subscriptions (create, read, list, delete, delivery log, dead letters,
//! validation). See `admin.rs` for the route table.

use std::collections::BTreeMap;
use std::sync::Arc;

use azemu_core::resp::Resp;
use azemu_core::{Request, Response};
use serde_json::{json, Value};

use crate::admin::{admin_error, json_resp, resolve, Scope};
use crate::events::{topic_resource_id, Schema};
use crate::model::{DeliveryMode, SubscriptionConfig, TopicKind};
use crate::pull::Now;
use crate::state::{key, State, Sub, Validation};
use crate::{push, Shared};

fn subs_of<'a>(st: &'a mut State, s: &Scope) -> Option<&'a mut BTreeMap<String, Sub>> {
    let h = st.hosts.get_mut(&s.host)?;
    match &s.topic {
        None => Some(&mut h.subs),
        Some(t) => Some(&mut h.topics.get_mut(t)?.subs),
    }
}

fn subs_ref<'a>(st: &'a State, s: &Scope) -> Option<&'a BTreeMap<String, Sub>> {
    let h = st.hosts.get(&s.host)?;
    match &s.topic {
        None => Some(&h.subs),
        Some(t) => Some(&h.topics.get(t)?.subs),
    }
}

pub fn sub_view(sub: &Sub, now: &Now) -> Value {
    let mut v = serde_json::to_value(&sub.cfg).unwrap_or_else(|_| json!({}));
    let (available, locked, delayed) = sub.queue.counts(now);
    if let Some(o) = v.as_object_mut() {
        o.insert("name".into(), json!(sub.name));
        o.insert("deliveryMode".into(), json!(sub.cfg.mode()));
        o.insert("createdAt".into(), json!(azemu_core::time::iso8601_7(sub.created)));
        o.insert("validation".into(), serde_json::to_value(&sub.validation).unwrap_or(Value::Null));
        o.insert(
            "stats".into(),
            json!({
                "available": available,
                "locked": locked,
                "delayed": delayed,
                "deadLettered": sub.queue.dead.len(),
                "enqueued": sub.queue.total_enqueued,
                "acknowledged": sub.queue.total_acknowledged,
                "delivered": sub.delivered,
                "failedAttempts": sub.failed_attempts,
                "pendingPush": sub.pending.len(),
                "pendingPushBytes": sub.pending_bytes,
                "shedPush": sub.shed,
            }),
        );
    }
    v
}

pub fn list(shared: &Arc<Shared>, req: &Request, topic: &str) -> Response {
    let st = shared.state.lock();
    let scope = match resolve(&st, topic, req) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let now = Now::now();
    let value: Vec<Value> = subs_ref(&st, &scope).map(|m| m.values().map(|s| sub_view(s, &now)).collect()).unwrap_or_default();
    json_resp(200, &json!({ "value": value }))
}

pub fn get(shared: &Arc<Shared>, req: &Request, topic: &str, sub: &str) -> Response {
    let st = shared.state.lock();
    let scope = match resolve(&st, topic, req) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match subs_ref(&st, &scope).and_then(|m| m.get(&key(sub))) {
        Some(s) => json_resp(200, &sub_view(s, &Now::now())),
        None => sub_missing(topic, sub),
    }
}

fn sub_missing(topic: &str, sub: &str) -> Response {
    admin_error(404, "NotFound", &format!("The subscription '{sub}' was not found on the topic '{topic}'."))
}

pub fn delete(shared: &Arc<Shared>, req: &Request, topic: &str, sub: &str) -> Response {
    let mut st = shared.state.lock();
    let scope = match resolve(&st, topic, req) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let removed = subs_of(&mut st, &scope).and_then(|m| m.remove(&key(sub)));
    drop(st);
    shared.notify.notify_waiters();
    match removed {
        Some(_) => Resp::new(204u16).build(),
        None => sub_missing(topic, sub),
    }
}

pub fn inspect(shared: &Arc<Shared>, req: &Request, topic: &str, sub: &str, what: &str) -> Response {
    let st = shared.state.lock();
    let scope = match resolve(&st, topic, req) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(s) = subs_ref(&st, &scope).and_then(|m| m.get(&key(sub))) else { return sub_missing(topic, sub) };
    let now = Now::now();
    let value: Vec<Value> = match what {
        "deliveries" => s.log.iter().filter_map(|a| serde_json::to_value(a).ok()).collect(),
        "deadletters" => s.queue.dead.iter().filter_map(|d| serde_json::to_value(d).ok()).collect(),
        _ => s
            .queue
            .snapshot(&now)
            .into_iter()
            .map(|(event, count, state)| json!({ "event": event, "deliveryCount": count, "state": state }))
            .collect(),
    };
    json_resp(200, &json!({ "value": value }))
}

/// What the handshake needs to know about where the subscription is.
struct Target {
    scope: Scope,
    topic_id: String,
    ce: bool,
}

fn target(st: &State, scope: Scope, cfg: &SubscriptionConfig) -> Target {
    let h = st.hosts.get(&scope.host);
    let name = h.map(|h| h.name.clone()).unwrap_or_default();
    let topic_name = scope.topic.as_ref().and_then(|t| h.and_then(|h| h.topics.get(t)).map(|t| t.name.clone()));
    let topic_id = match scope.kind {
        TopicKind::Namespace => format!(
            "/subscriptions/{}/resourceGroups/azemu/providers/Microsoft.EventGrid/namespaces/{name}/topics/{}",
            crate::events::FAKE_SUBSCRIPTION,
            topic_name.unwrap_or_default()
        ),
        TopicKind::Source => h.and_then(|h| h.resource_id.clone()).unwrap_or_default(),
        k => {
            h.and_then(|h| h.resource_id.clone()).unwrap_or_else(|| topic_resource_id(&name, topic_name.as_deref(), k == TopicKind::Domain))
        }
    };
    let input = h.and_then(|h| h.input_schema.schema());
    let ce = cfg.delivery_schema.or(input) == Some(Schema::CloudEvents);
    Target { scope, topic_id, ce }
}

fn validation_url(base: &str, code: &str) -> String {
    format!("{base}/_azemu/eventgrid/validate/{code}")
}

async fn run_handshake(shared: &Arc<Shared>, base: &str, t: &Target, url: &str) -> Validation {
    let code = uuid::Uuid::new_v4().to_string();
    let mut v = push::handshake(&shared.opts, url, &t.topic_id, &validation_url(base, &code), t.ce, &code).await;
    if v.status == "AwaitingManualAction" {
        v.code = Some(code);
    }
    v
}

pub async fn put(shared: &Arc<Shared>, req: &Request, topic: &str, sub: &str) -> Response {
    if sub.trim().is_empty() || sub.contains('/') {
        return admin_error(400, "BadRequest", "The subscription name must not be empty or contain '/'.");
    }
    let cfg: SubscriptionConfig = if req.body.iter().all(|b| b.is_ascii_whitespace()) {
        SubscriptionConfig::default()
    } else {
        match serde_json::from_slice(&req.body) {
            Ok(c) => c,
            Err(e) => return admin_error(400, "BadRequest", &format!("Invalid JSON body: {e}")),
        }
    };
    let scope = {
        let st = shared.state.lock();
        match resolve(&st, topic, req) {
            Ok(s) => s,
            Err(r) => return r,
        }
    };
    let base = req.ctx.endpoint.base_url();
    match create(shared, scope, topic, sub, cfg, Some(&base)).await {
        Ok((created, view)) => json_resp(if created { 201 } else { 200 }, &view),
        Err((status, message)) => admin_error(
            status,
            if status == 501 {
                "NotImplemented"
            } else if status == 404 {
                "NotFound"
            } else {
                "BadRequest"
            },
            &message,
        ),
    }
}

/// Make or replace a subscription in `scope`: check it, run the webhook handshake when `base`
/// (the URL of this emulator, for the manual validation link) is given, store it. `Err` is the
/// status and message of the answer. Shared by the admin API and the Rust API for other services.
pub(crate) async fn create(
    shared: &Arc<Shared>,
    scope: Scope,
    topic: &str,
    sub: &str,
    mut cfg: SubscriptionConfig,
    base: Option<&str>,
) -> Result<(bool, Value), (u16, String)> {
    if sub.trim().is_empty() || sub.contains('/') {
        return Err((400, "The subscription name must not be empty or contain '/'.".into()));
    }
    cfg.normalize().map_err(|m| (400, m))?;
    let tgt = {
        let st = shared.state.lock();
        if !st.hosts.contains_key(&scope.host) {
            return Err((404, format!("The topic '{topic}' was not found.")));
        }
        cfg.validate_with(scope.kind, &shared.opts.webhooks).map_err(|m| (400, m))?;
        if let Some(crate::destination::Resolved::Handler { kind, .. }) = cfg.handler_destination() {
            if !shared.destinations.read().contains_key(&kind) {
                return Err((
                    501,
                    format!(
                        "eventgrid: {} destinations need the unified azemu binary (no handler is registered here).",
                        kind.endpoint_type()
                    ),
                ));
            }
        }
        target(&st, scope, &cfg)
    };
    let validation = match (cfg.mode(), &cfg.webhook_url) {
        (DeliveryMode::Push, Some(url)) if !cfg.skip_validation => match base {
            Some(base) => run_handshake(shared, base, &tgt, url).await,
            None => Validation::new("Skipped", "The validation handshake was skipped (no validation URL was given)."),
        },
        (DeliveryMode::Push, Some(_)) => Validation::new("Skipped", "The validation handshake was skipped (skipValidation)."),
        (DeliveryMode::Push, None) => Validation::new("NotRequired", "Delivery to this destination needs no validation."),
        (DeliveryMode::Pull, _) => Validation::new("NotRequired", "Pull delivery needs no validation."),
    };
    let now = Now::now();
    let mut st = shared.state.lock();
    let Some(map) = subs_of(&mut st, &tgt.scope) else {
        return Err((404, format!("The topic '{topic}' was deleted while the subscription was being created.")));
    };
    let created = match map.get_mut(&key(sub)) {
        Some(existing) => {
            existing.cfg = cfg;
            existing.validation = validation;
            false
        }
        None => {
            let mut s = Sub::new(sub, cfg, now.wall);
            s.validation = validation;
            map.insert(key(sub), s);
            true
        }
    };
    let view = map.get(&key(sub)).map(|s| sub_view(s, &now)).unwrap_or(Value::Null);
    drop(st);
    Ok((created, view))
}

/// `POST …/validate`: run the handshake again.
pub async fn revalidate(shared: &Arc<Shared>, req: &Request, topic: &str, sub: &str) -> Response {
    let (tgt, url) = {
        let st = shared.state.lock();
        let scope = match resolve(&st, topic, req) {
            Ok(s) => s,
            Err(r) => return r,
        };
        let Some(s) = subs_ref(&st, &scope).and_then(|m| m.get(&key(sub))) else { return sub_missing(topic, sub) };
        let Some(url) = s.cfg.webhook_url.clone().filter(|_| s.cfg.mode() == DeliveryMode::Push) else {
            return admin_error(400, "BadRequest", "Only push subscriptions have a validation handshake.");
        };
        let cfg = s.cfg.clone();
        (target(&st, scope, &cfg), url)
    };
    let validation = run_handshake(shared, &req.ctx.endpoint.base_url(), &tgt, &url).await;
    let mut st = shared.state.lock();
    let Some(s) = subs_of(&mut st, &tgt.scope).and_then(|m| m.get_mut(&key(sub))) else { return sub_missing(topic, sub) };
    s.validation = validation;
    json_resp(200, &sub_view(s, &Now::now()))
}

/// `GET /validate/{code}`: what a webhook owner calls when the handshake needs a manual step.
pub fn manual_validate(shared: &Arc<Shared>, code: &str) -> Response {
    let mut st = shared.state.lock();
    for h in st.hosts.values_mut() {
        let subs = h.subs.values_mut().chain(h.topics.values_mut().flat_map(|t| t.subs.values_mut()));
        for s in subs {
            if s.validation.status == "AwaitingManualAction" && s.validation.code.as_deref() == Some(code) {
                s.validation = Validation::new("Succeeded", "Validated manually through the validation URL.");
                return Resp::new(200u16).text("Webhook successfully validated as a subscription endpoint.").build();
            }
        }
    }
    admin_error(404, "NotFound", "No subscription is waiting for this validation code.")
}
