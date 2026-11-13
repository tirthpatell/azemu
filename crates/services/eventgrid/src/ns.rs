//! Namespace topics: publish, and pull delivery (receive, acknowledge, release, reject,
//! renew lock). The wire format is the `EventGridNamespace` data-plane API 2024-06-01.

use std::sync::Arc;
use std::time::{Duration, Instant};

use azemu_core::resp::Resp;
use azemu_core::{Request, Response};
use serde_json::{json, Value};

use crate::model::{DeliveryMode, InputSchema, PullParams, SubscriptionConfig, TopicKind};
use crate::pull::{Now, TokenResults};
use crate::state::{key, Host, State, Sub, SubKey, Topic};
use crate::{auth, errors, publish, schemas, Shared};

/// Largest publish request.
pub const MAX_NAMESPACE_BODY: usize = 1_048_576;
pub const DEFAULT_MAX_WAIT_SECS: f64 = 60.0;
pub const MAX_WAIT_SECS: f64 = 120.0;
pub const MAX_LOCK_TOKENS: usize = 100;

fn json_ok(v: &Value) -> Response {
    Resp::new(200u16).json(v).build()
}

/// The namespace `host` and topic `topic`: create them when allowed, else 404.
fn ensure_topic(st: &mut State, shared: &Shared, host: &str, topic: &str) -> Result<(), Response> {
    match st.host(host).map(|h| h.kind) {
        Some(TopicKind::Namespace) => {}
        Some(_) => {
            return Err(errors::not_found(&format!(
                "'{host}' is not an Event Grid namespace. Namespace topics are only available on namespace hosts."
            )))
        }
        None if shared.opts.autocreate => {
            st.hosts.insert(key(host), Host::new(host, TopicKind::Namespace, InputSchema::CloudEvents));
        }
        None => return Err(errors::not_found(&format!("The namespace '{host}' was not found."))),
    }
    let autocreate = shared.opts.autocreate;
    let Some(h) = st.host_mut(host) else { return Ok(()) };
    if let std::collections::btree_map::Entry::Vacant(v) = h.topics.entry(key(topic)) {
        if !autocreate {
            return Err(errors::not_found(&format!("The topic '{topic}' was not found in the namespace '{host}'.")));
        }
        v.insert(Topic { name: topic.to_string(), input_schema: InputSchema::CloudEvents, ..Topic::default() });
    }
    Ok(())
}

/// `POST /topics/{topic}:publish`
pub fn publish(shared: &Arc<Shared>, req: &Request, topic: &str) -> Response {
    if let Err(r) = auth::check(req) {
        return r;
    }
    if req.body.len() > MAX_NAMESPACE_BODY {
        return errors::payload_too_large(MAX_NAMESPACE_BODY);
    }
    let events = match schemas::parse_namespace(req) {
        Ok(e) => e,
        Err(r) => return r,
    };
    let now = Now::now();
    let mut jobs = Vec::new();
    {
        let mut st = shared.state.lock();
        if let Err(r) = ensure_topic(&mut st, shared, &req.account, topic) {
            return r;
        }
        for ev in &events {
            st.fan_out(&req.account, Some(topic), ev, &now, &mut jobs);
        }
    }
    publish::finish_publish(shared, jobs);
    json_ok(&json!({}))
}

/// Find the subscription; create it on receive when auto-creation is on.
fn find_sub(
    st: &mut State,
    shared: &Shared,
    host: &str,
    topic: &str,
    sub: &str,
    create: bool,
    now: &Now,
) -> Result<(SubKey, PullParams), Response> {
    let exists = st.host(host).map(|h| h.kind == TopicKind::Namespace && h.topics.contains_key(&key(topic)));
    if exists != Some(true) {
        if create {
            ensure_topic(st, shared, host, topic)?;
        } else if st.host(host).map(|h| h.kind != TopicKind::Namespace).unwrap_or(false) {
            return Err(errors::not_found(&format!("'{host}' is not an Event Grid namespace.")));
        } else {
            return Err(errors::not_found(&format!("The topic '{topic}' was not found in the namespace '{host}'.")));
        }
    }
    let k = SubKey { host: key(host), topic: Some(key(topic)), sub: key(sub) };
    if st.sub(&k).is_none() {
        if !(create && shared.opts.autocreate) {
            return Err(errors::not_found(&format!("The event subscription '{sub}' was not found for the topic '{topic}'.")));
        }
        if let Some(t) = st.host_mut(host).and_then(|h| h.topics.get_mut(&key(topic))) {
            t.subs.insert(
                key(sub),
                Sub::new(sub, SubscriptionConfig { delivery_mode: Some(DeliveryMode::Pull), ..Default::default() }, now.wall),
            );
        }
    }
    match st.sub(&k) {
        Some(s) if s.cfg.mode() == DeliveryMode::Push => Err(errors::bad_request(&format!(
            "The event subscription '{sub}' uses push delivery; receive and acknowledge are only available for pull delivery."
        ))),
        Some(s) => Ok((k, s.cfg.pull_params())),
        None => Err(errors::not_found("The event subscription was not found.")),
    }
}

fn number_param(req: &Request, name: &str) -> Result<Option<f64>, Response> {
    match req.query.get(name) {
        None => Ok(None),
        Some(v) => v
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .map(Some)
            .ok_or_else(|| errors::bad_request(&format!("The query parameter '{name}' must be a number, got '{v}'."))),
    }
}

/// `POST /topics/{topic}/eventsubscriptions/{sub}:receive`
pub async fn receive(shared: &Arc<Shared>, req: &Request, topic: &str, sub: &str) -> Response {
    if let Err(r) = auth::check(req) {
        return r;
    }
    let max_events = match number_param(req, "maxEvents") {
        Err(r) => return r,
        Ok(None) => 1,
        Ok(Some(n)) if (1.0..=100.0).contains(&n) && n.fract() == 0.0 => n as usize,
        Ok(Some(_)) => return errors::bad_request("maxEvents must be an integer from 1 to 100."),
    };
    let wait = match number_param(req, "maxWaitTime") {
        Err(r) => return r,
        Ok(None) => DEFAULT_MAX_WAIT_SECS,
        Ok(Some(n)) if (0.0..=MAX_WAIT_SECS).contains(&n) => n,
        Ok(Some(_)) => return errors::bad_request("maxWaitTime must be from 10 to 120 seconds."),
    };
    let deadline = Instant::now() + Duration::from_secs_f64(wait);
    loop {
        // Register for wake-ups before looking at the queue, so a publish between the look
        // and the wait is not missed.
        let notified = shared.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let wake_at;
        {
            let now = Now::now();
            let mut st = shared.state.lock();
            let (k, params) = match find_sub(&mut st, shared, &req.account, topic, sub, true, &now) {
                Ok(x) => x,
                Err(r) => return r,
            };
            let Some(s) = st.sub_mut(&k) else { return errors::not_found("The event subscription was not found.") };
            let got = s.queue.receive(max_events, &now, &params);
            if !got.is_empty() {
                let value: Vec<Value> = got
                    .into_iter()
                    .map(|r| {
                        json!({
                            "brokerProperties": { "lockToken": r.token, "deliveryCount": r.delivery_count },
                            "event": r.event,
                        })
                    })
                    .collect();
                return json_ok(&json!({ "value": value }));
            }
            wake_at = s.queue.next_wake(&now, &params);
        }
        let now = Instant::now();
        if now >= deadline {
            return json_ok(&json!({ "value": [] }));
        }
        let until = wake_at.map_or(deadline, |w| w.min(deadline));
        tokio::select! {
            _ = &mut notified => {}
            _ = tokio::time::sleep_until(until.into()) => {}
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TokenOp {
    Acknowledge,
    Release,
    Reject,
    Renew,
}

fn lock_tokens(req: &Request) -> Result<Vec<String>, Response> {
    let v: Value =
        serde_json::from_slice(&req.body).map_err(|e| errors::bad_request(&format!("The request body is not valid JSON: {e}.")))?;
    let list = v
        .get("lockTokens")
        .and_then(Value::as_array)
        .ok_or_else(|| errors::bad_request("The request body must have a 'lockTokens' array."))?;
    let tokens: Vec<String> = list.iter().filter_map(|t| t.as_str().map(str::to_string)).collect();
    if tokens.len() != list.len() {
        return Err(errors::bad_request("'lockTokens' must contain only strings."));
    }
    if tokens.is_empty() {
        return Err(errors::bad_request("'lockTokens' must contain at least one lock token."));
    }
    if tokens.len() > MAX_LOCK_TOKENS {
        return Err(errors::bad_request(&format!("'lockTokens' can contain at most {MAX_LOCK_TOKENS} lock tokens.")));
    }
    Ok(tokens)
}

fn results_json(res: TokenResults) -> Response {
    let failed: Vec<Value> =
        res.failed.into_iter().map(|(t, code, msg)| json!({ "lockToken": t, "error": { "code": code, "message": msg } })).collect();
    json_ok(&json!({ "failedLockTokens": failed, "succeededLockTokens": res.succeeded }))
}

/// `:acknowledge`, `:release`, `:reject` and `:renewLock`.
fn token_op(shared: &Arc<Shared>, req: &Request, topic: &str, sub: &str, op: TokenOp) -> Response {
    if let Err(r) = auth::check(req) {
        return r;
    }
    let delay = if op == TokenOp::Release {
        match number_param(req, "releaseDelayInSeconds") {
            Err(r) => return r,
            Ok(None) => Duration::ZERO,
            Ok(Some(n)) if (0.0..=86_400.0).contains(&n) => Duration::from_secs_f64(n),
            Ok(Some(_)) => return errors::bad_request("releaseDelayInSeconds must be 0, 10, 60, 600 or 3600."),
        }
    } else {
        Duration::ZERO
    };
    let tokens = match lock_tokens(req) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let now = Now::now();
    let res = {
        let mut st = shared.state.lock();
        let (k, params) = match find_sub(&mut st, shared, &req.account, topic, sub, false, &now) {
            Ok(x) => x,
            Err(r) => return r,
        };
        let Some(s) = st.sub_mut(&k) else { return errors::not_found("The event subscription was not found.") };
        match op {
            TokenOp::Acknowledge => s.queue.acknowledge(&tokens, &now, &params),
            TokenOp::Release => s.queue.release(&tokens, delay, &now, &params),
            TokenOp::Reject => s.queue.reject(&tokens, &now, &params),
            TokenOp::Renew => s.queue.renew(&tokens, &now, &params),
        }
    };
    if op == TokenOp::Release {
        shared.notify.notify_waiters();
    }
    results_json(res)
}

pub fn acknowledge(shared: &Arc<Shared>, req: &Request, topic: &str, sub: &str) -> Response {
    token_op(shared, req, topic, sub, TokenOp::Acknowledge)
}

pub fn release(shared: &Arc<Shared>, req: &Request, topic: &str, sub: &str) -> Response {
    token_op(shared, req, topic, sub, TokenOp::Release)
}

pub fn reject(shared: &Arc<Shared>, req: &Request, topic: &str, sub: &str) -> Response {
    token_op(shared, req, topic, sub, TokenOp::Reject)
}

pub fn renew_lock(shared: &Arc<Shared>, req: &Request, topic: &str, sub: &str) -> Response {
    token_op(shared, req, topic, sub, TokenOp::Renew)
}
