//! Everything the service keeps: hosts (custom topics, domains, namespaces), their topics
//! and subscriptions. One struct behind one lock; `reset()` replaces it.
//!
//! Names are case-insensitive: the maps are keyed by the lower-case name and the entries
//! remember the spelling they were created with.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use azemu_core::time::{iso8601_7, OffsetDateTime};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Notify;

use crate::events::{topic_resource_id, Event, Schema};
use crate::model::{DeliveryMode, InputSchema, SubscriptionConfig, TopicKind};
use crate::pull::{Now, Queue};

static REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A number nobody has had before in this process. Every change to something that is saved to disk takes one (see
/// `persist.rs`): the highest number handed out is the service's `persist_generation`, and a subscription whose numbers
/// differ from those at the last save has changed since.
pub fn next_revision() -> u64 {
    REVISION.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// The number the last change took.
pub fn current_revision() -> u64 {
    REVISION.load(std::sync::atomic::Ordering::SeqCst)
}

/// Number of delivery attempts kept per subscription.
pub const DELIVERY_LOG_CAP: usize = 200;

/// Result of the webhook validation handshake.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Validation {
    /// `NotRequired` (pull), `Skipped`, `Succeeded`, `Failed`.
    pub status: String,
    pub detail: String,
    /// The validation code of a handshake that waits for a manual GET of the validation URL.
    #[serde(skip)]
    pub code: Option<String>,
}

impl Validation {
    pub fn new(status: &str, detail: &str) -> Validation {
        Validation { status: status.into(), detail: detail.into(), code: None }
    }

    /// May events be delivered?
    pub fn allows_delivery(&self) -> bool {
        matches!(self.status.as_str(), "NotRequired" | "Skipped" | "Succeeded")
    }
}

/// One attempt to deliver an event to a webhook.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryAttempt {
    pub time: String,
    pub event_id: String,
    pub attempt: u32,
    /// `Success`, `Failure`, `DeadLettered`, `Dropped`.
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    pub detail: String,
    pub duration_ms: u64,
}

/// How many webhook (and queue, Service Bus, event hub) deliveries that are waiting for a retry one subscription and
/// the whole service keep. Each is an event body in memory until it is delivered or dead-lettered, which for a target
/// that is down takes up to 24 hours. When a limit is passed the oldest pending deliveries are dead-lettered
/// (reason `PendingQueueFull`) and stop retrying.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PushLimits {
    pub per_subscription: usize,
    pub per_subscription_bytes: u64,
    pub total: usize,
    pub total_bytes: u64,
}

impl Default for PushLimits {
    fn default() -> Self {
        PushLimits { per_subscription: 1_000, per_subscription_bytes: 32 * 1024 * 1024, total: 5_000, total_bytes: 128 * 1024 * 1024 }
    }
}

/// A delivery that has not finished: kept so the oldest can be dead-lettered when the limits are passed.
pub struct Pending {
    pub bytes: u64,
    /// The event as it was sent (shared with the delivery task, not copied).
    pub body: Arc<Value>,
    pub event_id: String,
    /// Wakes the delivery task so it stops at once and frees its copy of the event.
    pub cancel: Arc<Notify>,
    /// What it takes to start the delivery again after a restart (see `persist.rs`).
    pub job: PushJob,
    /// Attempts made so far, and when the first was made (milliseconds since 1970).
    pub attempts: u32,
    pub started_ms: i64,
}

pub struct Sub {
    pub name: String,
    pub cfg: SubscriptionConfig,
    pub created: OffsetDateTime,
    pub validation: Validation,
    pub queue: Queue,
    pub log: VecDeque<DeliveryAttempt>,
    pub delivered: u64,
    pub failed_attempts: u64,
    /// Push deliveries in progress or waiting for a retry, by id (ids grow, so the first is the oldest).
    pub pending: BTreeMap<u64, Pending>,
    pub pending_bytes: u64,
    /// Deliveries dead-lettered because the limits of `PushLimits` were passed.
    pub shed: u64,
    /// Changes whenever anything saved about this subscription besides its queue changes (see [`next_revision`]).
    pub rev: u64,
}

impl Sub {
    pub fn new(name: &str, cfg: SubscriptionConfig, created: OffsetDateTime) -> Sub {
        let validation = match cfg.mode() {
            DeliveryMode::Pull => Validation::new("NotRequired", "Pull delivery needs no validation."),
            DeliveryMode::Push => Validation::new("Skipped", "Not validated."),
        };
        Sub {
            name: name.to_string(),
            cfg,
            created,
            validation,
            queue: Queue::default(),
            log: VecDeque::new(),
            delivered: 0,
            failed_attempts: 0,
            pending: BTreeMap::new(),
            pending_bytes: 0,
            shed: 0,
            rev: next_revision(),
        }
    }

    /// A delivery ended (delivered or dead-lettered by its task): its size. `None` when it was no longer pending.
    pub fn finish_pending(&mut self, id: u64) -> Option<u64> {
        let p = self.pending.remove(&id)?;
        self.pending_bytes = self.pending_bytes.saturating_sub(p.bytes);
        self.rev = next_revision();
        Some(p.bytes)
    }

    /// Dead-letter the oldest pending delivery and wake its task so that it ends.
    fn shed_oldest(&mut self, now: &Now) {
        let Some((_, p)) = self.pending.pop_first() else { return };
        self.rev = next_revision();
        self.pending_bytes = self.pending_bytes.saturating_sub(p.bytes);
        self.shed += 1;
        p.cancel.notify_one();
        let detail = "The retry queue of this subscription is full (azemu keeps a limited number of events waiting for a retry); the oldest was dead-lettered.";
        self.queue.dead_letter_sized((*p.body).clone(), p.bytes, "PendingQueueFull", detail, 0, now);
        self.log_attempt(DeliveryAttempt {
            time: now_iso(now),
            event_id: p.event_id,
            attempt: 0,
            outcome: "DeadLettered".into(),
            status_code: None,
            detail: format!("{detail} PendingQueueFull"),
            duration_ms: 0,
        });
    }

    pub fn log_attempt(&mut self, a: DeliveryAttempt) {
        self.rev = next_revision();
        if self.log.len() >= DELIVERY_LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(a);
    }
}

#[derive(Default)]
pub struct Topic {
    pub name: String,
    pub input_schema: InputSchema,
    pub subs: BTreeMap<String, Sub>,
    pub published: u64,
}

pub struct Host {
    pub name: String,
    pub kind: TopicKind,
    pub input_schema: InputSchema,
    /// Subscriptions of a custom topic, or domain-wide subscriptions of a domain.
    pub subs: BTreeMap<String, Sub>,
    /// Domain topics (domain) or namespace topics (namespace).
    pub topics: BTreeMap<String, Topic>,
    pub published: u64,
    /// Event sources only: the resource id events carry as `topic` / `source`.
    pub resource_id: Option<String>,
}

impl Host {
    pub fn new(name: &str, kind: TopicKind, input_schema: InputSchema) -> Host {
        Host { name: name.to_string(), kind, input_schema, subs: BTreeMap::new(), topics: BTreeMap::new(), published: 0, resource_id: None }
    }
}

/// Where a subscription lives: a host, optionally one of its topics.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SubKey {
    pub host: String,
    pub topic: Option<String>,
    pub sub: String,
}

/// An event that must be POSTed to a webhook. Built under the lock, sent after it.
#[derive(Clone, Debug)]
pub struct PushJob {
    pub key: SubKey,
    /// Key of this delivery in `Sub::pending`.
    pub id: u64,
    pub cancel: Arc<Notify>,
    pub sub_name: String,
    pub url: String,
    pub schema: Schema,
    pub body: Arc<Value>,
    pub event_id: String,
    pub data_version: String,
    pub max_attempts: u32,
    pub ttl: std::time::Duration,
    pub intervals_ms: Option<Vec<u64>>,
    /// A Storage queue, Service Bus or Event Hubs destination instead of a webhook.
    pub dest: Option<Dest>,
}

/// Where a job goes when it is not a webhook.
#[derive(Clone, Debug)]
pub struct Dest {
    pub kind: crate::destination::DestinationKind,
    pub target: crate::destination::DestinationTarget,
    pub ttl_seconds: Option<i64>,
}

#[derive(Default)]
pub struct State {
    pub hosts: BTreeMap<String, Host>,
    pub limits: PushLimits,
    pub(crate) next_push_id: u64,
}

pub fn key(name: &str) -> String {
    name.to_ascii_lowercase()
}

impl State {
    pub fn new(limits: PushLimits) -> State {
        State { limits, ..State::default() }
    }

    pub fn host(&self, name: &str) -> Option<&Host> {
        self.hosts.get(&key(name))
    }

    pub fn host_mut(&mut self, name: &str) -> Option<&mut Host> {
        self.hosts.get_mut(&key(name))
    }

    pub fn sub_mut(&mut self, k: &SubKey) -> Option<&mut Sub> {
        let host = self.hosts.get_mut(&k.host)?;
        match &k.topic {
            Some(t) => host.topics.get_mut(t)?.subs.get_mut(&k.sub),
            None => host.subs.get_mut(&k.sub),
        }
    }

    pub fn sub(&self, k: &SubKey) -> Option<&Sub> {
        let host = self.hosts.get(&k.host)?;
        match &k.topic {
            Some(t) => host.topics.get(t)?.subs.get(&k.sub),
            None => host.subs.get(&k.sub),
        }
    }

    /// Route one event published to `host` (and `topic`, for domains and namespaces) to
    /// every subscription whose filter matches. Pull subscriptions get the event queued;
    /// push subscriptions are returned as jobs to be sent after the lock is released.
    /// Returns the number of subscriptions that matched.
    pub fn fan_out(&mut self, host: &str, topic: Option<&str>, ev: &Event, now: &Now, jobs: &mut Vec<PushJob>) -> usize {
        let hk = key(host);
        next_revision();
        let State { hosts, limits, next_push_id } = self;
        let Some(h) = hosts.get_mut(&hk) else { return 0 };
        h.published += 1;
        let is_domain = h.kind == TopicKind::Domain;
        let topic_id = match &h.resource_id {
            Some(id) => id.clone(),
            None => topic_resource_id(&h.name, topic, is_domain),
        };
        let mut matched = 0;
        let tk = topic.map(key);
        let mut ctx = PushCtx { limits: *limits, next_id: next_push_id };
        // domain-wide / custom-topic subscriptions first, then the topic's own
        for (sk, sub) in h.subs.iter_mut() {
            let k = SubKey { host: hk.clone(), topic: None, sub: sk.clone() };
            matched += deliver_to(sub, k, ev, &topic_id, now, jobs, &mut ctx) as usize;
        }
        if let Some(tk) = tk {
            if let Some(t) = h.topics.get_mut(&tk) {
                t.published += 1;
                for (sk, sub) in t.subs.iter_mut() {
                    let k = SubKey { host: hk.clone(), topic: Some(tk.clone()), sub: sk.clone() };
                    matched += deliver_to(sub, k, ev, &topic_id, now, jobs, &mut ctx) as usize;
                }
            }
        }
        self.enforce_total_limit(now);
        matched
    }

    /// Every subscription, whatever host or topic it is under.
    fn all_subs_mut(&mut self) -> impl Iterator<Item = &mut Sub> {
        self.hosts.values_mut().flat_map(|h| h.subs.values_mut().chain(h.topics.values_mut().flat_map(|t| t.subs.values_mut())))
    }

    /// While the pending deliveries of all subscriptions together are over the limits, dead-letter the oldest of all.
    fn enforce_total_limit(&mut self, now: &Now) {
        let limits = self.limits;
        loop {
            let (count, bytes) = self.all_subs_mut().fold((0usize, 0u64), |(c, b), s| (c + s.pending.len(), b + s.pending_bytes));
            if count <= limits.total && bytes <= limits.total_bytes {
                return;
            }
            // the subscription whose first (oldest) pending delivery has the lowest id
            let oldest = self.all_subs_mut().filter_map(|s| s.pending.keys().next().copied().map(|id| (id, s))).min_by_key(|(id, _)| *id);
            match oldest {
                Some((_, sub)) => sub.shed_oldest(now),
                None => return,
            }
        }
    }

    /// Admin view of the event sources of other services.
    pub fn source_summaries(&self) -> Vec<Value> {
        self.hosts
            .values()
            .filter(|h| h.kind == TopicKind::Source)
            .map(|h| {
                serde_json::json!({
                    "key": h.name, "resourceId": h.resource_id,
                    "subscriptions": h.subs.values().map(|s| s.name.clone()).collect::<Vec<_>>(),
                    "published": h.published,
                })
            })
            .collect()
    }

    /// Admin view of every topic.
    pub fn topic_summaries(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for h in self.hosts.values() {
            match h.kind {
                TopicKind::Custom => out.push(serde_json::json!({
                    "name": h.name, "kind": "custom", "inputSchema": h.input_schema,
                    "subscriptions": h.subs.values().map(|s| s.name.clone()).collect::<Vec<_>>(),
                    "published": h.published,
                })),
                TopicKind::Domain => {
                    out.push(serde_json::json!({
                        "name": h.name, "kind": "domain", "inputSchema": h.input_schema,
                        "subscriptions": h.subs.values().map(|s| s.name.clone()).collect::<Vec<_>>(),
                        "published": h.published,
                    }));
                    for t in h.topics.values() {
                        out.push(serde_json::json!({
                            "name": t.name, "kind": "domainTopic", "domain": h.name,
                            "subscriptions": t.subs.values().map(|s| s.name.clone()).collect::<Vec<_>>(),
                            "published": t.published,
                        }));
                    }
                }
                TopicKind::Source => out.push(serde_json::json!({
                    "name": h.name, "kind": "source", "resourceId": h.resource_id,
                    "subscriptions": h.subs.values().map(|s| s.name.clone()).collect::<Vec<_>>(),
                    "published": h.published,
                })),
                TopicKind::Namespace => {
                    for t in h.topics.values() {
                        out.push(serde_json::json!({
                            "name": t.name, "kind": "namespaceTopic", "namespace": h.name,
                            "subscriptions": t.subs.values().map(|s| s.name.clone()).collect::<Vec<_>>(),
                            "published": t.published,
                        }));
                    }
                }
            }
        }
        out
    }
}

/// The id counter and the limits, for the deliveries made while one event is routed.
struct PushCtx<'a> {
    limits: PushLimits,
    next_id: &'a mut u64,
}

fn deliver_to(sub: &mut Sub, key: SubKey, ev: &Event, topic_id: &str, now: &Now, jobs: &mut Vec<PushJob>, ctx: &mut PushCtx<'_>) -> bool {
    if !sub.cfg.filter.matches(ev) {
        return false;
    }
    match sub.cfg.mode() {
        DeliveryMode::Pull => {
            sub.queue.enqueue(ev.deliver_as(Schema::CloudEvents, topic_id), now);
        }
        DeliveryMode::Push => {
            if !sub.validation.allows_delivery() {
                return true;
            }
            let dest = match sub.cfg.handler_destination() {
                Some(crate::destination::Resolved::Handler { kind, target, ttl_seconds }) => Some(Dest { kind, target, ttl_seconds }),
                _ => None,
            };
            let url = match (sub.cfg.webhook_url.clone(), &dest) {
                (Some(u), _) => u,
                (None, Some(_)) => String::new(),
                (None, None) => return true,
            };
            let schema = sub.cfg.delivery_schema.unwrap_or(ev.schema);
            let body = ev.deliver_as(schema, topic_id);
            let event_id = ev.id().unwrap_or("").to_string();
            let data_version = body.get("dataVersion").and_then(Value::as_str).unwrap_or("1").to_string();
            let rp = &sub.cfg.retry_policy;
            let (max_attempts, ttl, intervals_ms) = (
                rp.max_delivery_attempts.unwrap_or(crate::model::DEFAULT_MAX_ATTEMPTS).max(1),
                crate::model::secs(rp.event_time_to_live_in_minutes.unwrap_or(crate::model::DEFAULT_TTL_MINUTES) * 60.0),
                rp.retry_intervals_ms.clone(),
            );
            sub.rev = next_revision();
            let id = *ctx.next_id;
            *ctx.next_id += 1;
            let bytes = body.to_string().len() as u64;
            let body = Arc::new(body);
            let cancel = Arc::new(Notify::new());
            let job = PushJob {
                key: key.clone(),
                id,
                cancel: cancel.clone(),
                sub_name: sub.name.clone(),
                url,
                schema,
                body: body.clone(),
                event_id: event_id.clone(),
                data_version,
                max_attempts,
                ttl,
                intervals_ms,
                dest,
            };
            let started_ms = (now.wall.unix_timestamp_nanos() / 1_000_000) as i64;
            sub.pending.insert(id, Pending { bytes, body, event_id, cancel, job: job.clone(), attempts: 0, started_ms });
            sub.pending_bytes += bytes;
            // over the limit of this subscription: the oldest deliveries are dead-lettered (the new one stays)
            while sub.pending.len() > 1
                && (sub.pending.len() > ctx.limits.per_subscription || sub.pending_bytes > ctx.limits.per_subscription_bytes)
            {
                sub.shed_oldest(now);
            }
            jobs.push(job);
        }
    }
    true
}

pub fn now_iso(now: &Now) -> String {
    iso8601_7(now.wall)
}
