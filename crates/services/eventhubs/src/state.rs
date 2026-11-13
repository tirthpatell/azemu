//! Namespaces, event hubs, the receiver registry (who is reading which partition) and the
//! shared handle both the AMQP and the HTTP side use.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use azemu_amqp::api::{LinkHandle, LinkId};
use azemu_amqp::message::Message;
use parking_lot::Mutex;

use crate::errors::EhError;
use crate::model::*;

pub const DEFAULT_NAMESPACE: &str = "default";

/// Event Hubs quota: receivers without an epoch per partition and consumer group.
pub const MAX_NON_EPOCH_RECEIVERS: usize = 5;

static NEXT_HUB_ID: AtomicU64 = AtomicU64::new(1);

/// After loading: hubs made from now on get ids above those that came back from disk.
pub fn raise_hub_ids(above: u64) {
    NEXT_HUB_ID.fetch_max(above.saturating_add(1), Ordering::Relaxed);
}

pub fn now_ms() -> i64 {
    (azemu_core::time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

/// A receiver link that is attached to a partition.
pub struct RecvReg {
    pub conn: u64,
    pub link: LinkId,
    pub pid: usize,
    /// lower-case consumer group
    pub cg: String,
    pub epoch: Option<i64>,
    pub handle: LinkHandle,
}

#[derive(Default)]
pub struct Namespace {
    pub hubs: BTreeMap<String, Hub>,
}

/// Where a sender link sends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SendTarget {
    /// The event hub itself: the service picks the partition.
    Any,
    Partition(usize),
}

#[derive(Default)]
pub struct State {
    pub namespaces: BTreeMap<String, Namespace>,
    /// When a config file names exactly one namespace, `default` (what `sb://localhost`
    /// resolves to) is an alias of it.
    pub default_alias: Option<String>,
    receivers: HashMap<(String, String), Vec<RecvReg>>,
    /// Woken (all waiters) whenever events are stored or an event hub goes away: what a Kafka fetch that is waiting
    /// for data listens to. A reset replaces the whole state, so a waiter takes the current one on every round.
    pub notify: Arc<tokio::sync::Notify>,
}

impl State {
    /// The namespace a name stands for (`default` may be an alias).
    pub fn resolve(&self, ns: &str) -> String {
        let k = key(ns);
        match (&self.default_alias, k.as_str()) {
            (Some(a), DEFAULT_NAMESPACE) => a.clone(),
            _ => k,
        }
    }

    /// A namespace exists once an event hub was created in it or a config file declared it.
    pub fn knows_namespace(&self, ns: &str) -> bool {
        self.namespaces.contains_key(&self.resolve(ns))
    }

    pub fn declare_namespace(&mut self, ns: &str) {
        let k = self.resolve(ns);
        self.namespaces.entry(k).or_default();
        next_revision();
    }

    pub fn hub(&self, ns: &str, hub: &str) -> Option<&Hub> {
        self.namespaces.get(&self.resolve(ns))?.hubs.get(&key(hub))
    }

    pub fn hub_mut(&mut self, ns: &str, hub: &str) -> Option<&mut Hub> {
        let n = self.resolve(ns);
        self.namespaces.get_mut(&n)?.hubs.get_mut(&key(hub))
    }

    pub fn create_hub(&mut self, ns: &str, name: &str, partitions: u32, retention_ms: i64, now: i64) -> Result<&mut Hub, EhError> {
        if partitions == 0 || partitions > MAX_PARTITIONS {
            return Err(EhError::out_of_range(format!("PartitionCount must be between 1 and {MAX_PARTITIONS}.")));
        }
        if retention_ms <= 0 {
            return Err(EhError::out_of_range("The message retention must be positive."));
        }
        if name.is_empty() || name.contains('/') {
            return Err(EhError::argument(format!("'{name}' is not a valid event hub name.")));
        }
        if self.hub(ns, name).is_some() {
            return Err(EhError::already_exists(&format!("The event hub '{name}'")));
        }
        if self.namespaces.get(&self.resolve(ns)).map(|n| n.hubs.len()).unwrap_or(0) >= MAX_HUBS_PER_NAMESPACE {
            return Err(EhError::new(
                azemu_amqp::condition::RESOURCE_LIMIT_EXCEEDED,
                403,
                format!("The maximum number of event hubs in a namespace ({MAX_HUBS_PER_NAMESPACE}) has been reached."),
            ));
        }
        // process-wide, so a hub created after a reset never has the id of one from before it
        let id = NEXT_HUB_ID.fetch_add(1, Ordering::Relaxed);
        let nsk = self.resolve(ns);
        let n = self.namespaces.entry(nsk).or_default();
        let h = n.hubs.entry(key(name)).or_insert_with(|| Hub::new(name, id, partitions, retention_ms, now));
        next_revision();
        Ok(h)
    }

    /// The event hub exists, or is created with the defaults when `autocreate` is on.
    pub fn ensure_hub(&mut self, ns: &str, name: &str, autocreate: bool, now: i64) -> Result<(), EhError> {
        if self.hub(ns, name).is_some() {
            return Ok(());
        }
        if !autocreate {
            return Err(EhError::entity_not_found(&self.resolve(ns), name));
        }
        self.create_hub(ns, name, DEFAULT_PARTITIONS, DEFAULT_RETENTION_MS, now)?.autocreated = true;
        Ok(())
    }

    pub fn delete_hub(&mut self, ns: &str, name: &str) -> bool {
        let nsk = self.resolve(ns);
        let removed = self.namespaces.get_mut(&nsk).and_then(|n| n.hubs.remove(&key(name))).is_some();
        if removed {
            next_revision();
            self.wake(&nsk, name, None);
        }
        removed
    }

    pub fn wake(&self, ns: &str, hub: &str, pid: Option<usize>) {
        self.notify.notify_waiters();
        if let Some(regs) = self.receivers.get(&(self.resolve(ns), key(hub))) {
            for r in regs.iter().filter(|r| pid.map(|p| p == r.pid).unwrap_or(true)) {
                r.handle.wake();
            }
        }
    }

    /// Wake every receiver of every hub (before the state is dropped by a reset).
    pub fn wake_everyone(&self) {
        self.notify.notify_waiters();
        for regs in self.receivers.values() {
            for r in regs {
                r.handle.wake();
            }
        }
    }

    /// Store events. `batch` events sent to the hub without a partition key stay together.
    pub fn store(&mut self, ns: &str, hub: &str, target: SendTarget, msgs: Vec<Message>, batch: bool, now: i64) -> Result<(), EhError> {
        self.store_prepared(ns, hub, target, msgs.into_iter().map(prepare).collect(), batch, now)
    }

    /// [`State::store`] for events that went through [`prepare`] (done outside the lock).
    pub fn store_prepared(
        &mut self,
        ns: &str,
        hub: &str,
        target: SendTarget,
        msgs: Vec<PreparedEvent>,
        batch: bool,
        now: i64,
    ) -> Result<(), EhError> {
        let nsk = self.resolve(ns);
        let h =
            self.namespaces.get_mut(&nsk).and_then(|n| n.hubs.get_mut(&key(hub))).ok_or_else(|| EhError::entity_not_found(&nsk, hub))?;
        h.trim_all(now);
        let mut touched = Vec::new();
        let mut batch_partition: Option<usize> = None;
        for m in msgs {
            let p = match target {
                SendTarget::Partition(p) => p,
                SendTarget::Any => match m.route_key.as_deref() {
                    Some(k) => h.route(Some(k)),
                    None if batch => *batch_partition.get_or_insert_with(|| h.next_round_robin()),
                    None => h.next_round_robin(),
                },
            };
            h.partitions[p].append_prepared(m, now);
            if !touched.contains(&p) {
                touched.push(p);
            }
        }
        for p in touched {
            self.wake(&nsk, hub, Some(p));
        }
        Ok(())
    }

    /// Register a receiver. A receiver with an epoch takes over every receiver with a lower or
    /// equal epoch (and every one without); a lower epoch, or none, cannot attach next to it.
    pub fn register_receiver(&mut self, ns: &str, hub: &str, reg: RecvReg) -> Result<(), EhError> {
        let k = (self.resolve(ns), key(hub));
        let list = self.receivers.entry(k).or_default();
        let same = |r: &RecvReg| r.pid == reg.pid && r.cg == reg.cg;
        let blocker =
            list.iter().filter(|r| same(r)).filter_map(|r| r.epoch).filter(|e| reg.epoch.map(|mine| *e > mine).unwrap_or(true)).max();
        if let Some(theirs) = blocker {
            return Err(EhError::stolen(match reg.epoch {
                Some(mine) => format!(
                    "Receiver with a higher epoch of '{theirs}' already exists. Receiver with epoch {mine} cannot be created. Make sure you are creating receiver with increasing epoch value to ensure connectivity, or ensure all old epoch receivers are closed or disconnected. TrackingId:{}",
                    crate::errors::tracking_id()
                ),
                None => format!(
                    "A receiver with epoch '{theirs}' already exists on this partition and consumer group, so a receiver without an epoch cannot be created. TrackingId:{}",
                    crate::errors::tracking_id()
                ),
            }));
        }
        if reg.epoch.is_none() && list.iter().filter(|r| same(r) && r.epoch.is_none()).count() >= MAX_NON_EPOCH_RECEIVERS {
            return Err(EhError::new(
                azemu_amqp::condition::RESOURCE_LIMIT_EXCEEDED,
                403,
                format!(
                    "Exceeded the maximum number of allowed receivers per partition in a consumer group which is {MAX_NON_EPOCH_RECEIVERS}. Close a receiver, or use a receiver with an epoch (owner level). TrackingId:{}",
                    crate::errors::tracking_id()
                ),
            ));
        }
        if let Some(mine) = reg.epoch {
            list.retain(|r| {
                if same(r) {
                    r.handle.detach(Some(
                        EhError::stolen(format!(
                            "New receiver with higher epoch of '{mine}' is created hence current receiver with epoch '{}' is getting disconnected. If you are recreating the receiver, make sure a higher epoch is used. TrackingId:{}",
                            r.epoch.map(|e| e.to_string()).unwrap_or_else(|| "none".into()),
                            crate::errors::tracking_id()
                        ))
                        .to_amqp(),
                    ));
                    false
                } else {
                    true
                }
            });
        }
        list.push(reg);
        Ok(())
    }

    pub fn unregister_receiver(&mut self, ns: &str, hub: &str, conn: u64, link: LinkId) {
        let k = (self.resolve(ns), key(hub));
        if let Some(list) = self.receivers.get_mut(&k) {
            list.retain(|r| !(r.conn == conn && r.link == link));
            if list.is_empty() {
                self.receivers.remove(&k);
            }
        }
    }

    pub fn receiver_count(&self, ns: &str, hub: &str) -> usize {
        self.receivers.get(&(self.resolve(ns), key(hub))).map(|l| l.len()).unwrap_or(0)
    }
}

/// Process-wide shared state of the Event Hubs emulator, used by the AMQP side, the HTTP side
/// and the connection multiplexer.
pub struct Shared {
    pub state: Mutex<State>,
    pub autocreate: bool,
    pub config_path: Option<std::path::PathBuf>,
    next_conn: AtomicU64,
    /// Kafka consumer groups and committed offsets (separate from the AMQP consumer groups, as on Azure).
    pub kafka: crate::kafka::groups::Coordinator,
    /// The next producer id Kafka `InitProducerId` hands out (saved, so an id is never given twice).
    pub next_producer_id: std::sync::atomic::AtomicI64,
}

impl Shared {
    pub fn new(autocreate: bool, config_path: Option<std::path::PathBuf>) -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(State::default()),
            autocreate,
            config_path,
            next_conn: AtomicU64::new(1),
            kafka: Default::default(),
            next_producer_id: std::sync::atomic::AtomicI64::new(1),
        })
    }

    /// From the environment, through `azemu_core::settings` (read here, when the service is built):
    /// `AZEMU_EVENTHUBS_AUTOCREATE` (off as on Azure; on with `AZEMU_LENIENT=1`: event hubs and consumer groups, over
    /// AMQP and Kafka alike), `AZEMU_EVENTHUBS_CONFIG` (path of a Config.json) and
    /// `AZEMU_KAFKA_INITIAL_REBALANCE_DELAY_MS`.
    pub fn from_env() -> Arc<Shared> {
        use azemu_core::settings;
        settings::describe("AZEMU_EVENTHUBS_AUTOCREATE", "off (on with `AZEMU_LENIENT`)", "An event hub or consumer group that is sent to or read from but was never created comes into being (over AMQP and over the Kafka endpoint), instead of the not-found answer Azure gives.");
        settings::describe(
            "AZEMU_EVENTHUBS_CONFIG",
            "none",
            "Path of a JSON file with event hubs and consumer groups to create at start and after every reset.",
        );
        settings::describe(
            "AZEMU_KAFKA_INITIAL_REBALANCE_DELAY_MS",
            "500",
            "Milliseconds the first round of a new Kafka consumer group waits for more members to join (at most 30000).",
        );
        let autocreate = settings::lenient_flag("AZEMU_EVENTHUBS_AUTOCREATE");
        let config = settings::text("AZEMU_EVENTHUBS_CONFIG").map(std::path::PathBuf::from);
        let delay_ms = settings::number("AZEMU_KAFKA_INITIAL_REBALANCE_DELAY_MS", 500);
        let mut s = Shared::new(autocreate, config);
        if let Some(sh) = Arc::get_mut(&mut s) {
            sh.kafka = crate::kafka::groups::Coordinator::with_initial_delay(Duration::from_millis(delay_ms.min(30_000)));
        }
        s.load_config();
        s
    }

    /// Delete an event hub and what the Kafka side remembers of it (committed offsets), whoever asked for it: AMQP management,
    /// the admin API, Kafka `DeleteTopics` or ARM. Returns whether it existed.
    pub fn delete_hub(&self, ns: &str, name: &str) -> bool {
        let deleted = self.state.lock().delete_hub(ns, name);
        if deleted {
            self.kafka.forget_topic(ns, name);
            self.kafka.forget_topic(&self.state.lock().resolve(ns), name);
        }
        deleted
    }

    /// Delete a namespace with its event hubs and Kafka groups.
    pub fn delete_namespace(&self, ns: &str) {
        let hubs: Vec<String> = {
            let st = self.state.lock();
            st.namespaces.get(&st.resolve(ns)).map(|n| n.hubs.values().map(|h| h.name.clone()).collect()).unwrap_or_default()
        };
        for h in hubs {
            self.delete_hub(ns, &h);
        }
        let mut st = self.state.lock();
        let k = st.resolve(ns);
        st.namespaces.remove(&k);
        next_revision();
        drop(st);
        self.kafka.forget_namespace(ns);
        if !k.eq_ignore_ascii_case(ns) {
            self.kafka.forget_namespace(&k); // `default` stands for the namespace a config file names
        }
    }

    /// Empty one namespace (the name as in the host name) and put back what the config file declares for it, as
    /// [`reset`](Self::reset) does for all of them: its event hubs and events go, receivers waiting there are woken, its Kafka
    /// groups and offsets end, and persistence sees a change. `true` when the namespace was known. Other namespaces are untouched.
    pub fn reset_namespace(&self, ns: &str) -> bool {
        let known = self.state.lock().knows_namespace(ns);
        self.delete_namespace(ns);
        if let Some(path) = &self.config_path {
            if let Ok(cfg) = crate::config::load(path) {
                let mut fresh = State::default();
                if crate::config::apply(&mut fresh, &cfg, now_ms()).is_ok() {
                    let mut st = self.state.lock();
                    let k = st.resolve(ns);
                    if let Some(n) = fresh.namespaces.remove(&fresh.resolve(ns)) {
                        st.namespaces.insert(k, n);
                        next_revision();
                    }
                }
            }
        }
        known
    }

    pub fn next_conn_id(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    /// Drop all event hubs and events, then reload the config file. Receivers are woken so each
    /// notices that its event hub is gone.
    pub fn reset(&self) {
        let old = std::mem::take(&mut *self.state.lock());
        old.wake_everyone();
        self.kafka.reset();
        self.next_producer_id.store(1, Ordering::Relaxed);
        next_revision();
        self.load_config();
    }

    pub fn load_config(&self) {
        if let Some(path) = &self.config_path {
            match crate::config::load(path) {
                Ok(cfg) => {
                    let mut st = self.state.lock();
                    if let Err(e) = crate::config::apply(&mut st, &cfg, now_ms()) {
                        tracing::warn!("eventhubs config {}: {e}", path.display());
                    }
                }
                Err(e) => tracing::warn!("eventhubs config {}: {e}", path.display()),
            }
        }
    }

    /// Does this namespace name belong to Event Hubs (a hub was created in it, or the config declares it)?
    pub fn knows_namespace(&self, ns: &str) -> bool {
        self.state.lock().knows_namespace(ns)
    }
}
