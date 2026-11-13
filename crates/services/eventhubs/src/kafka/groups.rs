//! Kafka consumer groups: the classic rebalance protocol (JoinGroup, SyncGroup, Heartbeat, LeaveGroup) and the
//! committed offsets, per namespace. Groups are separate from the AMQP consumer groups, as on Azure ("Kafka consumer
//! groups on Event Hubs are fully distinct from standard Event Hubs consumer groups").
//!
//! One mutex guards everything and is never held across an await. A member waiting for a rebalance (JoinGroup,
//! SyncGroup) waits on a oneshot channel; the one that completes the round (the last member to join, the leader's
//! SyncGroup, or the first waiter whose deadline passes) sends the results. Members that stop heartbeating are
//! removed lazily, whenever the group is touched.
//!
//! Differences from Apache Kafka: no static membership (a `group.instance.id` is stored and handed back but does not
//! fence or survive a restart), no cooperative or KIP-848 protocol, the group is forgotten when it is empty and has no
//! offsets.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::oneshot;
use tokio::time::Instant;

use azemu_kafka::api::err;

pub const MAX_GROUP_ID_BYTES: usize = 256;
pub const MAX_OFFSET_METADATA_BYTES: usize = 1024;
/// Members of one group (Kafka has no fixed limit; this keeps the join answer small).
pub const MAX_MEMBERS: usize = 200;
pub const MAX_GROUPS_PER_NAMESPACE: usize = 10_000;
/// Committed partitions of one group.
pub const MAX_OFFSETS_PER_GROUP: usize = 100_000;
/// Member ids handed out and not yet used (JoinGroup v4+ asks the client to come back with its id).
const MAX_PENDING_IDS: usize = 1_000;
/// Kafka's `group.min.session.timeout.ms` and `group.max.session.timeout.ms`.
pub const MIN_SESSION_TIMEOUT_MS: i32 = 6_000;
pub const MAX_SESSION_TIMEOUT_MS: i32 = 1_800_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GState {
    Empty,
    PreparingRebalance,
    CompletingRebalance,
    Stable,
}

impl GState {
    pub fn name(self) -> &'static str {
        match self {
            GState::Empty => "Empty",
            GState::PreparingRebalance => "PreparingRebalance",
            GState::CompletingRebalance => "CompletingRebalance",
            GState::Stable => "Stable",
        }
    }
}

struct Member {
    instance_id: Option<String>,
    client_id: String,
    client_host: String,
    session_timeout: Duration,
    rebalance_timeout: Duration,
    protocols: Vec<(String, Bytes)>,
    assignment: Bytes,
    last_seen: Instant,
    /// Has a JoinGroup waiting in this round.
    joined: bool,
}

/// The committed offsets of one group, for saving: (namespace, group, [(topic, partition, offset)]).
pub type GroupOffsets = (String, String, Vec<(String, i32, Committed)>);

#[derive(Clone, Debug, PartialEq)]
pub struct Committed {
    pub offset: i64,
    pub metadata: Option<String>,
    pub leader_epoch: i32,
}

struct Group {
    state: GState,
    generation: i32,
    protocol_type: Option<String>,
    protocol: Option<String>,
    leader: Option<String>,
    members: BTreeMap<String, Member>,
    pending_ids: Vec<String>,
    join_waiters: HashMap<String, oneshot::Sender<JoinResult>>,
    sync_waiters: HashMap<String, oneshot::Sender<SyncResult>>,
    deadline: Option<Instant>,
    /// The first round of an empty group waits the whole initial delay, even when everyone has joined.
    initial: bool,
    offsets: HashMap<(String, i32), Committed>,
}

impl Group {
    fn new() -> Group {
        Group {
            state: GState::Empty,
            generation: 0,
            protocol_type: None,
            protocol: None,
            leader: None,
            members: BTreeMap::new(),
            pending_ids: Vec::new(),
            join_waiters: HashMap::new(),
            sync_waiters: HashMap::new(),
            deadline: None,
            initial: false,
            offsets: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct JoinRequest {
    pub group: String,
    pub member_id: String,
    pub instance_id: Option<String>,
    pub client_id: String,
    pub client_host: String,
    pub session_timeout_ms: i32,
    pub rebalance_timeout_ms: i32,
    pub protocol_type: String,
    pub protocols: Vec<(String, Bytes)>,
    pub version: i16,
}

#[derive(Debug, Clone, Default)]
pub struct JoinResult {
    pub error: i16,
    pub generation: i32,
    pub protocol_type: Option<String>,
    pub protocol: Option<String>,
    pub leader: String,
    pub member_id: String,
    /// (member id, instance id, metadata): only for the leader.
    pub members: Vec<(String, Option<String>, Bytes)>,
}

impl JoinResult {
    fn error(code: i16, member_id: &str) -> JoinResult {
        JoinResult { error: code, generation: -1, member_id: member_id.to_string(), ..Default::default() }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SyncResult {
    pub error: i16,
    pub protocol_type: Option<String>,
    pub protocol: Option<String>,
    pub assignment: Bytes,
}

impl SyncResult {
    fn error(code: i16) -> SyncResult {
        SyncResult { error: code, ..Default::default() }
    }
}

pub struct SyncRequest {
    pub group: String,
    pub member_id: String,
    pub generation: i32,
    pub protocol_type: Option<String>,
    pub protocol: Option<String>,
    pub assignments: Vec<(String, Bytes)>,
}

#[derive(Clone, Debug)]
pub struct MemberInfo {
    pub member_id: String,
    pub instance_id: Option<String>,
    pub client_id: String,
    pub client_host: String,
    pub metadata: Bytes,
    pub assignment: Bytes,
}

#[derive(Clone, Debug)]
pub struct GroupInfo {
    pub group: String,
    pub state: &'static str,
    pub protocol_type: String,
    pub protocol: String,
    pub generation: i32,
    pub members: Vec<MemberInfo>,
}

#[derive(Default)]
struct Inner {
    /// (namespace, group id)
    groups: HashMap<(String, String), Group>,
}

pub struct Coordinator {
    inner: Mutex<Inner>,
    /// Wait of the first round of an empty group (Kafka's `group.initial.rebalance.delay.ms`, 3 s there).
    initial_delay: Duration,
}

impl Default for Coordinator {
    /// Half a second; `Shared::from_env` reads `AZEMU_KAFKA_INITIAL_REBALANCE_DELAY_MS` through `settings`.
    fn default() -> Self {
        Coordinator::with_initial_delay(Duration::from_millis(500))
    }
}

fn valid_group_id(g: &str) -> bool {
    !g.is_empty() && g.len() <= MAX_GROUP_ID_BYTES
}

impl Coordinator {
    pub fn with_initial_delay(initial_delay: Duration) -> Coordinator {
        Coordinator { inner: Mutex::new(Inner::default()), initial_delay }
    }

    /// Drop every group (the emulator was reset). Waiting members are answered with an error.
    pub fn reset(&self) {
        let old = std::mem::take(&mut *self.inner.lock());
        crate::model::next_revision();
        drop(old);
    }

    /// Take the offsets of a topic out of every group of the namespace (the topic was deleted).
    pub fn forget_topic(&self, ns: &str, topic: &str) {
        let mut g = self.inner.lock();
        for ((n, _), grp) in g.groups.iter_mut() {
            if n.eq_ignore_ascii_case(ns) {
                grp.offsets.retain(|(t, _), _| !t.eq_ignore_ascii_case(topic));
            }
        }
        crate::model::next_revision();
    }

    /// Drop every group of the namespace (the namespace was deleted).
    pub fn forget_namespace(&self, ns: &str) {
        let removed: Vec<_> = {
            let mut g = self.inner.lock();
            let keys: Vec<_> = g.groups.keys().filter(|(n, _)| n.eq_ignore_ascii_case(ns)).cloned().collect();
            keys.into_iter().filter_map(|k| g.groups.remove(&k)).collect()
        };
        crate::model::next_revision();
        drop(removed);
    }

    // ---- joining ------------------------------------------------------------------------------------------------

    pub async fn join(&self, ns: &str, req: JoinRequest) -> JoinResult {
        let key = (ns.to_string(), req.group.clone());
        let rx = match self.join_start(&key, &req) {
            Ok(rx) => rx,
            Err(r) => return *r,
        };
        self.await_join(&key, rx).await
    }

    fn join_start(&self, key: &(String, String), req: &JoinRequest) -> Result<oneshot::Receiver<JoinResult>, Box<JoinResult>> {
        let now = Instant::now();
        if !valid_group_id(&req.group) {
            return Err(Box::new(JoinResult::error(err::INVALID_GROUP_ID, &req.member_id)));
        }
        if req.session_timeout_ms < MIN_SESSION_TIMEOUT_MS
            || req.session_timeout_ms > MAX_SESSION_TIMEOUT_MS
            || req.rebalance_timeout_ms < 0
        {
            return Err(Box::new(JoinResult::error(err::INVALID_SESSION_TIMEOUT, &req.member_id)));
        }
        if req.protocol_type.is_empty() || req.protocols.is_empty() {
            return Err(Box::new(JoinResult::error(err::INCONSISTENT_GROUP_PROTOCOL, &req.member_id)));
        }
        let mut inner = self.inner.lock();
        if !inner.groups.contains_key(key) && inner.groups.keys().filter(|(n, _)| *n == key.0).count() >= MAX_GROUPS_PER_NAMESPACE {
            return Err(Box::new(JoinResult::error(err::GROUP_MAX_SIZE_REACHED, &req.member_id)));
        }
        let g = inner.groups.entry(key.clone()).or_insert_with(Group::new);
        expire(g, now, self.initial_delay);
        if g.state != GState::Empty || !g.members.is_empty() {
            if g.protocol_type.as_deref() != Some(req.protocol_type.as_str()) {
                return Err(Box::new(JoinResult::error(err::INCONSISTENT_GROUP_PROTOCOL, &req.member_id)));
            }
            if !g.members.is_empty() && candidates(g, Some(&req.member_id), &req.protocols).is_empty() {
                return Err(Box::new(JoinResult::error(err::INCONSISTENT_GROUP_PROTOCOL, &req.member_id)));
            }
        }
        // the member id
        let mut member_id = req.member_id.clone();
        if member_id.is_empty() {
            member_id = format!("{}-{}", if req.client_id.is_empty() { "member" } else { &req.client_id }, uuid::Uuid::new_v4());
            if req.version >= 4 {
                if g.pending_ids.len() >= MAX_PENDING_IDS {
                    g.pending_ids.remove(0);
                }
                g.pending_ids.push(member_id.clone());
                return Err(Box::new(JoinResult::error(err::MEMBER_ID_REQUIRED, &member_id)));
            }
        } else if !g.members.contains_key(&member_id) {
            match g.pending_ids.iter().position(|p| *p == member_id) {
                Some(i) => {
                    g.pending_ids.remove(i);
                }
                None => return Err(Box::new(JoinResult::error(err::UNKNOWN_MEMBER_ID, &member_id))),
            }
        }
        if !g.members.contains_key(&member_id) && g.members.len() >= MAX_MEMBERS {
            return Err(Box::new(JoinResult::error(err::GROUP_MAX_SIZE_REACHED, &member_id)));
        }
        if g.state == GState::Empty && g.members.is_empty() {
            g.protocol_type = Some(req.protocol_type.clone());
        }
        let session = Duration::from_millis(req.session_timeout_ms as u64);
        let rebalance =
            Duration::from_millis(if req.rebalance_timeout_ms > 0 { req.rebalance_timeout_ms } else { req.session_timeout_ms } as u64);
        g.members.insert(
            member_id.clone(),
            Member {
                instance_id: req.instance_id.clone(),
                client_id: req.client_id.clone(),
                client_host: req.client_host.clone(),
                session_timeout: session,
                rebalance_timeout: rebalance,
                protocols: req.protocols.clone(),
                assignment: Bytes::new(),
                last_seen: now,
                joined: true,
            },
        );
        let (tx, rx) = oneshot::channel();
        // an earlier join of the same member that is still waiting is replaced (its channel closes)
        g.join_waiters.insert(member_id.clone(), tx);
        if g.state != GState::PreparingRebalance {
            prepare_rebalance(g, now, self.initial_delay);
        }
        maybe_complete(g, now);
        Ok(rx)
    }

    async fn await_join(&self, key: &(String, String), mut rx: oneshot::Receiver<JoinResult>) -> JoinResult {
        loop {
            // wake at the round's deadline, or in a second to look for members that stopped heartbeating
            let tick = Instant::now() + Duration::from_secs(1);
            let wake = {
                let inner = self.inner.lock();
                inner.groups.get(key).and_then(|g| g.deadline).map_or(tick, |d| d.min(tick))
            };
            tokio::select! {
                r = &mut rx => return r.unwrap_or_else(|_| JoinResult::error(err::COORDINATOR_NOT_AVAILABLE, "")),
                _ = tokio::time::sleep_until(wake) => {
                    let mut inner = self.inner.lock();
                    if let Some(g) = inner.groups.get_mut(key) {
                        let now = Instant::now();
                        expire(g, now, self.initial_delay);
                        if g.state == GState::PreparingRebalance && g.deadline.is_some_and(|d| now >= d) {
                            complete_join(g, now);
                        }
                    }
                }
            }
        }
    }

    // ---- syncing ------------------------------------------------------------------------------------------------

    pub async fn sync(&self, ns: &str, req: SyncRequest) -> SyncResult {
        let key = (ns.to_string(), req.group.clone());
        let (rx, wait) = {
            let now = Instant::now();
            let mut inner = self.inner.lock();
            let Some(g) = inner.groups.get_mut(&key) else { return SyncResult::error(err::UNKNOWN_MEMBER_ID) };
            expire(g, now, self.initial_delay);
            if !g.members.contains_key(&req.member_id) {
                return SyncResult::error(err::UNKNOWN_MEMBER_ID);
            }
            if req.generation != g.generation {
                return SyncResult::error(err::ILLEGAL_GENERATION);
            }
            if req.protocol_type.as_deref().is_some_and(|t| Some(t) != g.protocol_type.as_deref())
                || req.protocol.as_deref().is_some_and(|p| Some(p) != g.protocol.as_deref())
            {
                return SyncResult::error(err::INCONSISTENT_GROUP_PROTOCOL);
            }
            if let Some(m) = g.members.get_mut(&req.member_id) {
                m.last_seen = now;
            }
            match g.state {
                GState::PreparingRebalance => return SyncResult::error(err::REBALANCE_IN_PROGRESS),
                GState::Empty => return SyncResult::error(err::UNKNOWN_MEMBER_ID),
                GState::Stable => {
                    let a = g.members.get(&req.member_id).map(|m| m.assignment.clone()).unwrap_or_default();
                    return SyncResult { error: 0, protocol_type: g.protocol_type.clone(), protocol: g.protocol.clone(), assignment: a };
                }
                GState::CompletingRebalance => {}
            }
            if g.leader.as_deref() == Some(req.member_id.as_str()) {
                let mut given: HashMap<String, Bytes> = req.assignments.into_iter().collect();
                for (id, m) in g.members.iter_mut() {
                    m.assignment = given.remove(id).unwrap_or_default();
                }
                g.state = GState::Stable;
                let waiters: Vec<_> = g.sync_waiters.drain().collect();
                for (id, tx) in waiters {
                    let a = g.members.get(&id).map(|m| m.assignment.clone()).unwrap_or_default();
                    let _ = tx.send(SyncResult {
                        error: 0,
                        protocol_type: g.protocol_type.clone(),
                        protocol: g.protocol.clone(),
                        assignment: a,
                    });
                }
                let a = g.members.get(&req.member_id).map(|m| m.assignment.clone()).unwrap_or_default();
                return SyncResult { error: 0, protocol_type: g.protocol_type.clone(), protocol: g.protocol.clone(), assignment: a };
            }
            let (tx, rx) = oneshot::channel();
            g.sync_waiters.insert(req.member_id.clone(), tx);
            let wait = g.members.get(&req.member_id).map(|m| m.rebalance_timeout).unwrap_or(Duration::from_secs(60));
            (rx, wait)
        };
        // a follower waits for the leader's assignment, for at most the rebalance timeout
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => SyncResult::error(err::COORDINATOR_NOT_AVAILABLE),
            Err(_) => {
                let mut inner = self.inner.lock();
                if let Some(g) = inner.groups.get_mut(&key) {
                    g.sync_waiters.remove(&req.member_id);
                }
                SyncResult::error(err::REBALANCE_IN_PROGRESS)
            }
        }
    }

    // ---- heartbeat and leave ------------------------------------------------------------------------------------

    pub fn heartbeat(&self, ns: &str, group: &str, member_id: &str, generation: i32) -> i16 {
        let now = Instant::now();
        let mut inner = self.inner.lock();
        let Some(g) = inner.groups.get_mut(&(ns.to_string(), group.to_string())) else { return err::UNKNOWN_MEMBER_ID };
        expire(g, now, self.initial_delay);
        if !g.members.contains_key(member_id) {
            return err::UNKNOWN_MEMBER_ID;
        }
        if let Some(m) = g.members.get_mut(member_id) {
            m.last_seen = now;
        }
        match g.state {
            GState::Empty => err::UNKNOWN_MEMBER_ID,
            _ if generation != g.generation => err::ILLEGAL_GENERATION,
            GState::PreparingRebalance => err::REBALANCE_IN_PROGRESS,
            GState::CompletingRebalance | GState::Stable => err::NONE,
        }
    }

    /// Remove members. Returns an error code per (member id).
    pub fn leave(&self, ns: &str, group: &str, members: &[String]) -> Vec<i16> {
        let now = Instant::now();
        let mut inner = self.inner.lock();
        let Some(g) = inner.groups.get_mut(&(ns.to_string(), group.to_string())) else {
            return members.iter().map(|_| err::UNKNOWN_MEMBER_ID).collect();
        };
        expire(g, now, self.initial_delay);
        let mut out = Vec::new();
        let mut removed = false;
        for m in members {
            if g.members.remove(m).is_some() {
                g.join_waiters.remove(m);
                g.sync_waiters.remove(m);
                if g.leader.as_deref() == Some(m.as_str()) {
                    g.leader = None;
                }
                removed = true;
                out.push(err::NONE);
            } else {
                out.push(err::UNKNOWN_MEMBER_ID);
            }
        }
        if removed {
            after_removal(g, now, self.initial_delay);
        }
        out
    }

    // ---- offsets ------------------------------------------------------------------------------------------------

    /// Commit offsets. With a generation of -1 and no member id this is a commit by a client that is not a member
    /// (an admin tool or a consumer that assigns partitions itself) and is always accepted.
    pub fn commit(&self, ns: &str, group: &str, generation: i32, member_id: &str, offsets: Vec<(String, i32, Committed)>) -> i16 {
        if !valid_group_id(group) {
            return err::INVALID_GROUP_ID;
        }
        let now = Instant::now();
        let mut inner = self.inner.lock();
        let key = (ns.to_string(), group.to_string());
        if !inner.groups.contains_key(&key) && inner.groups.keys().filter(|(n, _)| *n == key.0).count() >= MAX_GROUPS_PER_NAMESPACE {
            return err::GROUP_MAX_SIZE_REACHED;
        }
        let g = inner.groups.entry(key).or_insert_with(Group::new);
        expire(g, now, self.initial_delay);
        if generation >= 0 || !member_id.is_empty() {
            if !g.members.contains_key(member_id) {
                return err::UNKNOWN_MEMBER_ID;
            }
            if generation != g.generation {
                return err::ILLEGAL_GENERATION;
            }
            if g.state == GState::PreparingRebalance {
                return err::REBALANCE_IN_PROGRESS;
            }
            if let Some(m) = g.members.get_mut(member_id) {
                m.last_seen = now;
            }
        } else if g.state != GState::Empty {
            // Kafka: a commit without a generation is only allowed while the group has no members
            return err::UNKNOWN_MEMBER_ID;
        }
        if g.offsets.len() + offsets.len() > MAX_OFFSETS_PER_GROUP {
            return err::INVALID_COMMIT_OFFSET_SIZE;
        }
        for (t, p, c) in offsets {
            g.offsets.insert((t, p), c);
        }
        crate::model::next_revision();
        err::NONE
    }

    pub fn fetch_offset(&self, ns: &str, group: &str, topic: &str, partition: i32) -> Option<Committed> {
        let inner = self.inner.lock();
        let g = inner.groups.get(&(ns.to_string(), group.to_string()))?;
        g.offsets
            .get(&(topic.to_string(), partition))
            .cloned()
            .or_else(|| g.offsets.iter().find(|((t, p), _)| *p == partition && t.eq_ignore_ascii_case(topic)).map(|(_, c)| c.clone()))
    }

    /// Every committed (topic, partition) of the group.
    pub fn all_offsets(&self, ns: &str, group: &str) -> Vec<(String, i32, Committed)> {
        let inner = self.inner.lock();
        let mut v: Vec<_> = inner
            .groups
            .get(&(ns.to_string(), group.to_string()))
            .map(|g| g.offsets.iter().map(|((t, p), c)| (t.clone(), *p, c.clone())).collect())
            .unwrap_or_default();
        v.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        v
    }

    // ---- saving -------------------------------------------------------------------------------------------------

    /// The committed offsets of every group that has any: (namespace, group, [(topic, partition, offset)]). Members,
    /// generations and rebalances are not kept: after a restart a group is empty and its members join again.
    pub fn export_offsets(&self) -> Vec<GroupOffsets> {
        let inner = self.inner.lock();
        let mut out: Vec<_> = inner
            .groups
            .iter()
            .filter(|(_, g)| !g.offsets.is_empty())
            .map(|((n, id), g)| {
                let mut o: Vec<_> = g.offsets.iter().map(|((t, p), c)| (t.clone(), *p, c.clone())).collect();
                o.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
                (n.clone(), id.clone(), o)
            })
            .collect();
        out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        out
    }

    /// Put committed offsets back (at start, on an empty coordinator).
    pub fn import_offsets(&self, groups: Vec<GroupOffsets>) {
        let mut inner = self.inner.lock();
        for (ns, id, offsets) in groups {
            let g = inner.groups.entry((ns, id)).or_insert_with(Group::new);
            for (t, p, c) in offsets {
                g.offsets.insert((t, p), c);
            }
        }
    }

    // ---- reading ------------------------------------------------------------------------------------------------

    pub fn describe(&self, ns: &str, group: &str) -> Option<GroupInfo> {
        let now = Instant::now();
        let mut inner = self.inner.lock();
        let g = inner.groups.get_mut(&(ns.to_string(), group.to_string()))?;
        expire(g, now, self.initial_delay);
        Some(info(group, g))
    }

    pub fn list(&self, ns: &str) -> Vec<GroupInfo> {
        let now = Instant::now();
        let mut inner = self.inner.lock();
        let mut out = Vec::new();
        for ((n, id), g) in inner.groups.iter_mut() {
            if n == ns {
                expire(g, now, self.initial_delay);
                out.push(info(id, g));
            }
        }
        out.sort_by(|a, b| a.group.cmp(&b.group));
        out
    }

    /// Delete groups that have no members; Kafka's DeleteGroups. Returns false for a group with members.
    pub fn delete_empty(&self, ns: &str, group: &str) -> bool {
        let mut inner = self.inner.lock();
        let key = (ns.to_string(), group.to_string());
        match inner.groups.get(&key) {
            Some(g) if !g.members.is_empty() => false,
            _ => {
                inner.groups.remove(&key);
                crate::model::next_revision();
                true
            }
        }
    }
}

fn info(id: &str, g: &Group) -> GroupInfo {
    GroupInfo {
        group: id.to_string(),
        state: g.state.name(),
        protocol_type: g.protocol_type.clone().unwrap_or_default(),
        protocol: g.protocol.clone().unwrap_or_default(),
        generation: g.generation,
        members: g
            .members
            .iter()
            .map(|(mid, m)| MemberInfo {
                member_id: mid.clone(),
                instance_id: m.instance_id.clone(),
                client_id: m.client_id.clone(),
                client_host: m.client_host.clone(),
                metadata: g
                    .protocol
                    .as_ref()
                    .and_then(|p| m.protocols.iter().find(|(n, _)| n == p))
                    .map(|(_, b)| b.clone())
                    .unwrap_or_default(),
                assignment: m.assignment.clone(),
            })
            .collect(),
    }
}

// ---- the state machine (all called with the lock held) ---------------------------------------------------------

/// The protocols every member (and `joining`, in place of its older entry) supports.
fn candidates(g: &Group, joining: Option<&str>, joining_protocols: &[(String, Bytes)]) -> Vec<String> {
    let mut sets: Vec<Vec<&str>> = g
        .members
        .iter()
        .filter(|(id, _)| Some(id.as_str()) != joining)
        .map(|(_, m)| m.protocols.iter().map(|(n, _)| n.as_str()).collect())
        .collect();
    sets.push(joining_protocols.iter().map(|(n, _)| n.as_str()).collect());
    let first = sets[0].clone();
    first.into_iter().filter(|p| sets.iter().all(|s| s.contains(p))).map(str::to_string).collect()
}

/// Members that did not heartbeat for their session timeout are removed (not those waiting in a join).
fn expire(g: &mut Group, now: Instant, initial_delay: Duration) {
    let dead: Vec<String> = g
        .members
        .iter()
        .filter(|(_, m)| !m.joined && now.saturating_duration_since(m.last_seen) > m.session_timeout)
        .map(|(id, _)| id.clone())
        .collect();
    if dead.is_empty() {
        return;
    }
    for id in dead {
        g.members.remove(&id);
        g.join_waiters.remove(&id);
        g.sync_waiters.remove(&id);
        if g.leader.as_deref() == Some(id.as_str()) {
            g.leader = None;
        }
    }
    after_removal(g, now, initial_delay);
}

fn after_removal(g: &mut Group, now: Instant, initial_delay: Duration) {
    if g.members.is_empty() {
        g.state = GState::Empty;
        g.leader = None;
        g.protocol = None;
        g.protocol_type = None;
        g.deadline = None;
        g.generation += 1;
        return;
    }
    match g.state {
        GState::PreparingRebalance => maybe_complete(g, now),
        _ => {
            prepare_rebalance(g, now, initial_delay);
            maybe_complete(g, now);
        }
    }
}

fn prepare_rebalance(g: &mut Group, now: Instant, initial_delay: Duration) {
    let was_empty = g.state == GState::Empty;
    g.state = GState::PreparingRebalance;
    // members that were not waiting must rejoin; the ones that wait already have joined
    for (id, m) in g.members.iter_mut() {
        m.joined = g.join_waiters.contains_key(id);
    }
    for (_, tx) in g.sync_waiters.drain() {
        let _ = tx.send(SyncResult::error(err::REBALANCE_IN_PROGRESS));
    }
    g.initial = was_empty;
    let wait =
        if was_empty { initial_delay } else { g.members.values().map(|m| m.rebalance_timeout).max().unwrap_or(Duration::from_secs(60)) };
    g.deadline = Some(now + wait);
}

/// Complete the round now when every member has joined (not in the initial wait).
fn maybe_complete(g: &mut Group, now: Instant) {
    if g.state == GState::PreparingRebalance && !g.initial && g.members.values().all(|m| m.joined) {
        complete_join(g, now);
    }
}

fn complete_join(g: &mut Group, now: Instant) {
    // members that did not come back are out
    let gone: Vec<String> = g.members.iter().filter(|(_, m)| !m.joined).map(|(id, _)| id.clone()).collect();
    for id in gone {
        g.members.remove(&id);
        g.sync_waiters.remove(&id);
    }
    g.deadline = None;
    g.initial = false;
    g.generation += 1;
    if g.members.is_empty() {
        g.state = GState::Empty;
        g.leader = None;
        g.protocol = None;
        g.protocol_type = None;
        return;
    }
    // the protocol: each member votes for its first choice among those everyone supports
    let cands = {
        let mut sets: Vec<Vec<&str>> = g.members.values().map(|m| m.protocols.iter().map(|(n, _)| n.as_str()).collect()).collect();
        let first = sets.remove(0);
        let mut c: Vec<String> = first.into_iter().filter(|p| sets.iter().all(|s| s.contains(p))).map(str::to_string).collect();
        if c.is_empty() {
            c = g.members.values().next().and_then(|m| m.protocols.first()).map(|(n, _)| vec![n.clone()]).unwrap_or_default();
        }
        c
    };
    let mut votes: HashMap<&str, usize> = HashMap::new();
    for m in g.members.values() {
        if let Some((n, _)) = m.protocols.iter().find(|(n, _)| cands.contains(n)) {
            *votes.entry(n.as_str()).or_default() += 1;
        }
    }
    let chosen = cands.iter().max_by_key(|c| votes.get(c.as_str()).copied().unwrap_or(0)).cloned().unwrap_or_default();
    g.protocol = Some(chosen.clone());
    if g.leader.as_ref().is_none_or(|l| !g.members.contains_key(l)) {
        g.leader = g.members.keys().next().cloned();
    }
    g.state = GState::CompletingRebalance;
    let leader = g.leader.clone().unwrap_or_default();
    let all: Vec<(String, Option<String>, Bytes)> = g
        .members
        .iter()
        .map(|(id, m)| {
            (id.clone(), m.instance_id.clone(), m.protocols.iter().find(|(n, _)| *n == chosen).map(|(_, b)| b.clone()).unwrap_or_default())
        })
        .collect();
    let waiters: Vec<_> = g.join_waiters.drain().collect();
    for m in g.members.values_mut() {
        m.joined = false;
        m.assignment = Bytes::new();
        m.last_seen = now;
    }
    for (id, tx) in waiters {
        if !g.members.contains_key(&id) {
            continue;
        }
        let _ = tx.send(JoinResult {
            error: 0,
            generation: g.generation,
            protocol_type: g.protocol_type.clone(),
            protocol: Some(chosen.clone()),
            leader: leader.clone(),
            member_id: id.clone(),
            members: if id == leader { all.clone() } else { Vec::new() },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn join_req(group: &str, member: &str, session_ms: i32, rebalance_ms: i32) -> JoinRequest {
        JoinRequest {
            group: group.into(),
            member_id: member.into(),
            instance_id: None,
            client_id: "c".into(),
            client_host: "/127.0.0.1".into(),
            session_timeout_ms: session_ms,
            rebalance_timeout_ms: rebalance_ms,
            protocol_type: "consumer".into(),
            protocols: vec![("range".into(), Bytes::from_static(b"m"))],
            version: 3,
        }
    }

    fn sync_req(group: &str, member: &str, generation: i32, assignments: &[(&str, &str)]) -> SyncRequest {
        SyncRequest {
            group: group.into(),
            member_id: member.into(),
            generation,
            protocol_type: None,
            protocol: None,
            assignments: assignments.iter().map(|(m, a)| (m.to_string(), Bytes::copy_from_slice(a.as_bytes()))).collect(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_round_of_a_new_group_waits_the_initial_delay() {
        let c = Coordinator::default();
        let start = Instant::now();
        let r = c.join("ns", join_req("g", "", 10_000, 10_000)).await;
        assert_eq!((r.error, r.generation), (0, 1));
        assert!(start.elapsed() >= c.initial_delay && start.elapsed() < c.initial_delay + Duration::from_secs(2), "{:?}", start.elapsed());
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_member_that_joins_during_the_initial_delay_is_in_the_first_generation() {
        let c = Arc::new(Coordinator::default());
        let c1 = c.clone();
        let a = tokio::spawn(async move { c1.join("ns", join_req("g", "", 10_000, 10_000)).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let c2 = c.clone();
        let b = tokio::spawn(async move { c2.join("ns", join_req("g", "", 10_000, 10_000)).await });
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        assert_eq!((a.generation, b.generation), (1, 1));
        assert_eq!(a.leader, b.leader);
        let leader = if a.member_id == a.leader { &a } else { &b };
        assert_eq!(leader.members.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_member_that_never_rejoins_is_dropped_at_the_rebalance_timeout() {
        let c = Arc::new(Coordinator::default());
        let a = c.join("ns", join_req("g", "", 30_000, 12_000)).await;
        assert_eq!(c.sync("ns", sync_req("g", &a.member_id, 1, &[(&a.member_id, "x")])).await.error, 0);
        // B joins; A (silent) does not rejoin
        let c2 = c.clone();
        let start = Instant::now();
        let b = tokio::spawn(async move { c2.join("ns", join_req("g", "", 30_000, 5_000)).await });
        let b = b.await.unwrap();
        assert_eq!((b.error, b.generation, b.members.len()), (0, 2, 1));
        // the round waits for the longest rebalance timeout of the group (12 s) at most, then A is out
        assert!(start.elapsed() <= Duration::from_secs(13), "{:?}", start.elapsed());
        assert_eq!(c.describe("ns", "g").unwrap().members.len(), 1);
        assert_eq!(c.heartbeat("ns", "g", &a.member_id, 1), err::UNKNOWN_MEMBER_ID);
    }

    #[tokio::test(start_paused = true)]
    async fn a_member_that_stops_heartbeating_is_dropped_after_its_session_timeout_and_the_rest_rebalance() {
        let c = Arc::new(Coordinator::default());
        let c1 = c.clone();
        let c2 = c.clone();
        let (a, b) = tokio::join!(async move { c1.join("ns", join_req("g", "", 6_000, 10_000)).await }, async move {
            c2.join("ns", join_req("g", "", 6_000, 10_000)).await
        });
        let (a, b) = if a.member_id == a.leader { (a, b) } else { (b, a) }; // a is the leader
        let leader_sync = c.sync("ns", sync_req("g", &a.member_id, 1, &[(&a.member_id, "A"), (&b.member_id, "B")])).await;
        assert_eq!(leader_sync.error, 0);
        assert_eq!(c.sync("ns", sync_req("g", &b.member_id, 1, &[])).await.assignment.as_ref(), b"B");
        // only b heartbeats
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            c.heartbeat("ns", "g", &b.member_id, 1);
        }
        assert_eq!(c.heartbeat("ns", "g", &b.member_id, 1), err::REBALANCE_IN_PROGRESS);
        let info = c.describe("ns", "g").unwrap();
        assert_eq!((info.state, info.members.len()), ("PreparingRebalance", 1));
        // b rejoins and is alone in generation 2
        let r = c.join("ns", join_req("g", &b.member_id, 6_000, 10_000)).await;
        assert_eq!((r.error, r.generation, r.members.len()), (0, 2, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_follower_waiting_for_the_leader_is_answered_with_rebalance_in_progress_at_the_timeout() {
        let c = Arc::new(Coordinator::default());
        let c1 = c.clone();
        let c2 = c.clone();
        let (a, b) = tokio::join!(async move { c1.join("ns", join_req("g", "", 30_000, 4_000)).await }, async move {
            c2.join("ns", join_req("g", "", 30_000, 4_000)).await
        });
        let follower = if a.member_id == a.leader { &b } else { &a };
        let start = Instant::now();
        let r = c.sync("ns", sync_req("g", &follower.member_id, 1, &[])).await;
        assert_eq!(r.error, err::REBALANCE_IN_PROGRESS);
        assert!(start.elapsed() >= Duration::from_secs(4));
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_join_wakes_a_waiting_follower_with_rebalance_in_progress() {
        let c = Arc::new(Coordinator::default());
        let c1 = c.clone();
        let c2 = c.clone();
        let (a, b) = tokio::join!(async move { c1.join("ns", join_req("g", "", 30_000, 10_000)).await }, async move {
            c2.join("ns", join_req("g", "", 30_000, 10_000)).await
        });
        let follower = if a.member_id == a.leader { b } else { a };
        let cf = c.clone();
        let fid = follower.member_id.clone();
        let waiting = tokio::spawn(async move { cf.sync("ns", sync_req("g", &fid, 1, &[])).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let c3 = c.clone();
        tokio::spawn(async move { c3.join("ns", join_req("g", "", 30_000, 10_000)).await });
        let r = waiting.await.unwrap();
        assert_eq!(r.error, err::REBALANCE_IN_PROGRESS);
    }

    #[test]
    fn commits_without_a_member_are_refused_while_the_group_has_members() {
        let c = Coordinator::default();
        let off = |o| vec![("t".to_string(), 0, Committed { offset: o, metadata: None, leader_epoch: -1 })];
        assert_eq!(c.commit("ns", "g", -1, "", off(1)), err::NONE);
        assert_eq!(c.fetch_offset("ns", "g", "t", 0).unwrap().offset, 1);
        assert_eq!(c.fetch_offset("ns", "g", "T", 0).unwrap().offset, 1, "topic names are compared without case, as event hub names are");
        assert_eq!(c.commit("ns", &"g".repeat(300), -1, "", off(1)), err::INVALID_GROUP_ID);
        // namespaces are separate
        assert!(c.fetch_offset("other", "g", "t", 0).is_none());
    }
}
