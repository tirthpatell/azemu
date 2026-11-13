//! Classic publish (`POST /api/events`) to custom topics, domains and partner topics,
//! and the in-process entry point for events raised by other services.

use std::sync::Arc;

use azemu_core::resp::Resp;
use azemu_core::{Request, Response};
use serde_json::{json, Value};

use crate::events::{Event, Schema};
use crate::model::{InputSchema, TopicKind};
use crate::pull::Now;
use crate::state::{key, Host, PushJob, State, Topic};
use crate::{auth, errors, push, schemas, Shared};

/// Largest request body (recorded from Azure: "The maximum size (1536000) has been exceeded.").
pub const MAX_CLASSIC_BODY: usize = 1_536_000;

/// What `publish_system_event` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemEventOutcome {
    /// The topic existed or was created.
    pub accepted: bool,
    /// Subscriptions whose filter matched the event.
    pub matched_subscriptions: usize,
}

/// `POST /api/events`
pub fn classic(shared: &Arc<Shared>, req: &Request) -> Response {
    if let Err(r) = auth::check(req) {
        return r;
    }
    if req.body.len() > MAX_CLASSIC_BODY {
        return errors::payload_too_large(MAX_CLASSIC_BODY);
    }
    let name = req.account.as_str();
    let existing = {
        let st = shared.state.lock();
        st.host(name).map(|h| (h.kind, h.input_schema))
    };
    let input = match existing {
        Some((TopicKind::Namespace, _)) => {
            return errors::not_found(&format!(
                "'{name}' is an Event Grid namespace. Publish to a namespace topic with POST /topics/{{topic}}:publish."
            ))
        }
        Some((_, input)) => input,
        None if shared.opts.autocreate => InputSchema::Auto,
        None => return errors::not_found(&format!("The topic '{name}' was not found.")),
    };
    let events = match schemas::parse_classic(req, input) {
        Ok(e) => e,
        Err(r) => return r,
    };
    let now = Now::now();
    let mut jobs = Vec::new();
    {
        let mut st = shared.state.lock();
        if st.host(name).is_none() {
            if !shared.opts.autocreate {
                return errors::not_found(&format!("The topic '{name}' was not found."));
            }
            st.hosts.insert(key(name), Host::new(name, TopicKind::Custom, InputSchema::Auto));
        }
        if let Err(r) = route(&mut st, name, &events, &now, &mut jobs) {
            return r;
        }
    }
    finish_publish(shared, jobs);
    Resp::new(200u16).build()
}

/// Send each event to the matching subscriptions of `host`. For a domain the event's
/// `topic` names the domain topic (created on first use).
pub(crate) fn route(st: &mut State, host: &str, events: &[Event], now: &Now, jobs: &mut Vec<PushJob>) -> Result<usize, Response> {
    let Some(h) = st.host_mut(host) else { return Ok(0) };
    let mut matched = 0;
    if h.kind != TopicKind::Domain {
        for ev in events {
            matched += st.fan_out(host, None, ev, now, jobs);
        }
        return Ok(matched);
    }
    let mut targets = Vec::with_capacity(events.len());
    for (i, ev) in events.iter().enumerate() {
        match domain_topic_of(ev) {
            Some(t) => targets.push(t),
            None => {
                let field = if ev.schema == Schema::CloudEvents { "source" } else { "topic" };
                return Err(errors::bad_request(&format!(
                    "The event at index {i} does not name a domain topic. Events published to a domain must set '{field}' to the name of a domain topic."
                )));
            }
        }
    }
    for (ev, topic) in events.iter().zip(targets) {
        if let Some(h) = st.host_mut(host) {
            h.topics.entry(key(&topic)).or_insert_with(|| Topic { name: topic.clone(), ..Topic::default() });
        }
        matched += st.fan_out(host, Some(&topic), ev, now, jobs);
    }
    Ok(matched)
}

/// The domain topic an event is addressed to: the last path segment of `topic` (Event Grid
/// schema, custom) or of `source` (CloudEvents).
fn domain_topic_of(ev: &Event) -> Option<String> {
    let field = if ev.schema == Schema::CloudEvents { "source" } else { "topic" };
    let raw = ev.body.get(field)?.as_str()?.trim();
    let name = match raw.rfind("/topics/") {
        Some(i) => &raw[i + "/topics/".len()..],
        None => raw.trim_end_matches('/').rsplit('/').next().unwrap_or(raw),
    };
    let name = name.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

pub(crate) fn finish_publish(shared: &Arc<Shared>, jobs: Vec<PushJob>) {
    push::spawn_jobs(shared, jobs);
    shared.notify.notify_waiters();
}

/// Entry point for other services (see `EventGridService::publish_system_event`).
pub fn system_event(shared: &Arc<Shared>, topic: &str, event: Value) -> SystemEventOutcome {
    let mut body = match event {
        Value::Object(o) => o,
        _ => return SystemEventOutcome { accepted: false, matched_subscriptions: 0 },
    };
    let now = Now::now();
    body.entry("id").or_insert_with(|| json!(uuid::Uuid::new_v4().to_string()));
    body.entry("eventTime").or_insert_with(|| json!(azemu_core::time::iso8601_7(now.wall)));
    body.entry("dataVersion").or_insert_with(|| json!("1"));
    body.entry("subject").or_insert_with(|| json!(""));
    let ev = Event::new(Schema::EventGrid, Value::Object(body));
    let mut jobs = Vec::new();
    let matched;
    {
        let mut st = shared.state.lock();
        if st.host(topic).is_none() {
            if !shared.opts.autocreate {
                return SystemEventOutcome { accepted: false, matched_subscriptions: 0 };
            }
            st.hosts.insert(key(topic), Host::new(topic, TopicKind::Custom, InputSchema::Auto));
        }
        if matches!(st.host(topic).map(|h| h.kind), Some(TopicKind::Namespace)) {
            return SystemEventOutcome { accepted: false, matched_subscriptions: 0 };
        }
        matched = match route(&mut st, topic, std::slice::from_ref(&ev), &now, &mut jobs) {
            Ok(n) => n,
            Err(_) => return SystemEventOutcome { accepted: false, matched_subscriptions: 0 },
        };
    }
    finish_publish(shared, jobs);
    SystemEventOutcome { accepted: true, matched_subscriptions: matched }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_topic_names() {
        let eg = |t: &str| Event::new(Schema::EventGrid, json!({"topic": t}));
        assert_eq!(domain_topic_of(&eg("orders")).as_deref(), Some("orders"));
        assert_eq!(
            domain_topic_of(&eg("/subscriptions/x/resourceGroups/r/providers/Microsoft.EventGrid/domains/d/topics/orders")).as_deref(),
            Some("orders")
        );
        assert_eq!(domain_topic_of(&eg("/a/b/c")).as_deref(), Some("c"));
        assert_eq!(domain_topic_of(&eg("")), None);
        assert_eq!(domain_topic_of(&Event::new(Schema::EventGrid, json!({}))), None);
        let ce = Event::new(Schema::CloudEvents, json!({"source": "orders"}));
        assert_eq!(domain_topic_of(&ce).as_deref(), Some("orders"));
    }
}
