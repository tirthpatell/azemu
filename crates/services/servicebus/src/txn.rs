//! AMQP 1.0 transactions as Service Bus does them (AMQP 1.0 part 4; Microsoft Learn, "Overview of
//! Service Bus transaction processing" and "AMQP 1.0 in Azure Service Bus and Event Hubs protocol guide").
//!
//! The client (the transaction controller) attaches a sender link to the `amqp:coordinator` target and
//! sends `declare` (answered with `declared{txn-id}`) and `discharge{txn-id, fail}` (answered with
//! `accepted`, or `rejected` with a `amqp:transaction:*` error). Transactional work carries the
//! delivery state `transactional-state{txn-id}`: a transfer (a send), a disposition (complete, abandon,
//! defer, dead-letter) and the requests to `<entity>/$management` that change something (schedule,
//! cancel, update-disposition, set-session-state).
//!
//! How it is built: nothing is changed while the transaction is open. Every operation is checked when
//! it is made (the entity exists and takes the message, the lock is valid) and remembered. A commit
//! checks all of them again and then applies them in order, all or none, under one hold of the state
//! lock, through the same code a plain send or settlement uses (so duplicate detection, sessions,
//! scheduling, forwarding, dead-lettering and topic rules behave the same). A rollback, a time out, or
//! the end of the controller link or the connection just forgets them: the messages that were completed
//! in the transaction were never touched and stay locked by their receiver until the lock runs out.
//!
//! Scope: a transaction belongs to the connection that declared it and works on any link of it. It may
//! touch several entities (the Java SDK does that with `enableCrossEntityTransactions` and no via link;
//! a sender attached with `com.microsoft:transfer-destination-address` is accepted too and sends to the
//! destination). A transaction times out after 2 minutes (`AZEMU_SERVICEBUS_TXN_TIMEOUT_SECONDS`) and has
//! at most 100 operations (messages sent and messages settled count one each).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use azemu_amqp::api::{IncomingDelivery, LinkId, TransferAction};
use azemu_amqp::message::{Body, Message};
use azemu_amqp::terminus::{DeliveryState, TxnControl};
use azemu_amqp::{condition, Error as AmqpError};

use crate::amqp_conn::*;
use crate::core::*;
use crate::errors::SbError;
use crate::mgmt::{op_cancel_scheduled, Target};
use crate::model::*;
use crate::state::*;
use crate::store::*;

/// Operations in one transaction (Azure: 100).
pub const MAX_TXN_OPS: usize = 100;
/// Transactions a connection may have open (not discharged) at the same time.
pub const MAX_OPEN_TXNS: usize = 1000;
/// Bytes counted for every open transaction before it holds any work.
const TXN_FIXED_BYTES: usize = 1024;
/// How long the record of a transaction that timed out is kept, so that a late discharge is answered
/// with the time out error and not with "unknown id".
const TOMBSTONE_KEEP: Duration = Duration::from_secs(600);

/// One remembered operation of a transaction.
pub(crate) enum TxOp {
    /// Messages sent to a queue or topic.
    Send { target: Address, incoming: Vec<Incoming> },
    /// Scheduled sends: the sequence numbers were reserved and answered when the request was made.
    Schedule { target: Address, items: Vec<(Incoming, u64)> },
    /// A settlement of a locked message.
    Settle { addr: Address, token: Token, outcome: Outcome },
    /// `cancel-scheduled-message`.
    Cancel { target: Target, seqs: Vec<i64> },
    /// `set-session-state`.
    SessionState { addr: Address, session: String, state: Option<Vec<u8>> },
}

struct Txn {
    controller: LinkId,
    deadline: Instant,
    ops: Vec<TxOp>,
    weight: usize,
    /// Bytes this transaction holds (also counted in the budget of the service).
    bytes: usize,
    timed_out: bool,
    /// The entity the transaction works on and the AMQP session (channel) of the link that first used it.
    bound: Option<(String, u16)>,
    budget: Arc<TxnBudget>,
}

impl Txn {
    /// Let go of the operations and of the bytes they held (a time out; or, through `Drop`, any end).
    fn release(&mut self) {
        self.ops.clear();
        self.ops.shrink_to_fit();
        self.budget.release(self.bytes);
        self.bytes = 0;
    }
}

impl Drop for Txn {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

/// How many bytes of work transactions may hold before they commit: the messages they carry and the
/// settlements they remember. Operations are limited to 100 per transaction (Azure's rule), and one
/// message to what its entity allows (256 KB by default, up to 100 MB), so the largest transaction that
/// real code makes is far below the first limit; the others stop one client from using all the memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxnLimits {
    /// Per transaction.
    pub per_txn: usize,
    /// All open transactions of one connection.
    pub per_connection: usize,
    /// All open transactions of the service.
    pub total: usize,
}

impl Default for TxnLimits {
    fn default() -> Self {
        TxnLimits { per_txn: 128 << 20, per_connection: 256 << 20, total: 1 << 30 }
    }
}

/// What every open transaction of the service holds, against the limits.
#[derive(Debug, Default)]
pub struct TxnBudget {
    /// The limits and the bytes held by all open transactions.
    state: Mutex<(TxnLimits, usize)>,
}

impl TxnBudget {
    pub fn new(limits: TxnLimits) -> Arc<TxnBudget> {
        Arc::new(TxnBudget { state: Mutex::new((limits, 0)) })
    }
    pub fn limits(&self) -> TxnLimits {
        self.state.lock().0
    }
    pub fn set_limits(&self, l: TxnLimits) {
        self.state.lock().0 = l;
    }
    /// Bytes held by all open transactions.
    pub fn used(&self) -> usize {
        self.state.lock().1
    }
    fn reserve(&self, n: usize) -> bool {
        let mut st = self.state.lock();
        match st.1.checked_add(n) {
            Some(v) if v <= st.0.total => {
                st.1 = v;
                true
            }
            _ => false,
        }
    }
    fn release(&self, n: usize) {
        if n > 0 {
            let mut st = self.state.lock();
            st.1 = st.1.saturating_sub(n);
        }
    }
}

/// The transactions of one connection.
pub(crate) struct TxnTable {
    inner: Arc<Mutex<TxnInner>>,
    budget: Arc<TxnBudget>,
    sweeper_started: bool,
    /// Who makes the operation that comes next (set by the connection before it adds one).
    ctx: OpContext,
}

/// The link an operation of a transaction comes over.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct OpContext {
    /// The AMQP session (channel).
    pub session: u16,
    /// A send-via link: it names its own destination and is outside the one entity rule.
    pub via: bool,
}

#[derive(Default)]
struct TxnInner {
    map: std::collections::HashMap<Vec<u8>, Txn>,
}

impl Default for TxnTable {
    fn default() -> Self {
        TxnTable::new(TxnBudget::new(TxnLimits::default()))
    }
}

fn txn_error(condition: &'static str, status: i32, description: impl Into<String>) -> SbError {
    SbError::new(condition, status, description)
}

fn unknown_id() -> SbError {
    txn_error(condition::TXN_UNKNOWN_ID, 404, "The transaction was not found: it was already committed or rolled back, it belongs to another connection, or its controller link was closed.")
}

fn timed_out() -> SbError {
    txn_error(condition::TXN_TIMEOUT, 408, "The transaction timed out (a transaction lasts 2 minutes at most) and was rolled back.")
}

fn rolled_back(cause: &SbError) -> SbError {
    txn_error(condition::TXN_ROLLBACK, 400, format!("The transaction was rolled back, nothing was applied: {}", cause.description))
}

fn second_entity(first: &str, second: &str) -> SbError {
    txn_error(
        condition::NOT_ALLOWED,
        400,
        format!(
            "The transaction already works on the entity '{first}' and cannot also work on '{second}': a transaction is limited to one entity unless the client is set up for cross-entity transactions (all its links on one connection and session, or sending via a shared entity)."
        ),
    )
}

fn too_big(what: &str, limit: usize) -> SbError {
    txn_error(
        condition::RESOURCE_LIMIT_EXCEEDED,
        403,
        format!("The work held by {what} has reached the limit of {limit} bytes (messages and settlements waiting for the commit). Commit or roll back the transaction first."),
    )
}

impl TxnInner {
    /// Forget what timed out a long time ago and drop the work of what timed out lately.
    fn sweep(&mut self, now: Instant) {
        self.map.retain(|_, t| !(t.timed_out && now > t.deadline + TOMBSTONE_KEEP));
        for t in self.map.values_mut() {
            if now >= t.deadline && !t.timed_out {
                t.timed_out = true;
                t.release();
            }
        }
    }

    /// An open transaction, or the error that says why there is none.
    fn open(&mut self, id: &[u8], now: Instant) -> Result<&mut Txn, SbError> {
        let t = self.map.get_mut(id).ok_or_else(unknown_id)?;
        if !t.timed_out && now >= t.deadline {
            t.timed_out = true;
            t.release();
        }
        if t.timed_out {
            return Err(timed_out());
        }
        Ok(t)
    }
}

impl TxnTable {
    pub fn new(budget: Arc<TxnBudget>) -> TxnTable {
        TxnTable { inner: Arc::new(Mutex::new(TxnInner::default())), budget, sweeper_started: false, ctx: OpContext::default() }
    }

    /// A task that lets go of the work of transactions that timed out although nobody touches them
    /// again, for as long as the connection (this table) lives.
    fn start_sweeper(&mut self, timeout: Duration) {
        if self.sweeper_started {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        self.sweeper_started = true;
        let weak = Arc::downgrade(&self.inner);
        let every = (timeout / 4).clamp(Duration::from_millis(50), Duration::from_secs(5));
        handle.spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(inner) = weak.upgrade() else { return };
                inner.lock().sweep(Instant::now());
            }
        });
    }

    fn declare(&mut self, controller: LinkId, timeout: Duration, now: Instant) -> Result<Vec<u8>, SbError> {
        self.start_sweeper(timeout);
        let mut inner = self.inner.lock();
        inner.sweep(now);
        if inner.map.len() >= MAX_OPEN_TXNS {
            return Err(SbError::new(
                condition::RESOURCE_LIMIT_EXCEEDED,
                403,
                format!("The limit of {MAX_OPEN_TXNS} open transactions per connection has been reached."),
            ));
        }
        // an open transaction costs something even with no work in it (so that empty ones are limited too)
        if !self.budget.reserve(TXN_FIXED_BYTES) {
            return Err(too_big("all open transactions of the service", self.budget.limits().total));
        }
        let id = uuid::Uuid::new_v4().as_bytes().to_vec();
        inner.map.insert(
            id.clone(),
            Txn {
                controller,
                deadline: now + timeout,
                ops: Vec::new(),
                weight: 0,
                bytes: TXN_FIXED_BYTES,
                timed_out: false,
                bound: None,
                budget: self.budget.clone(),
            },
        );
        Ok(id)
    }

    /// Remember an operation (`weight` counts toward the limit of 100 operations, its bytes toward the
    /// byte limits of the transaction, the connection and the service).
    pub fn add(&mut self, id: &[u8], op: TxOp, weight: usize, now: Instant) -> Result<(), SbError> {
        let bytes = op.bytes();
        let (entity, ctx) = (op.entity(), self.ctx);
        let limits = self.budget.limits();
        let mut inner = self.inner.lock();
        let held_by_connection: usize = inner.map.values().map(|t| t.bytes).sum();
        let t = inner.open(id, now)?;
        if t.weight + weight > MAX_TXN_OPS {
            return Err(SbError::new(
                condition::RESOURCE_LIMIT_EXCEEDED,
                403,
                format!("The transaction has more than the {MAX_TXN_OPS} operations Service Bus allows in one transaction."),
            ));
        }
        // One entity per transaction, unless the client puts the work of several entities on one AMQP session
        // (what the .NET SDK does with `EnableCrossEntityTransactions`; see `docs/services/servicebus.md`)
        // or sends via a shared entity.
        if !ctx.via {
            match &t.bound {
                None => t.bound = Some((entity, ctx.session)),
                Some((bound, session)) if *bound == entity || *session == ctx.session => {}
                Some((bound, _)) => return Err(second_entity(bound, &entity)),
            }
        }
        if t.bytes.saturating_add(bytes) > limits.per_txn {
            return Err(too_big("this transaction", limits.per_txn));
        }
        if held_by_connection.saturating_add(bytes) > limits.per_connection {
            return Err(too_big("the transactions of this connection", limits.per_connection));
        }
        if !self.budget.reserve(bytes) {
            return Err(too_big("all open transactions of the service", limits.total));
        }
        t.weight += weight;
        t.bytes += bytes;
        t.ops.push(op);
        Ok(())
    }

    /// Tell the table which link the next operation comes over.
    pub fn set_context(&mut self, ctx: OpContext) {
        self.ctx = ctx;
    }

    /// Check the transaction is open (a transactional request that adds nothing yet).
    pub fn check_open(&mut self, id: &[u8], now: Instant) -> Result<(), SbError> {
        self.inner.lock().open(id, now).map(|_| ())
    }

    /// End a transaction and hand over its work. `Err` when there is none to commit or roll back.
    fn take(&mut self, id: &[u8], now: Instant) -> Result<Vec<TxOp>, SbError> {
        let mut inner = self.inner.lock();
        match inner.open(id, now) {
            Ok(_) => Ok(inner.map.remove(id).map(|mut t| std::mem::take(&mut t.ops)).unwrap_or_default()),
            Err(e) => {
                // a transaction that timed out is told so once
                if e.condition == condition::TXN_TIMEOUT {
                    inner.map.remove(id);
                }
                Err(e)
            }
        }
    }

    /// The controller link ended: its open transactions are rolled back.
    pub fn drop_controller(&mut self, link: LinkId) {
        self.inner.lock().map.retain(|_, t| t.controller != link);
    }
}

impl TxOp {
    /// The entity the operation works on.
    fn entity(&self) -> String {
        use crate::mgmt::path_of;
        match self {
            TxOp::Send { target, .. } | TxOp::Schedule { target, .. } => path_of(target).to_lowercase(),
            TxOp::Settle { addr, .. } | TxOp::SessionState { addr, .. } => path_of(addr).to_lowercase(),
            TxOp::Cancel { target, .. } => match target {
                Target::Topic(name) => name.to_lowercase(),
                Target::Core(a) => path_of(a).to_lowercase(),
            },
        }
    }

    /// What the operation holds in memory, roughly: the messages (their encoded size and a fixed cost
    /// for the decoded form), the changes asked for in a settlement, session state.
    fn bytes(&self) -> usize {
        const FIXED: usize = 512;
        fn pairs(p: &Option<Vec<(azemu_amqp::types::Value, azemu_amqp::types::Value)>>) -> usize {
            p.as_ref()
                .map(|p| p.iter().map(|(k, v)| azemu_amqp::codec::encode_to_vec(k).len() + azemu_amqp::codec::encode_to_vec(v).len()).sum())
                .unwrap_or(0)
        }
        match self {
            TxOp::Send { incoming, .. } => incoming.iter().map(|i| i.size + FIXED).sum(),
            TxOp::Schedule { items, .. } => items.iter().map(|(i, _)| i.size + FIXED).sum(),
            TxOp::Settle { outcome, .. } => {
                FIXED
                    + match outcome {
                        Outcome::Complete | Outcome::Release => 0,
                        Outcome::Abandon { modify } | Outcome::Defer { modify } => pairs(modify),
                        Outcome::DeadLetter { reason, description, modify } => {
                            reason.as_ref().map_or(0, |r| r.len()) + description.as_ref().map_or(0, |d| d.len()) + pairs(modify)
                        }
                    }
            }
            TxOp::Cancel { seqs, .. } => FIXED + seqs.len() * 8,
            TxOp::SessionState { session, state, .. } => FIXED + session.len() + state.as_ref().map_or(0, |s| s.len()),
        }
    }
}

/// The transaction id of a `transactional-state`.
pub(crate) fn txn_id_of(state: &Option<DeliveryState>) -> Option<Vec<u8>> {
    match state {
        Some(DeliveryState::Transactional { txn_id, .. }) => Some(txn_id.clone()),
        _ => None,
    }
}

/// The answer to transactional work: the outcome wrapped in the transactional state.
pub(crate) fn in_txn(txn: &Option<Vec<u8>>, outcome: DeliveryState) -> DeliveryState {
    match txn {
        Some(id) => DeliveryState::Transactional { txn_id: id.clone(), outcome: Some(Box::new(outcome)) },
        None => outcome,
    }
}

/// Can the entity take this message at all? `with_room` also asks whether the entity has room for it now
/// (a send asks; a commit does not, because room depends on the order of the operations, see `commit`).
pub(crate) fn target_exists(st: &mut State, ns: &str, target: &Address, inc: &Incoming, with_room: bool) -> Result<(), SbError> {
    let nsr = st.ns(ns);
    match target {
        Address::Topic { name } => nsr.topic(name).ok_or_else(|| SbError::entity_not_found(name, ns))?.check_send(inc.size),
        other => {
            let core = nsr.core(other).ok_or_else(|| SbError::entity_not_found(&crate::mgmt::path_of(other), ns))?;
            if with_room {
                core.validate_send(inc)
            } else {
                core.validate_send_static(inc)
            }
        }
    }
}

impl SbConn {
    pub(crate) fn txn_timeout(&self) -> Duration {
        self.shared.txn_timeout()
    }

    /// A message arrived on the coordinator link: `declare` or `discharge`.
    pub(crate) fn on_coordinator_transfer(&mut self, link: LinkId, d: &IncomingDelivery) -> TransferAction {
        let reject = |e: SbError| TransferAction::Settle(DeliveryState::rejected(e.to_amqp()));
        let msg = match Message::decode(&d.payload) {
            Ok(m) => m,
            Err(e) => return TransferAction::Settle(DeliveryState::rejected(AmqpError::new(condition::DECODE_ERROR, e.to_string()))),
        };
        let control = match &msg.body {
            Body::Value(v) => TxnControl::from_value(v).ok(),
            _ => None,
        };
        let Some(control) = control else {
            return TransferAction::Settle(DeliveryState::rejected(AmqpError::new(
                condition::NOT_ALLOWED,
                "The coordinator takes only declare and discharge messages.",
            )));
        };
        let now = Instant::now();
        match control {
            TxnControl::Declare { .. } => match self.txns.declare(link, self.txn_timeout(), now) {
                Ok(id) => TransferAction::Settle(DeliveryState::Declared { txn_id: id }),
                Err(e) => reject(e),
            },
            TxnControl::Discharge { txn_id, fail } => {
                let ops = match self.txns.take(&txn_id, now) {
                    Ok(ops) => ops,
                    Err(e) => return reject(e),
                };
                if fail {
                    return TransferAction::Settle(DeliveryState::Accepted);
                }
                match self.commit(ops) {
                    Ok(()) => TransferAction::Settle(DeliveryState::Accepted),
                    Err(e) => reject(e),
                }
            }
        }
    }

    /// Make the operations of a transaction happen: all of them, or none.
    fn commit(&mut self, ops: Vec<TxOp>) -> Result<(), SbError> {
        if ops.is_empty() {
            return Ok(());
        }
        let ns = self.ns.clone();
        let now = now_ms();
        let shared = self.shared.clone();
        let mut st = shared.state.lock();
        // what is due by now (locks that ran out, messages that expired) happens first and for good: it
        // is the clock's doing, not the transaction's, and it must not be taken back with it
        st.tick_due(now);
        // 1. check everything
        let mut settled: HashSet<(String, Token)> = HashSet::new();
        for op in &ops {
            let checked = match op {
                TxOp::Send { target, incoming } => incoming.iter().try_for_each(|i| target_exists(&mut st, &ns, target, i, false)),
                TxOp::Schedule { target, items } => items.iter().try_for_each(|(i, _)| target_exists(&mut st, &ns, target, i, false)),
                TxOp::Settle { addr, token, .. } => {
                    let path = crate::mgmt::path_of(addr);
                    match st.ns(&ns).core_mut(addr) {
                        None => Err(SbError::entity_not_found(&path, &ns)),
                        Some(core) => core.check_lock(token, now).and_then(|_| {
                            if settled.insert((path, *token)) {
                                Ok(())
                            } else {
                                Err(SbError::message_lock_lost())
                            }
                        }),
                    }
                }
                TxOp::Cancel { target, seqs } => {
                    let mut found = |seq: i64| -> bool {
                        let nsr = st.ns(&ns);
                        match target {
                            Target::Topic(name) => nsr.topic(name).map(|t| t.scheduled.get(seq as u64).is_some()).unwrap_or(false),
                            Target::Core(a) => {
                                nsr.core(a).and_then(|c| c.main.get(seq as u64)).map(|m| m.state == MsgState::Scheduled).unwrap_or(false)
                            }
                        }
                    };
                    match seqs.iter().find(|s| !found(**s)).copied() {
                        Some(s) => Err(SbError::message_not_found(s)),
                        None => Ok(()),
                    }
                }
                TxOp::SessionState { addr, session, .. } => match st.ns(&ns).core(addr) {
                    None => Err(SbError::entity_not_found(&crate::mgmt::path_of(addr), &ns)),
                    Some(core) if core.session_locked(session, now) => Ok(()),
                    Some(_) => Err(SbError::session_lock_lost()),
                },
            };
            checked.map_err(|e| rolled_back(&e))?;
        }
        // 2. apply them in order, remembering every change. What cannot be checked beforehand (room in
        //    an entity after the earlier operations of this transaction, a duplicate id, a forwarding
        //    chain) is found out here, and then everything is taken back.
        st.ns(&ns).begin_journal();
        let mut forwards = Vec::new();
        let mut failure: Option<SbError> = None;
        for op in ops {
            let applied: Result<(), SbError> = match op {
                TxOp::Send { target, incoming } => incoming.into_iter().try_for_each(|i| st.send(&ns, &target, i, now).map(|_| ())),
                TxOp::Schedule { target, items } => {
                    items.into_iter().try_for_each(|(i, seq)| st.send_reserved(&ns, &target, i, now, Some(seq)).map(|_| ()))
                }
                TxOp::Settle { addr, token, outcome } => match st.ns(&ns).core_mut(&addr) {
                    Some(core) => core.settle(&token, outcome, now).map(|f| forwards.extend(f)),
                    None => Err(SbError::entity_not_found(&crate::mgmt::path_of(&addr), &ns)),
                },
                TxOp::Cancel { target, seqs } => op_cancel_scheduled(&mut st, &ns, &target, &seqs),
                TxOp::SessionState { addr, session, state } => match st.ns(&ns).core_mut(&addr) {
                    Some(core) => core.set_session_state(&session, state, now),
                    None => Err(SbError::entity_not_found(&crate::mgmt::path_of(&addr), &ns)),
                },
            };
            if let Err(e) = applied {
                failure = Some(e);
                break;
            }
        }
        if let Some(e) = failure {
            st.ns(&ns).end_journal(true);
            return Err(rolled_back(&e));
        }
        // forwarding what the settlements dead-lettered cannot fail the commit (as with a plain settlement)
        st.forward_all(&ns, forwards, now);
        st.ns(&ns).end_journal(false);
        Ok(())
    }

    /// A send made inside a transaction: check it and remember it.
    pub(crate) fn txn_send(&mut self, txn: &[u8], target: &Address, messages: Vec<(Message, usize)>) -> Result<(), SbError> {
        let now = Instant::now();
        self.txns.check_open(txn, now)?;
        let ns = self.ns.clone();
        let incoming: Vec<Incoming> = messages.into_iter().map(|(m, size)| Incoming::new(m, size)).collect();
        {
            let mut st = self.shared.state.lock();
            for i in &incoming {
                target_exists(&mut st, &ns, target, i, true)?;
            }
        }
        let weight = incoming.len();
        self.txns.add(txn, TxOp::Send { target: target.clone(), incoming }, weight, now)
    }

    /// A settlement made inside a transaction (a disposition with a `transactional-state`).
    pub(crate) fn txn_settle(&mut self, txn: &[u8], addr: &Address, token: Token, outcome: Outcome) -> Result<(), SbError> {
        let now = Instant::now();
        self.txns.check_open(txn, now)?;
        let ns = self.ns.clone();
        {
            let mut st = self.shared.state.lock();
            let core = st.ns(&ns).core_mut(addr).ok_or_else(|| SbError::entity_not_found(&crate::mgmt::path_of(addr), &ns))?;
            core.check_lock(&token, now_ms())?;
        }
        self.txns.add(txn, TxOp::Settle { addr: addr.clone(), token, outcome }, 1, now)
    }
}
