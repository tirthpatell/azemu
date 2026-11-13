//! Namespaces and entities: where queues, topics and subscriptions live, address resolution,
//! topic fan-out, auto-forwarding and the periodic tick.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;

use crate::core::*;
use crate::errors::SbError;
use crate::model::*;
use crate::rules::Rule;
use crate::store::*;

pub const MAX_FORWARD_DEPTH: usize = 4;
/// Most messages one batch (one AMQP transfer in the batch format, or one `schedule-message` request) may hold.
/// Microsoft Learn's Service Bus quotas page limits a batch by size only (1 MB), so this number is ours: it is
/// what Event Hubs allows, and it keeps a batch of empty messages (a few bytes each on the wire) from being
/// decoded into millions of messages.
pub const MAX_MESSAGES_PER_BATCH: usize = 50_000;
pub const DEFAULT_NAMESPACE: &str = "default";

pub fn now_ms() -> i64 {
    (azemu_core::time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

pub struct Queue {
    pub name: String,
    pub core: Core,
}

pub struct Subscription {
    pub name: String,
    pub core: Core,
    pub rules: Vec<Rule>,
}

pub struct Topic {
    pub name: String,
    pub settings: Settings,
    pub subs: BTreeMap<String, Subscription>,
    /// Scheduled messages waiting to be fanned out (one entry per message sent to the topic).
    pub scheduled: Store,
    /// Message ids seen inside the duplicate detection window (topics with `RequiresDuplicateDetection`).
    pub dup: std::collections::HashMap<String, i64>,
    /// While a transaction commits: duplicate ids added or replaced (with their old expiry).
    dup_journal: Option<Vec<(String, Option<i64>)>>,
    /// Changes whenever `dup` does (see `persist.rs`).
    pub dup_rev: u64,
}

/// Azure's quota: queues and topics together in one namespace (Standard).
pub const MAX_ENTITIES_PER_NAMESPACE: usize = 10_000;
/// Azure's quota: subscriptions on one topic.
pub const MAX_SUBSCRIPTIONS_PER_TOPIC: usize = 2_000;

#[derive(Default)]
pub struct Namespace {
    pub queues: BTreeMap<String, Queue>,
    pub topics: BTreeMap<String, Topic>,
}

#[derive(Default)]
pub struct State {
    pub namespaces: BTreeMap<String, Namespace>,
    /// When a config file names exactly one namespace, `default` (what `sb://localhost`
    /// resolves to) is an alias of it.
    pub default_alias: Option<String>,
}

pub fn key(name: &str) -> String {
    name.to_lowercase()
}

impl Topic {
    pub fn new(name: &str, settings: Settings) -> Topic {
        Topic {
            name: name.to_string(),
            settings,
            subs: BTreeMap::new(),
            scheduled: Store::new(),
            dup: Default::default(),
            dup_journal: None,
            dup_rev: next_revision(),
        }
    }

    /// Copy `inc` into every subscription whose rules accept it. Returns forwards to deliver.
    pub fn fan_out(&mut self, inc: &Incoming, now: i64) -> Result<Vec<Forward>, SbError> {
        let mut forwards = Vec::new();
        let mut first_err = None;
        for sub in self.subs.values_mut() {
            let copies = crate::rules::evaluate(&sub.rules, inc, sub.core.settings.dead_letter_on_filter_exception);
            for message in copies {
                let mut copy = inc.clone();
                copy.message = message;
                if let Some(target) = sub.core.settings.forward_to.clone() {
                    if copy.enq_seq.is_none() {
                        copy.enq_seq = Some(sub.core.main.alloc_seq());
                    }
                    forwards.push(Forward { target, incoming: copy });
                    continue;
                }
                match sub.core.enqueue(copy, now) {
                    Ok(_) => {}
                    Err(e) => first_err = first_err.or(Some(e)),
                }
            }
        }
        match first_err {
            Some(e) if forwards.is_empty() => Err(e),
            _ => Ok(forwards),
        }
    }

    /// Check a message can be sent: topic limits and every subscription that would take it.
    pub fn check_send(&self, size: usize) -> Result<(), SbError> {
        match self.settings.status.as_str() {
            "Disabled" | "SendDisabled" => {
                return Err(SbError::new(
                    crate::errors::ENTITY_DISABLED,
                    400,
                    format!("The messaging entity '{}' is disabled for sending.", self.name),
                ));
            }
            _ => {}
        }
        if size as u64 > self.settings.max_message_size_bytes() {
            return Err(SbError::new(
                azemu_amqp::condition::LINK_MESSAGE_SIZE_EXCEEDED,
                413,
                format!(
                    "The received message (delivery-id:0, size:{size} bytes) exceeds the limit ({} bytes) currently allowed on the link.",
                    self.settings.max_message_size_bytes()
                ),
            ));
        }
        Ok(())
    }

    pub fn size_bytes(&self) -> u64 {
        self.subs.values().map(|s| s.core.size_bytes()).sum()
    }

    /// Duplicate detection of the topic itself: true when the id was seen inside the window,
    /// otherwise the id is recorded.
    pub(crate) fn seen_before(&mut self, id: String, now: i64) -> bool {
        self.dup.retain(|_, exp| *exp > now);
        if self.dup.contains_key(&id) {
            return true;
        }
        if let Some(j) = &mut self.dup_journal {
            j.push((id.clone(), None));
        }
        self.dup.insert(id, now + self.settings.duplicate_window_ms);
        self.dup_rev = next_revision();
        false
    }

    /// See [`Core::begin_journal`]: the scheduled store, the duplicate ids and every subscription.
    pub fn begin_journal(&mut self) {
        self.scheduled.begin_journal();
        self.dup_journal = Some(Vec::new());
        for s in self.subs.values_mut() {
            s.core.begin_journal();
        }
    }

    pub fn end_journal(&mut self, undo: bool) {
        self.scheduled.end_journal(undo);
        if let Some(added) = self.dup_journal.take() {
            if undo {
                for (id, _) in added {
                    self.dup.remove(&id);
                }
                self.dup_rev = next_revision();
            }
        }
        for s in self.subs.values_mut() {
            s.core.end_journal(undo);
        }
    }
}

impl Namespace {
    /// Wake every receiver that waits on a queue or subscription of this namespace.
    pub fn wake_receivers(&mut self) {
        for q in self.queues.values_mut() {
            q.core.wake_all();
        }
        for t in self.topics.values_mut() {
            for sub in t.subs.values_mut() {
                sub.core.wake_all();
            }
        }
    }

    pub fn queue(&self, name: &str) -> Option<&Queue> {
        self.queues.get(&key(name))
    }
    pub fn queue_mut(&mut self, name: &str) -> Option<&mut Queue> {
        self.queues.get_mut(&key(name))
    }
    pub fn topic(&self, name: &str) -> Option<&Topic> {
        self.topics.get(&key(name))
    }
    pub fn topic_mut(&mut self, name: &str) -> Option<&mut Topic> {
        self.topics.get_mut(&key(name))
    }

    /// The engine behind a queue or subscription address.
    pub fn core_mut(&mut self, addr: &Address) -> Option<&mut Core> {
        match addr {
            Address::Queue { name, .. } => self.queue_mut(name).map(|q| &mut q.core),
            Address::Subscription { topic, name, .. } => self.topic_mut(topic)?.subs.get_mut(&key(name)).map(|s| &mut s.core),
            _ => None,
        }
    }
    pub fn core(&self, addr: &Address) -> Option<&Core> {
        match addr {
            Address::Queue { name, .. } => self.queue(name).map(|q| &q.core),
            Address::Subscription { topic, name, .. } => self.topic(topic)?.subs.get(&key(name)).map(|s| &s.core),
            _ => None,
        }
    }

    /// Does an entity (queue, or topic) with this path exist?
    pub fn exists(&self, addr: &Address) -> bool {
        match addr {
            Address::Queue { name, .. } => self.queue(name).is_some(),
            Address::Topic { name } => self.topic(name).is_some(),
            Address::Subscription { topic, name, .. } => self.topic(topic).map(|t| t.subs.contains_key(&key(name))).unwrap_or(false),
            _ => false,
        }
    }

    /// Start remembering every change in every entity of the namespace, for a transaction commit
    /// (forwarding can reach any entity). Cheap: it only sets a flag on each store.
    pub fn begin_journal(&mut self) {
        for q in self.queues.values_mut() {
            q.core.begin_journal();
        }
        for t in self.topics.values_mut() {
            t.begin_journal();
        }
    }

    /// Stop remembering; with `undo`, everything done since [`Namespace::begin_journal`] is taken back.
    pub fn end_journal(&mut self, undo: bool) {
        for q in self.queues.values_mut() {
            q.core.end_journal(undo);
        }
        for t in self.topics.values_mut() {
            t.end_journal(undo);
        }
    }

    fn check_entity_quota(&self) -> Result<(), SbError> {
        if self.queues.len() + self.topics.len() >= MAX_ENTITIES_PER_NAMESPACE {
            return Err(SbError::new(
                azemu_amqp::condition::RESOURCE_LIMIT_EXCEEDED,
                403,
                format!("The maximum number of queues and topics in a namespace ({MAX_ENTITIES_PER_NAMESPACE}) has been reached."),
            ));
        }
        Ok(())
    }

    pub fn create_queue(&mut self, name: &str, settings: Settings) -> Result<(), SbError> {
        if self.queues.contains_key(&key(name)) || self.topics.contains_key(&key(name)) {
            return Err(SbError::new(crate::errors::ENTITY_ALREADY_EXISTS, 409, format!("The messaging entity '{name}' already exists.")));
        }
        self.check_entity_quota()?;
        self.queues.insert(key(name), Queue { name: name.to_string(), core: Core::new(name, settings) });
        next_revision();
        Ok(())
    }

    pub fn create_topic(&mut self, name: &str, settings: Settings) -> Result<(), SbError> {
        if self.queues.contains_key(&key(name)) || self.topics.contains_key(&key(name)) {
            return Err(SbError::new(crate::errors::ENTITY_ALREADY_EXISTS, 409, format!("The messaging entity '{name}' already exists.")));
        }
        self.check_entity_quota()?;
        self.topics.insert(key(name), Topic::new(name, settings));
        next_revision();
        Ok(())
    }

    pub fn create_subscription(&mut self, topic: &str, name: &str, settings: Settings, rules: Vec<Rule>) -> Result<(), SbError> {
        let t = self.topic_mut(topic).ok_or_else(|| SbError::entity_not_found(topic, ""))?;
        if t.subs.contains_key(&key(name)) {
            return Err(SbError::new(
                crate::errors::ENTITY_ALREADY_EXISTS,
                409,
                format!("The messaging entity '{topic}/Subscriptions/{name}' already exists."),
            ));
        }
        if t.subs.len() >= MAX_SUBSCRIPTIONS_PER_TOPIC {
            return Err(SbError::new(
                azemu_amqp::condition::RESOURCE_LIMIT_EXCEEDED,
                403,
                format!("The maximum number of subscriptions on a topic ({MAX_SUBSCRIPTIONS_PER_TOPIC}) has been reached."),
            ));
        }
        let path = format!("{}/Subscriptions/{}", t.name, name);
        t.subs.insert(key(name), Subscription { name: name.to_string(), core: Core::new(path, settings), rules });
        next_revision();
        Ok(())
    }
}

/// Where a send lands.
pub enum SendTarget {
    Queue(String),
    Topic(String),
}

impl State {
    /// The key of a namespace in `namespaces` (`default` may be an alias of the one the config file names).
    pub fn resolve(&self, name: &str) -> String {
        let k = key(name);
        match (&self.default_alias, k.as_str()) {
            (Some(alias), DEFAULT_NAMESPACE) => alias.clone(),
            _ => k,
        }
    }

    pub fn ns(&mut self, name: &str) -> &mut Namespace {
        let k = self.resolve(name);
        self.namespaces.entry(k).or_default()
    }

    /// Wake every receiver that waits on any entity (the state is being dropped).
    pub fn wake_receivers(&mut self) {
        for ns in self.namespaces.values_mut() {
            ns.wake_receivers();
        }
    }

    /// Put a message into a queue or topic, following auto-forwarding chains.
    /// Returns the sequence number assigned by the first entity that stored it (queue sends and
    /// scheduled topic sends; 0 for plain topic fan-out).
    pub fn send(&mut self, ns_name: &str, target: &Address, inc: Incoming, now: i64) -> Result<u64, SbError> {
        self.send_reserved(ns_name, target, inc, now, None)
    }

    /// [`State::send`] for a message whose sequence number was reserved with [`State::reserve_seq`]
    /// (a scheduled send in a transaction).
    pub fn send_reserved(
        &mut self,
        ns_name: &str,
        target: &Address,
        mut inc: Incoming,
        now: i64,
        reserved: Option<u64>,
    ) -> Result<u64, SbError> {
        assign_message_id(&mut inc.message);
        self.deliver(ns_name, target, inc, now, 0, reserved)
    }

    /// Take the sequence number a scheduled message sent to `target` will get: the one of the
    /// entity it ends up in once auto-forwarding is followed (a queue's main queue, or the
    /// topic's list of scheduled messages).
    pub fn reserve_seq(&mut self, ns_name: &str, target: &Address) -> Result<u64, SbError> {
        let mut target = target.clone();
        for _ in 0..=MAX_FORWARD_DEPTH {
            let ns = self.ns(ns_name);
            match &target {
                Address::Queue { name, sub: SubQueue::Main } => {
                    let q = ns.queue_mut(name).ok_or_else(|| SbError::entity_not_found(name, ns_name))?;
                    match q.core.settings.forward_to.clone() {
                        Some(f) => target = parse_target(&f).ok_or_else(|| SbError::entity_not_found(&f, ns_name))?,
                        None => return Ok(q.core.main.alloc_seq()),
                    }
                }
                Address::Topic { name } => {
                    let t = ns.topic_mut(name).ok_or_else(|| SbError::entity_not_found(name, ns_name))?;
                    return Ok(t.scheduled.alloc_seq());
                }
                other => return Err(SbError::not_allowed(format!("cannot send to {other:?}"))),
            }
        }
        Err(SbError::new(
            azemu_amqp::condition::RESOURCE_LIMIT_EXCEEDED,
            400,
            "Maximum auto-forward depth exceeded (the forwarding chain is too long or loops).",
        ))
    }

    fn deliver(
        &mut self,
        ns_name: &str,
        target: &Address,
        inc: Incoming,
        now: i64,
        depth: usize,
        reserved: Option<u64>,
    ) -> Result<u64, SbError> {
        if depth > MAX_FORWARD_DEPTH {
            return Err(SbError::new(
                azemu_amqp::condition::RESOURCE_LIMIT_EXCEEDED,
                400,
                "Maximum auto-forward depth exceeded (the forwarding chain is too long or loops).",
            ));
        }
        let ns = self.ns(ns_name);
        match target {
            Address::Queue { name, sub: SubQueue::Main } => {
                let q = ns.queue_mut(name).ok_or_else(|| SbError::entity_not_found(name, ns_name))?;
                if let Some(f) = q.core.settings.forward_to.clone() {
                    let t = parse_target(&f).ok_or_else(|| SbError::entity_not_found(&f, ns_name))?;
                    if q.core.is_duplicate(&inc.message, now) {
                        return Ok(0);
                    }
                    let mut inc = inc;
                    if inc.enq_seq.is_none() {
                        inc.enq_seq = Some(q.core.main.alloc_seq());
                    }
                    return self.deliver(ns_name, &t, inc, now, depth + 1, reserved);
                }
                let seq = match q.core.enqueue_with_seq(inc, now, reserved)? {
                    Enqueued::Stored(s) => s,
                    Enqueued::Duplicate => 0,
                };
                Ok(seq)
            }
            Address::Topic { name } => {
                let t = ns.topic_mut(name).ok_or_else(|| SbError::entity_not_found(name, ns_name))?;
                t.check_send(inc.size)?;
                if t.settings.requires_duplicate_detection {
                    if let Some(id) = message_id_string(&inc.message) {
                        if t.seen_before(id, now) {
                            return Ok(0);
                        }
                    }
                }
                if let Some(when) = inc.scheduled_ms.filter(|w| *w > now) {
                    // scheduled messages wait in the topic's own store: it is bounded by the topic's size limit like a queue
                    if t.scheduled.bytes + charged(inc.size) > t.settings.max_size_bytes() {
                        return Err(SbError::new(
                            azemu_amqp::condition::RESOURCE_LIMIT_EXCEEDED,
                            403,
                            format!(
                                "The maximum entity size has been reached or exceeded for Entity: '{}' (scheduled messages). Size in bytes:{}, Max entity size in bytes: {}. For more information please see https://aka.ms/ServiceBusEntitySizeLimit",
                                t.name,
                                t.scheduled.bytes,
                                t.settings.max_size_bytes()
                            ),
                        ));
                    }
                    let seq = reserved.unwrap_or_else(|| t.scheduled.alloc_seq());
                    t.scheduled.insert(Msg {
                        seq,
                        enq_seq: seq,
                        state: MsgState::Scheduled,
                        message: Arc::new(inc.message.clone()),
                        enqueued_ms: now,
                        scheduled_ms: Some(when),
                        ttl_ms: None,
                        expires_ms: None,
                        delivery_count: 0,
                        lock: None,
                        session_id: None,
                        size: inc.size,
                        dead_letter_source: None,
                        deferred: false,
                    });
                    return Ok(seq);
                }
                let forwards = t.fan_out(&inc, now)?;
                for f in forwards {
                    self.forward(ns_name, f, now, depth + 1);
                }
                Ok(0)
            }
            other => Err(SbError::not_allowed(format!("cannot send to {other:?}"))),
        }
    }

    /// Deliver a forwarded message; errors are dropped (the message goes nowhere, as when the
    /// forward target was deleted).
    pub fn forward(&mut self, ns_name: &str, f: Forward, now: i64, depth: usize) {
        let Some(target) = parse_target(&f.target) else { return };
        if let Err(e) = self.deliver(ns_name, &target, f.incoming, now, depth, None) {
            tracing::debug!("forward to {} failed: {}", f.target, e.description);
        }
    }

    pub fn forward_all(&mut self, ns_name: &str, forwards: Vec<Forward>, now: i64) {
        for f in forwards {
            self.forward(ns_name, f, now, 1);
        }
    }

    /// Apply due transitions of every entity.
    pub fn tick_due(&mut self, now: i64) {
        let mut pending: Vec<(String, Vec<Forward>)> = Vec::new();
        for (ns_name, ns) in self.namespaces.iter_mut() {
            for q in ns.queues.values_mut() {
                if q.core.next_due().map(|d| d <= now).unwrap_or(false) {
                    let f = q.core.tick(now);
                    if !f.is_empty() {
                        pending.push((ns_name.clone(), f));
                    }
                }
            }
            for t in ns.topics.values_mut() {
                let mut due = Vec::new();
                while let Some(seq) = t.scheduled.pop_due_scheduled(now) {
                    if let Some(m) = t.scheduled.remove(seq) {
                        due.push(m);
                    }
                }
                for m in due {
                    let mut inc = Incoming::new(Arc::unwrap_or_clone(m.message), m.size);
                    inc.scheduled_ms = None;
                    match t.fan_out(&inc, now) {
                        Ok(f) => pending.push((ns_name.clone(), f)),
                        Err(e) => tracing::debug!("scheduled fan-out failed: {}", e.description),
                    }
                }
                for s in t.subs.values_mut() {
                    if s.core.next_due().map(|d| d <= now).unwrap_or(false) {
                        let f = s.core.tick(now);
                        if !f.is_empty() {
                            pending.push((ns_name.clone(), f));
                        }
                    }
                }
            }
        }
        for (ns, f) in pending {
            self.forward_all(&ns, f, now);
        }
    }
}

/// `queue`, `topic` or `sb://host/queue` as a send target.
pub fn parse_target(path: &str) -> Option<Address> {
    match Address::parse(path)? {
        Address::Queue { name, sub: SubQueue::Main } => Some(Address::Queue { name, sub: SubQueue::Main }),
        _ => None,
    }
}

/// Process-wide shared state of the Service Bus emulator, used by both the AMQP side and the
/// HTTP management side.
pub struct Shared {
    pub state: Mutex<State>,
    pub autocreate: bool,
    pub config_path: Option<std::path::PathBuf>,
    next_conn: AtomicU64,
    ticker_started: AtomicBool,
    /// How long an AMQP transaction lives before it is rolled back (Azure: 2 minutes).
    txn_timeout_ms: AtomicU64,
    /// What open AMQP transactions hold in memory, against their limits.
    pub txn_budget: Arc<crate::txn::TxnBudget>,
}

impl Shared {
    pub fn new(autocreate: bool, config_path: Option<std::path::PathBuf>) -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(State::default()),
            autocreate,
            config_path,
            next_conn: AtomicU64::new(1),
            ticker_started: AtomicBool::new(false),
            txn_timeout_ms: AtomicU64::new(120_000),
            txn_budget: crate::txn::TxnBudget::new(crate::txn::TxnLimits::default()),
        })
    }

    /// From the environment, through `azemu_core::settings` (read here, when the service is built):
    /// `AZEMU_SERVICEBUS_AUTOCREATE` (off as on Azure; on with `AZEMU_LENIENT=1`), `AZEMU_SERVICEBUS_CONFIG`
    /// (path of a Config.json), `AZEMU_SERVICEBUS_TXN_TIMEOUT_SECONDS` and the three `..._MAX_MB` limits.
    pub fn from_env() -> Arc<Shared> {
        use azemu_core::settings;
        settings::describe("AZEMU_SERVICEBUS_AUTOCREATE", "off (on with `AZEMU_LENIENT`)", "A queue, topic or subscription that is sent to or read from but was never created comes into being, instead of the not-found answer Azure gives.");
        settings::describe(
            "AZEMU_SERVICEBUS_CONFIG",
            "none",
            "Path of a JSON file with queues, topics, subscriptions and rules to create at start and after every reset.",
        );
        settings::describe(
            "AZEMU_SERVICEBUS_TXN_TIMEOUT_SECONDS",
            "120 (seconds)",
            "How long an AMQP transaction may stay open before it is rolled back (seconds, or a number with ms, s, m).",
        );
        settings::describe("AZEMU_SERVICEBUS_TXN_MAX_MB", "128", "Most megabytes of messages one open AMQP transaction may hold.");
        settings::describe(
            "AZEMU_SERVICEBUS_TXN_CONNECTION_MAX_MB",
            "256",
            "Most megabytes of messages the open transactions of one connection may hold.",
        );
        settings::describe(
            "AZEMU_SERVICEBUS_TXN_TOTAL_MAX_MB",
            "1024",
            "Most megabytes of messages all open transactions together may hold.",
        );
        let autocreate = settings::lenient_flag("AZEMU_SERVICEBUS_AUTOCREATE");
        let config = settings::text("AZEMU_SERVICEBUS_CONFIG").map(std::path::PathBuf::from);
        let s = Shared::new(autocreate, config);
        let timeout = settings::duration("AZEMU_SERVICEBUS_TXN_TIMEOUT_SECONDS", Duration::from_secs(120));
        if !timeout.is_zero() {
            s.set_txn_timeout(timeout);
        }
        let mut limits = crate::txn::TxnLimits::default();
        let mib = |name: &str, default: usize| match settings::number(name, (default >> 20) as u64) {
            0 => default,
            n => (n as usize).saturating_mul(1 << 20),
        };
        limits.per_txn = mib("AZEMU_SERVICEBUS_TXN_MAX_MB", limits.per_txn);
        limits.per_connection = mib("AZEMU_SERVICEBUS_TXN_CONNECTION_MAX_MB", limits.per_connection);
        limits.total = mib("AZEMU_SERVICEBUS_TXN_TOTAL_MAX_MB", limits.total);
        s.txn_budget.set_limits(limits);
        s.load_config();
        s
    }

    /// The time an AMQP transaction may stay open (default 2 minutes, `AZEMU_SERVICEBUS_TXN_TIMEOUT_SECONDS`).
    pub fn txn_timeout(&self) -> Duration {
        Duration::from_millis(self.txn_timeout_ms.load(Ordering::Relaxed))
    }

    pub fn set_txn_timeout(&self, d: Duration) {
        self.txn_timeout_ms.store(d.as_millis().clamp(1, u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    pub fn next_conn_id(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    /// Drop all entities and messages, then reload the config file.
    pub fn reset(&self) {
        let mut old = std::mem::take(&mut *self.state.lock());
        old.wake_receivers();
        next_revision();
        self.load_config();
    }

    /// Empty one namespace (the name as in the host name, `myns` for `myns.servicebus.localhost`) and put back what the
    /// config file declares for it, as [`reset`](Self::reset) does for all of them. Receivers waiting there are woken
    /// so each notices that its queue is gone. `true` when the namespace had entities. Other namespaces are untouched.
    pub fn reset_namespace(&self, name: &str) -> bool {
        let (key, had) = {
            let mut st = self.state.lock();
            let key = st.resolve(name);
            let old = st.namespaces.remove(&key);
            let had = old.is_some();
            if let Some(mut ns) = old {
                ns.wake_receivers();
            }
            (key, had)
        };
        next_revision();
        if let Some(path) = &self.config_path {
            if let Ok(cfg) = crate::config::load(path) {
                let mut fresh = State::default();
                if crate::config::apply(&mut fresh, &cfg, now_ms()).is_ok() {
                    if let Some(ns) = fresh.namespaces.remove(&key) {
                        self.state.lock().namespaces.insert(key, ns);
                    }
                }
            }
        }
        had
    }

    pub fn load_config(&self) {
        if let Some(path) = &self.config_path {
            match crate::config::load(path) {
                Ok(cfg) => {
                    let mut st = self.state.lock();
                    if let Err(e) = crate::config::apply(&mut st, &cfg, now_ms()) {
                        tracing::error!("servicebus config {} was not applied: {e}", path.display());
                    }
                }
                Err(e) => tracing::warn!("servicebus config {}: {e}", path.display()),
            }
        }
    }

    /// Start the background tick (once) if a tokio runtime is running.
    pub fn ensure_ticker(self: &Arc<Shared>) {
        if self.ticker_started.load(Ordering::Relaxed) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        if self.ticker_started.swap(true, Ordering::Relaxed) {
            return;
        }
        let weak: Weak<Shared> = Arc::downgrade(self);
        handle.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let Some(shared) = weak.upgrade() else { return };
                shared.state.lock().tick_due(now_ms());
            }
        });
    }
}

/// Service Bus gives a message a message-id when the sender set none (a GUID, 32 hex digits);
/// the JavaScript SDK keys its auto lock renewal on it and fails to settle a message without one.
/// Done once at the front door, so every subscription copy of a topic message has the same id.
pub(crate) fn assign_message_id(m: &mut azemu_amqp::message::Message) {
    let props = m.properties.get_or_insert_with(Default::default);
    if props.message_id.as_ref().map(|v| v.is_null()).unwrap_or(true) {
        props.message_id = Some(azemu_amqp::types::Value::String(uuid::Uuid::new_v4().simple().to_string()));
    }
}
