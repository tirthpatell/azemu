//! Saving the state to disk and reading it back (`--data-dir`). The reasons are in `docs/services/persistence.md`
//! (section "Service Bus").
//!
//! Layout of the service directory:
//!
//! * `servicebus.state` (kind `servicebus`): the namespaces with their queues, topics, subscriptions, every entity
//!   setting and every rule. Small; written again only when its content changed.
//! * `queue-<hash>.state`, `topic-<hash>.state`, `sub-<hash>.state` (kind `servicebus-entity`): the messages of one
//!   queue or subscription (active, deferred, scheduled, locked, the dead-letter queue and the transfer
//!   dead-letter queue, each with its sequence counter), session state, duplicate detection ids and messages waiting
//!   to be forwarded; for a topic its scheduled messages and its duplicate detection ids. Written again only when
//!   that entity changed since the last save.
//!
//! A save copies what it writes under the lock (a stored message is an `Arc`, so a copy is a pointer and a few small
//! fields) and encodes and writes after the lock is released. Entity files are written before `servicebus.state`,
//! and files nothing refers to are removed last. The files of one save are taken at one moment but written one after
//! the other: a kill in the middle of a save leaves entity files from two moments, and a message that was moving
//! between two entities (auto-forwarding, a transaction) at that moment can be in both or in neither.
//!
//! What is not saved: connections, links, message locks and sessions' locks, open transactions, receivers waiting
//! for a message. A message that was locked when the state was copied comes back after the restart as when its lock
//! runs out: delivery count raised by one, dead-lettered at the delivery limit.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::Arc;

use azemu_amqp::message::Message;
use azemu_core::persist;
use azemu_servicebus_sql::{Action, Filter, Params, Value as SqlValue};
use azemu_statefile::{hashed_name, read_state, remove_stale, write_stream};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::core::{Core, Forward, Incoming, SessionInfo};
use crate::model::Settings;
use crate::rules::{CorrelationFilter, Rule, RuleAction, RuleFilter};
use crate::state::{key, Namespace, Queue, State, Subscription, Topic};
use crate::store::{current_revision, next_revision, Lock, Msg, MsgState, Owner, Store};
use crate::{ServiceBusService, Shared};

pub const DEFS_FILE: &str = "servicebus.state";
pub const DEFS_KIND: &str = "servicebus";
pub const ENTITY_KIND: &str = "servicebus-entity";
pub const VERSION: u32 = 1;

// ---- the shapes in the files --------------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
enum SqlValueD {
    Null,
    Bool(bool),
    Long(i64),
    Double(f64),
    Str(String),
    Timestamp(i64),
}

impl From<&SqlValue> for SqlValueD {
    fn from(v: &SqlValue) -> Self {
        match v {
            SqlValue::Null => SqlValueD::Null,
            SqlValue::Bool(b) => SqlValueD::Bool(*b),
            SqlValue::Long(n) => SqlValueD::Long(*n),
            SqlValue::Double(n) => SqlValueD::Double(*n),
            SqlValue::Str(s) => SqlValueD::Str(s.clone()),
            SqlValue::Timestamp(t) => SqlValueD::Timestamp(*t),
        }
    }
}

impl From<SqlValueD> for SqlValue {
    fn from(v: SqlValueD) -> Self {
        match v {
            SqlValueD::Null => SqlValue::Null,
            SqlValueD::Bool(b) => SqlValue::Bool(b),
            SqlValueD::Long(n) => SqlValue::Long(n),
            SqlValueD::Double(n) => SqlValue::Double(n),
            SqlValueD::Str(s) => SqlValue::Str(s),
            SqlValueD::Timestamp(t) => SqlValue::Timestamp(t),
        }
    }
}

type ParamsD = Vec<(String, SqlValueD)>;

fn params_d(p: &Params) -> ParamsD {
    p.iter().map(|(k, v)| (k.clone(), v.into())).collect()
}

fn params_from(p: ParamsD) -> Params {
    p.into_iter().map(|(k, v)| (k, v.into())).collect()
}

#[derive(Serialize, Deserialize)]
enum FilterD {
    True,
    False,
    Sql { expression: String, parameters: ParamsD, compatibility_level: i32 },
    Correlation(CorrelationD),
}

#[derive(Serialize, Deserialize, Default)]
struct CorrelationD {
    correlation_id: Option<String>,
    message_id: Option<String>,
    to: Option<String>,
    reply_to: Option<String>,
    label: Option<String>,
    session_id: Option<String>,
    reply_to_session_id: Option<String>,
    content_type: Option<String>,
    properties: ParamsD,
}

#[derive(Serialize, Deserialize)]
struct RuleD {
    name: String,
    filter: FilterD,
    action: Option<(String, ParamsD)>,
    created_ms: i64,
}

fn rule_d(r: &Rule) -> RuleD {
    RuleD {
        name: r.name.clone(),
        filter: match &r.filter {
            RuleFilter::True => FilterD::True,
            RuleFilter::False => FilterD::False,
            RuleFilter::Sql { expression, parameters, compatibility_level, .. } => {
                FilterD::Sql { expression: expression.clone(), parameters: params_d(parameters), compatibility_level: *compatibility_level }
            }
            RuleFilter::Correlation(c) => FilterD::Correlation(CorrelationD {
                correlation_id: c.correlation_id.clone(),
                message_id: c.message_id.clone(),
                to: c.to.clone(),
                reply_to: c.reply_to.clone(),
                label: c.label.clone(),
                session_id: c.session_id.clone(),
                reply_to_session_id: c.reply_to_session_id.clone(),
                content_type: c.content_type.clone(),
                properties: c.properties.iter().map(|(k, v)| (k.clone(), v.into())).collect(),
            }),
        },
        action: r.action.as_ref().map(|a| (a.expression.clone(), params_d(&a.parameters))),
        created_ms: r.created_ms,
    }
}

fn rule_from(r: RuleD) -> Result<Rule, String> {
    let filter = match r.filter {
        FilterD::True => RuleFilter::True,
        FilterD::False => RuleFilter::False,
        FilterD::Sql { expression, parameters, compatibility_level } => {
            let compiled =
                Filter::parse(&expression).map_err(|e| format!("rule {}: the filter {expression:?} does not parse: {e}", r.name))?;
            RuleFilter::Sql { expression, parameters: params_from(parameters), compatibility_level, compiled }
        }
        FilterD::Correlation(c) => RuleFilter::Correlation(CorrelationFilter {
            correlation_id: c.correlation_id,
            message_id: c.message_id,
            to: c.to,
            reply_to: c.reply_to,
            label: c.label,
            session_id: c.session_id,
            reply_to_session_id: c.reply_to_session_id,
            content_type: c.content_type,
            properties: c.properties.into_iter().map(|(k, v)| (k, v.into())).collect(),
        }),
    };
    let action = match r.action {
        None => None,
        Some((expression, parameters)) => {
            let compiled =
                Action::parse(&expression).map_err(|e| format!("rule {}: the action {expression:?} does not parse: {e}", r.name))?;
            Some(RuleAction { expression, parameters: params_from(parameters), compiled })
        }
    };
    Ok(Rule { name: r.name, filter, action, created_ms: r.created_ms })
}

#[derive(Serialize, Deserialize)]
struct SubD {
    name: String,
    settings: Settings,
    autocreated: bool,
    rules: Vec<RuleD>,
}

#[derive(Serialize, Deserialize)]
struct QueueD {
    name: String,
    settings: Settings,
    autocreated: bool,
}

#[derive(Serialize, Deserialize)]
struct TopicD {
    name: String,
    settings: Settings,
    subs: Vec<SubD>,
}

#[derive(Serialize, Deserialize)]
struct NsD {
    name: String,
    queues: Vec<QueueD>,
    topics: Vec<TopicD>,
}

#[derive(Serialize, Deserialize, Default)]
struct DefsFile {
    default_alias: Option<String>,
    namespaces: Vec<NsD>,
}

#[derive(Serialize, Deserialize)]
struct LockD {
    token: Bytes,
    until_ms: i64,
    via_session: bool,
}

/// One stored message. The message itself is its AMQP encoding, so it comes back exactly as it was sent.
#[derive(Serialize, Deserialize)]
struct MsgD {
    seq: u64,
    enq_seq: u64,
    /// 0 ready, 1 locked, 2 deferred, 3 scheduled
    state: u8,
    message: Bytes,
    enqueued_ms: i64,
    scheduled_ms: Option<i64>,
    ttl_ms: Option<i64>,
    expires_ms: Option<i64>,
    delivery_count: u32,
    lock: Option<LockD>,
    session_id: Option<String>,
    size: usize,
    dead_letter_source: Option<String>,
    deferred: bool,
}

fn msg_d(m: &Msg) -> MsgD {
    MsgD {
        seq: m.seq,
        enq_seq: m.enq_seq,
        state: match m.state {
            MsgState::Ready => 0,
            MsgState::Locked => 1,
            MsgState::Deferred => 2,
            MsgState::Scheduled => 3,
        },
        message: Bytes::from(m.message.encode()),
        enqueued_ms: m.enqueued_ms,
        scheduled_ms: m.scheduled_ms,
        ttl_ms: m.ttl_ms,
        expires_ms: m.expires_ms,
        delivery_count: m.delivery_count,
        lock: m.lock.as_ref().map(|l| LockD { token: Bytes::copy_from_slice(&l.token), until_ms: l.until_ms, via_session: l.via_session }),
        session_id: m.session_id.clone(),
        size: m.size,
        dead_letter_source: m.dead_letter_source.clone(),
        deferred: m.deferred,
    }
}

fn msg_from(m: MsgD) -> Result<Msg, String> {
    let state = match m.state {
        0 => MsgState::Ready,
        1 => MsgState::Locked,
        2 => MsgState::Deferred,
        3 => MsgState::Scheduled,
        n => return Err(format!("message {} has an unknown state {n}", m.seq)),
    };
    let message = Message::decode(&m.message).map_err(|e| format!("message {} cannot be read: {e}", m.seq))?;
    let lock = match m.lock {
        None => None,
        Some(l) => {
            let token: [u8; 16] =
                l.token.as_ref().try_into().map_err(|_| format!("message {} has a lock token of {} bytes", m.seq, l.token.len()))?;
            Some(Lock { token, until_ms: l.until_ms, owner: Owner::default(), via_session: l.via_session })
        }
    };
    Ok(Msg {
        seq: m.seq,
        enq_seq: m.enq_seq,
        state,
        message: Arc::new(message),
        enqueued_ms: m.enqueued_ms,
        scheduled_ms: m.scheduled_ms,
        ttl_ms: m.ttl_ms,
        expires_ms: m.expires_ms,
        delivery_count: m.delivery_count,
        lock,
        session_id: m.session_id,
        size: m.size,
        dead_letter_source: m.dead_letter_source,
        deferred: m.deferred,
    })
}

/// Messages are encoded one at a time while the file is written.
struct MsgsOut(Vec<Msg>);

impl Serialize for MsgsOut {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for m in &self.0 {
            seq.serialize_element(&msg_d(m))?;
        }
        seq.end()
    }
}

#[derive(Serialize)]
struct StoreOut {
    next_seq: u64,
    msgs: MsgsOut,
}

#[derive(Deserialize)]
struct StoreIn {
    next_seq: u64,
    msgs: Vec<MsgD>,
}

#[derive(Serialize, Deserialize)]
struct SessionD {
    id: String,
    state: Bytes,
    last_updated_ms: i64,
}

#[derive(Serialize, Deserialize)]
struct ForwardD {
    target: String,
    message: Bytes,
    scheduled_ms: Option<i64>,
    size: usize,
    enq_seq: Option<u64>,
    dead_letter_source: Option<String>,
}

#[derive(Serialize)]
struct EntityOut {
    /// namespace (lower case), entity kind (`queue`, `topic`, `sub`) and path (lower case): which entity this is
    ns: String,
    kind: String,
    path: String,
    stores: Vec<StoreOut>,
    sessions: Vec<SessionD>,
    dup: Vec<(String, i64)>,
    stash: Vec<ForwardD>,
}

#[derive(Deserialize)]
struct EntityIn {
    ns: String,
    kind: String,
    path: String,
    stores: Vec<StoreIn>,
    sessions: Vec<SessionD>,
    dup: Vec<(String, i64)>,
    stash: Vec<ForwardD>,
}

// ---- what a save copies under the lock ------------------------------------------------------------------------

type Fingerprint = (u64, u64, u64, u64);

struct EntityCopy {
    file: String,
    fp: Fingerprint,
    out: EntityOut,
}

/// What the last save (or load) left on disk.
#[derive(Default)]
pub struct Saved {
    defs_hash: Option<u64>,
    entities: HashMap<String, Fingerprint>,
    revision: u64,
}

fn store_out(s: &Store) -> StoreOut {
    StoreOut { next_seq: s.next_seq(), msgs: MsgsOut(s.msgs.values().cloned().collect()) }
}

fn dup_list(d: &HashMap<String, i64>, now: i64) -> Vec<(String, i64)> {
    d.iter().filter(|(_, exp)| **exp > now).map(|(k, v)| (k.clone(), *v)).collect()
}

fn forward_d(f: &Forward) -> ForwardD {
    ForwardD {
        target: f.target.clone(),
        message: Bytes::from(f.incoming.message.encode()),
        scheduled_ms: f.incoming.scheduled_ms,
        size: f.incoming.size,
        enq_seq: f.incoming.enq_seq,
        dead_letter_source: f.incoming.dead_letter_source.clone(),
    }
}

fn core_copy(ns: &str, kind: &str, path: &str, c: &Core, now: i64) -> EntityCopy {
    let file = hashed_name(kind, &[ns, &path.to_lowercase()]);
    EntityCopy {
        file,
        fp: (c.main.rev, c.dlq.rev, c.tdlq.rev, c.extra_rev),
        out: EntityOut {
            ns: ns.to_string(),
            kind: kind.to_string(),
            path: path.to_lowercase(),
            stores: vec![store_out(&c.main), store_out(&c.dlq), store_out(&c.tdlq)],
            sessions: c
                .sessions
                .iter()
                .filter_map(|(id, s)| {
                    s.state.as_ref().map(|st| SessionD {
                        id: id.clone(),
                        state: Bytes::copy_from_slice(st),
                        last_updated_ms: s.last_updated_ms,
                    })
                })
                .collect(),
            dup: dup_list(&c.dup, now),
            stash: c.stash.iter().map(forward_d).collect(),
        },
    }
}

fn topic_copy(ns: &str, t: &Topic, now: i64) -> EntityCopy {
    let file = hashed_name("topic", &[ns, &t.name.to_lowercase()]);
    EntityCopy {
        file,
        fp: (t.scheduled.rev, t.dup_rev, 0, 0),
        out: EntityOut {
            ns: ns.to_string(),
            kind: "topic".into(),
            path: t.name.to_lowercase(),
            stores: vec![store_out(&t.scheduled)],
            sessions: vec![],
            dup: dup_list(&t.dup, now),
            stash: vec![],
        },
    }
}

fn hash_bytes(b: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

impl Shared {
    fn copy_for_save(&self, saved: &Saved, now: i64) -> (DefsFile, Vec<EntityCopy>, HashSet<String>) {
        let st = self.state.lock();
        let mut defs = DefsFile { default_alias: st.default_alias.clone(), namespaces: vec![] };
        let mut copies = Vec::new();
        let mut keep = HashSet::new();
        for (ns_name, ns) in &st.namespaces {
            if ns.queues.is_empty() && ns.topics.is_empty() {
                continue;
            }
            let mut nd = NsD { name: ns_name.clone(), queues: vec![], topics: vec![] };
            let mut stale = |file: &str, fp: Fingerprint| -> bool {
                keep.insert(file.to_string());
                saved.entities.get(file) != Some(&fp)
            };
            for q in ns.queues.values() {
                nd.queues.push(QueueD { name: q.name.clone(), settings: q.core.settings.clone(), autocreated: q.core.autocreated });
                // the cheap test first: only an entity that changed is copied
                let file = hashed_name("queue", &[ns_name, &q.name.to_lowercase()]);
                if stale(&file, (q.core.main.rev, q.core.dlq.rev, q.core.tdlq.rev, q.core.extra_rev)) {
                    copies.push(core_copy(ns_name, "queue", &q.name, &q.core, now));
                }
            }
            for t in ns.topics.values() {
                let mut td = TopicD { name: t.name.clone(), settings: t.settings.clone(), subs: vec![] };
                let file = hashed_name("topic", &[ns_name, &t.name.to_lowercase()]);
                if stale(&file, (t.scheduled.rev, t.dup_rev, 0, 0)) {
                    copies.push(topic_copy(ns_name, t, now));
                }
                for s in t.subs.values() {
                    td.subs.push(SubD {
                        name: s.name.clone(),
                        settings: s.core.settings.clone(),
                        autocreated: s.core.autocreated,
                        rules: s.rules.iter().map(rule_d).collect(),
                    });
                    let path = format!("{}/{}", t.name, s.name);
                    let file = hashed_name("sub", &[ns_name, &path.to_lowercase()]);
                    if stale(&file, (s.core.main.rev, s.core.dlq.rev, s.core.tdlq.rev, s.core.extra_rev)) {
                        copies.push(core_copy(ns_name, "sub", &path, &s.core, now));
                    }
                }
                nd.topics.push(td);
            }
            defs.namespaces.push(nd);
        }
        (defs, copies, keep)
    }

    pub fn save_to(&self, dir: &Path, saved: &mut Saved) -> io::Result<()> {
        let now = crate::state::now_ms();
        let (defs, copies, keep) = self.copy_for_save(saved, now);
        // -- the lock is released; everything below is encoding and writing --
        for c in copies {
            write_stream(&dir.join(&c.file), ENTITY_KIND, VERSION, &c.out)?;
            saved.entities.insert(c.file, c.fp);
        }
        let bytes = persist::encode(DEFS_KIND, VERSION, &defs)?;
        let h = hash_bytes(&bytes);
        if saved.defs_hash != Some(h) {
            persist::write_atomic_private(&dir.join(DEFS_FILE), &bytes)?;
            saved.defs_hash = Some(h);
        }
        for prefix in ["queue", "topic", "sub"] {
            remove_stale(dir, prefix, &keep)?;
        }
        saved.entities.retain(|k, _| keep.contains(k));
        Ok(())
    }

    pub fn load_from(&self, dir: &Path, saved: &mut Saved) -> io::Result<()> {
        let defs_path = dir.join(DEFS_FILE);
        let Some((_, defs)) = read_state::<DefsFile>(&defs_path, DEFS_KIND, VERSION)? else {
            return Ok(());
        };
        let bad = |p: &Path, m: String| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {m}", p.display()));
        let mut state = State { default_alias: defs.default_alias, ..Default::default() };
        let mut seen = HashSet::new();
        for nd in defs.namespaces {
            let ns_key = key(&nd.name);
            let mut ns = Namespace::default();
            let load_core = |kind: &str, path: &str, core: &mut Core, saved: &mut Saved, seen: &mut HashSet<String>| -> io::Result<()> {
                let file = hashed_name(kind, &[&ns_key, &path.to_lowercase()]);
                if !seen.insert(file.clone()) {
                    return Err(bad(&defs_path, format!("{kind} {path} is listed twice in namespace {}", nd.name)));
                }
                let p = dir.join(&file);
                if let Some((_, f)) = read_state::<EntityIn>(&p, ENTITY_KIND, VERSION)? {
                    if f.ns != ns_key || f.kind != kind || f.path != path.to_lowercase() {
                        return Err(bad(
                            &p,
                            format!("it holds {} {} of namespace {}, expected {kind} {path} of {ns_key}", f.kind, f.path, f.ns),
                        ));
                    }
                    fill_core(core, f).map_err(|m| bad(&p, m))?;
                }
                core.release_locks_after_restart();
                saved.entities.insert(file, (core.main.rev, core.dlq.rev, core.tdlq.rev, core.extra_rev));
                Ok(())
            };
            for q in nd.queues {
                let mut core = Core::new(q.name.clone(), q.settings);
                core.autocreated = q.autocreated;
                load_core("queue", &q.name, &mut core, saved, &mut seen)?;
                if ns.queues.insert(key(&q.name), Queue { name: q.name.clone(), core }).is_some() {
                    return Err(bad(&defs_path, format!("queue {} is listed twice", q.name)));
                }
            }
            for t in nd.topics {
                let mut topic = Topic::new(&t.name, t.settings);
                let file = hashed_name("topic", &[&ns_key, &t.name.to_lowercase()]);
                if !seen.insert(file.clone()) {
                    return Err(bad(&defs_path, format!("topic {} is listed twice in namespace {}", t.name, nd.name)));
                }
                let p = dir.join(&file);
                if let Some((_, f)) = read_state::<EntityIn>(&p, ENTITY_KIND, VERSION)? {
                    if f.ns != ns_key || f.kind != "topic" || f.path != t.name.to_lowercase() {
                        return Err(bad(
                            &p,
                            format!("it holds {} {} of namespace {}, expected topic {} of {ns_key}", f.kind, f.path, f.ns, t.name),
                        ));
                    }
                    fill_topic(&mut topic, f).map_err(|m| bad(&p, m))?;
                }
                saved.entities.insert(file, (topic.scheduled.rev, topic.dup_rev, 0, 0));
                for s in t.subs {
                    let mut core = Core::new(format!("{}/Subscriptions/{}", t.name, s.name), s.settings);
                    core.autocreated = s.autocreated;
                    let path = format!("{}/{}", t.name, s.name);
                    load_core("sub", &path, &mut core, saved, &mut seen)?;
                    let rules = s.rules.into_iter().map(rule_from).collect::<Result<Vec<_>, _>>().map_err(|m| bad(&defs_path, m))?;
                    if topic.subs.insert(key(&s.name), Subscription { name: s.name.clone(), core, rules }).is_some() {
                        return Err(bad(&defs_path, format!("subscription {} of topic {} is listed twice", s.name, t.name)));
                    }
                }
                if ns.topics.insert(key(&t.name), topic).is_some() {
                    return Err(bad(&defs_path, format!("topic {} is listed twice", t.name)));
                }
            }
            if state.namespaces.insert(ns_key, ns).is_some() {
                return Err(bad(&defs_path, format!("namespace {} is listed twice", nd.name)));
            }
        }
        *self.state.lock() = state;
        saved.revision = current_revision();
        Ok(())
    }
}

fn restore_store(store: &mut Store, f: StoreIn) -> Result<(), String> {
    let mut max = 0;
    for m in f.msgs {
        let m = msg_from(m)?;
        max = max.max(m.seq);
        store.insert(m);
    }
    // the counter never goes back, whatever the file says
    store.raise_next_seq(f.next_seq.max(max.saturating_add(1)));
    Ok(())
}

fn fill_core(core: &mut Core, f: EntityIn) -> Result<(), String> {
    let mut stores = f.stores.into_iter();
    for sq in [crate::model::SubQueue::Main, crate::model::SubQueue::DeadLetter, crate::model::SubQueue::TransferDeadLetter] {
        if let Some(s) = stores.next() {
            restore_store(core.store_mut(sq), s)?;
        }
    }
    for s in f.sessions {
        core.sessions.insert(s.id, SessionInfo { lock: None, state: Some(s.state.to_vec()), last_updated_ms: s.last_updated_ms });
    }
    core.dup = f.dup.into_iter().collect();
    for fw in f.stash {
        let message = Message::decode(&fw.message).map_err(|e| format!("a message waiting to be forwarded cannot be read: {e}"))?;
        core.stash.push(Forward {
            target: fw.target,
            incoming: Incoming {
                message,
                scheduled_ms: fw.scheduled_ms,
                size: fw.size,
                enq_seq: fw.enq_seq,
                dead_letter_source: fw.dead_letter_source,
            },
        });
    }
    Ok(())
}

fn fill_topic(t: &mut Topic, f: EntityIn) -> Result<(), String> {
    if let Some(s) = f.stores.into_iter().next() {
        restore_store(&mut t.scheduled, s)?;
    }
    t.dup = f.dup.into_iter().collect();
    Ok(())
}

// ---- the service ------------------------------------------------------------------------------------------------

/// What the service keeps between calls of `save`.
#[derive(Default)]
pub struct SaveMemory(pub parking_lot::Mutex<Saved>);

impl Saved {
    pub fn forget(&mut self) {
        self.defs_hash = None;
        self.entities.clear();
    }
}

impl ServiceBusService {
    pub(crate) fn persist_generation_value(&self) -> u64 {
        current_revision()
    }

    pub(crate) fn save_dir(&self, dir: &Path) -> io::Result<()> {
        let mut saved = self.saved.0.lock();
        self.shared.save_to(dir, &mut saved)
    }

    pub(crate) fn load_dir(&self, dir: &Path) -> io::Result<()> {
        let mut saved = self.saved.0.lock();
        self.shared.load_from(dir, &mut saved)?;
        next_revision();
        Ok(())
    }
}
