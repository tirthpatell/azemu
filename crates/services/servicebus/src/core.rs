//! The message engine of one queue or one subscription: sends, receives, locks, settlement,
//! expiry, dead-lettering, sessions, duplicate detection. Pure state: no I/O, the caller passes
//! `now` (milliseconds since the Unix epoch) and every transition that depends on time is applied
//! lazily from [`Core::tick`] (no per-message timers).

use std::collections::HashMap;
use std::sync::Arc;

use azemu_amqp::api::LinkHandle;
use azemu_amqp::message::Message;
use azemu_amqp::types::Value;

use crate::errors::SbError;
use crate::model::{Settings, SubQueue};
use crate::store::*;

pub const ANN_ENQUEUED_TIME: &str = "x-opt-enqueued-time";
pub const ANN_SEQUENCE_NUMBER: &str = "x-opt-sequence-number";
pub const ANN_LOCKED_UNTIL: &str = "x-opt-locked-until";
pub const ANN_LOCK_TOKEN: &str = "x-opt-lock-token";
pub const ANN_ENQUEUE_SEQUENCE_NUMBER: &str = "x-opt-enqueue-sequence-number";
pub const ANN_SCHEDULED_ENQUEUE_TIME: &str = "x-opt-scheduled-enqueue-time";
pub const ANN_PARTITION_KEY: &str = "x-opt-partition-key";
pub const ANN_VIA_PARTITION_KEY: &str = "x-opt-via-partition-key";
pub const ANN_DEAD_LETTER_SOURCE: &str = "x-opt-deadletter-source";
pub const ANN_MESSAGE_STATE: &str = "x-opt-message-state";
pub const PROP_DEAD_LETTER_REASON: &str = "DeadLetterReason";
pub const PROP_DEAD_LETTER_DESCRIPTION: &str = "DeadLetterErrorDescription";

/// .NET `DateTime.MaxValue` as a Unix timestamp in milliseconds (what Service Bus uses for
/// "never expires").
pub const MAX_EXPIRY_MS: i64 = 253_402_243_199_000;

/// A message on its way into an entity.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub message: Message,
    pub scheduled_ms: Option<i64>,
    pub size: usize,
    /// Set when the message was forwarded: the sequence number of its first entity.
    pub enq_seq: Option<u64>,
    pub dead_letter_source: Option<String>,
}

impl Incoming {
    /// Build from a decoded message; `size` is the encoded size. Reads the scheduled enqueue time.
    pub fn new(message: Message, size: usize) -> Incoming {
        let scheduled_ms = message.annotation(ANN_SCHEDULED_ENQUEUE_TIME).and_then(|v| v.as_i64());
        Incoming { message, scheduled_ms, size, enq_seq: None, dead_letter_source: None }
    }
}

/// A message to put in another entity (auto-forwarding).
#[derive(Debug, Clone)]
pub struct Forward {
    pub target: String,
    pub incoming: Incoming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveMode {
    PeekLock,
    ReceiveAndDelete,
}

#[derive(Debug, Clone)]
pub struct Delivered {
    pub msg: Msg,
    pub token: Option<Token>,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Complete,
    Abandon {
        modify: Option<Vec<(Value, Value)>>,
    },
    Defer {
        modify: Option<Vec<(Value, Value)>>,
    },
    DeadLetter {
        reason: Option<String>,
        description: Option<String>,
        modify: Option<Vec<(Value, Value)>>,
    },
    /// Give the message back without counting a delivery attempt.
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    Stored(u64),
    /// Dropped by duplicate detection (accepted silently, as Service Bus does).
    Duplicate,
}

pub struct SessionLock {
    pub owner: Owner,
    pub until_ms: i64,
}

#[derive(Default)]
pub struct SessionInfo {
    pub lock: Option<SessionLock>,
    pub state: Option<Vec<u8>>,
    pub last_updated_ms: i64,
}

/// Azure's limit for the state of one session (Standard tier).
pub const MAX_SESSION_STATE_BYTES: usize = 256 * 1024;

/// Duplicate-detection ids kept before expired ones are purged.
const DUP_PURGE_MIN: usize = 50_000;

struct Waiter {
    handle: LinkHandle,
    sub: SubQueue,
}

pub struct Core {
    /// Entity path: `queue` or `topic/Subscriptions/sub`.
    pub path: String,
    pub settings: Settings,
    pub main: Store,
    pub dlq: Store,
    pub tdlq: Store,
    pub sessions: HashMap<String, SessionInfo>,
    pub(crate) dup: HashMap<String, i64>,
    /// Size of `dup` at which expired ids are dropped next.
    dup_purge_at: usize,
    waiters: HashMap<(u64, u64), Waiter>,
    /// Receivers waiting for a session to accept (null session filter).
    session_waiters: HashMap<(u64, u64), LinkHandle>,
    /// Created on the fly by an attach (auto-creation): a session receiver may still turn it
    /// into a session-enabled entity.
    pub autocreated: bool,
    /// Messages to forward to other entities that an operation found while it applied what was
    /// due (a lock or time to live that ran out). Those operations cannot forward them, so they
    /// wait here until the next [`Core::tick`] from the background ticker returns them.
    pub(crate) stash: Vec<Forward>,
    /// While a transaction commits: what to put back if it has to be taken back.
    journal: Option<CoreJournal>,
    /// Changes with every change to what is saved besides the stores: session state, duplicate ids, the stash.
    pub extra_rev: u64,
}

/// A session record as it was: its state and when it was last updated.
type SessionBefore = (Option<Vec<u8>>, i64);

/// What a transaction commit changed in a [`Core`] besides the messages (those are in the stores).
#[derive(Default)]
struct CoreJournal {
    /// Duplicate detection ids added or replaced, with the expiry they had before.
    dup: Vec<(String, Option<i64>)>,
    /// Sessions touched, as they were before (None: the session had no record).
    sessions: HashMap<String, Option<SessionBefore>>,
    stash_len: usize,
}

pub struct Counts {
    pub active: usize,
    pub dead_letter: usize,
    pub scheduled: usize,
    pub transfer: usize,
    pub transfer_dead_letter: usize,
}

impl Core {
    pub fn new(path: impl Into<String>, settings: Settings) -> Core {
        Core {
            path: path.into(),
            settings,
            main: Store::new(),
            dlq: Store::new(),
            tdlq: Store::new(),
            sessions: HashMap::new(),
            dup: HashMap::new(),
            dup_purge_at: DUP_PURGE_MIN,
            waiters: HashMap::new(),
            session_waiters: HashMap::new(),
            autocreated: false,
            stash: Vec::new(),
            journal: None,
            extra_rev: next_revision(),
        }
    }

    /// Start remembering every change (messages, duplicate ids, session state), for a transaction
    /// that applies several operations as one.
    pub fn begin_journal(&mut self) {
        self.main.begin_journal();
        self.dlq.begin_journal();
        self.tdlq.begin_journal();
        self.journal = Some(CoreJournal { stash_len: self.stash.len(), ..Default::default() });
    }

    /// Stop remembering; with `undo`, put everything back as it was at [`Core::begin_journal`].
    pub fn end_journal(&mut self, undo: bool) {
        self.main.end_journal(undo);
        self.dlq.end_journal(undo);
        self.tdlq.end_journal(undo);
        let Some(j) = self.journal.take() else { return };
        if !undo {
            return;
        }
        for (id, before) in j.dup.into_iter().rev() {
            match before {
                Some(exp) => {
                    self.dup.insert(id, exp);
                }
                None => {
                    self.dup.remove(&id);
                }
            }
        }
        for (sid, before) in j.sessions {
            match before {
                None => {
                    // a record this transaction made: it held no lock before
                    if self.sessions.get(&sid).map(|s| s.lock.is_none()).unwrap_or(false) {
                        self.sessions.remove(&sid);
                    }
                }
                Some((state, updated)) => {
                    if let Some(s) = self.sessions.get_mut(&sid) {
                        s.state = state;
                        s.last_updated_ms = updated;
                    }
                }
            }
        }
        self.stash.truncate(j.stash_len);
        self.extra_rev = next_revision();
    }

    pub fn store(&self, sq: SubQueue) -> &Store {
        match sq {
            SubQueue::Main => &self.main,
            SubQueue::DeadLetter => &self.dlq,
            SubQueue::TransferDeadLetter => &self.tdlq,
        }
    }

    pub fn store_mut(&mut self, sq: SubQueue) -> &mut Store {
        match sq {
            SubQueue::Main => &mut self.main,
            SubQueue::DeadLetter => &mut self.dlq,
            SubQueue::TransferDeadLetter => &mut self.tdlq,
        }
    }

    pub fn counts(&self) -> Counts {
        Counts {
            active: self.main.count_active(),
            dead_letter: self.dlq.len(),
            scheduled: self.main.count_scheduled(),
            transfer: 0,
            transfer_dead_letter: self.tdlq.len(),
        }
    }

    pub fn size_bytes(&self) -> u64 {
        self.main.bytes + self.dlq.bytes + self.tdlq.bytes
    }

    // ---- waiters ---------------------------------------------------------------------------

    pub fn add_waiter(&mut self, key: (u64, u64), handle: LinkHandle, sub: SubQueue) {
        self.waiters.insert(key, Waiter { handle, sub });
    }
    pub fn remove_waiter(&mut self, key: (u64, u64)) {
        self.waiters.remove(&key);
        self.session_waiters.remove(&key);
    }
    pub fn add_session_waiter(&mut self, key: (u64, u64), handle: LinkHandle) {
        self.session_waiters.insert(key, handle);
    }

    fn wake(&mut self, sub: SubQueue) {
        let keys: Vec<(u64, u64)> = self.waiters.iter().filter(|(_, w)| w.sub == sub).map(|(k, _)| *k).collect();
        for k in keys {
            if let Some(w) = self.waiters.remove(&k) {
                w.handle.wake();
            }
        }
        if sub == SubQueue::Main {
            for (_, h) in self.session_waiters.drain() {
                h.wake();
            }
        }
    }

    /// Wake everything (the entity changed in a way receivers must notice, e.g. deleted).
    pub fn wake_all(&mut self) {
        for (_, w) in self.waiters.drain() {
            w.handle.wake();
        }
        for (_, h) in self.session_waiters.drain() {
            h.wake();
        }
    }

    pub fn has_waiters(&self) -> bool {
        !self.waiters.is_empty() || !self.session_waiters.is_empty()
    }

    // ---- sending ---------------------------------------------------------------------------

    /// Check everything `enqueue` checks, without storing: used to validate a whole batch first.
    pub fn check_send(&self, size: usize) -> Result<(), SbError> {
        self.check_send_limits(size)?;
        self.check_room(size)
    }

    /// The part of [`Core::check_send`] that does not depend on what is in the entity: the status
    /// and the size limit of one message.
    pub fn check_send_limits(&self, size: usize) -> Result<(), SbError> {
        match self.settings.status.as_str() {
            "Disabled" | "SendDisabled" => {
                return Err(SbError::new(
                    crate::errors::ENTITY_DISABLED,
                    400,
                    format!("The messaging entity '{}' is disabled for sending.", self.path),
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

    /// Is there room for one more message of `size` bytes (`MaxSizeInMegabytes`)? The message is charged
    /// its size plus [`MESSAGE_OVERHEAD`].
    fn check_room(&self, size: usize) -> Result<(), SbError> {
        self.check_room_for(charged(size))
    }

    /// Is there room for messages that are charged `charge` bytes together?
    pub fn check_room_for(&self, charge: u64) -> Result<(), SbError> {
        if self.size_bytes().saturating_add(charge) > self.settings.max_size_bytes() {
            return Err(SbError::new(
                azemu_amqp::condition::RESOURCE_LIMIT_EXCEEDED,
                403,
                format!(
                    "The maximum entity size has been reached or exceeded for Entity: '{}'. Size of entity in bytes:{}, Max entity size in bytes: {}. For more information please see https://aka.ms/ServiceBusEntitySizeLimit",
                    self.path,
                    self.size_bytes(),
                    self.settings.max_size_bytes()
                ),
            ));
        }
        Ok(())
    }

    /// Duplicate detection: true when a message with this id was accepted inside the window.
    /// Records the id otherwise. Always false when the entity does not require it.
    pub fn is_duplicate(&mut self, m: &Message, now: i64) -> bool {
        if !self.settings.requires_duplicate_detection {
            return false;
        }
        let Some(id) = message_id_string(m) else { return false };
        // Drop expired ids when the map has doubled since the last time (a purge on every send
        // would scan the whole map each time when none has expired yet).
        if self.dup.len() > self.dup_purge_at {
            self.dup.retain(|_, exp| *exp > now);
            self.dup_purge_at = (self.dup.len() * 2).max(DUP_PURGE_MIN);
        }
        match self.dup.get(&id) {
            Some(exp) if *exp > now => true,
            previous => {
                if let Some(j) = &mut self.journal {
                    j.dup.push((id.clone(), previous.copied()));
                }
                self.dup.insert(id, now + self.settings.duplicate_window_ms);
                self.extra_rev = next_revision();
                false
            }
        }
    }

    /// Everything `enqueue` refuses a message for that does not depend on what was sent before:
    /// the entity's state and limits and the session id. A transaction asks this when a send is made
    /// and again when it commits.
    pub fn validate_send(&self, inc: &Incoming) -> Result<(), SbError> {
        self.check_send(inc.size)?;
        self.check_session_id(inc)
    }

    /// [`Core::validate_send`] without the room left in the entity: what a commit checks first
    /// (room depends on the order of the operations of the transaction, so applying them finds out).
    pub fn validate_send_static(&self, inc: &Incoming) -> Result<(), SbError> {
        self.check_send_limits(inc.size)?;
        self.check_session_id(inc)
    }

    fn check_session_id(&self, inc: &Incoming) -> Result<(), SbError> {
        let session_id = inc.message.properties.as_ref().and_then(|p| p.group_id.as_ref());
        if self.settings.requires_session && session_id.is_none() {
            return Err(SbError::new(
                crate::errors::ARGUMENT_ERROR,
                400,
                "The SessionId was not set on a message, and it cannot be sent to the entity. Entities that have session support enabled can only receive messages that have the SessionId set to a valid value.",
            ));
        }
        Ok(())
    }

    pub fn enqueue(&mut self, inc: Incoming, now: i64) -> Result<Enqueued, SbError> {
        self.enqueue_with_seq(inc, now, None)
    }

    /// Like [`Core::enqueue`], storing the message under a sequence number reserved earlier with
    /// `store_mut(Main).alloc_seq()` (a scheduled send inside a transaction answers with its
    /// sequence number before the transaction commits).
    pub fn enqueue_with_seq(&mut self, inc: Incoming, now: i64, reserved: Option<u64>) -> Result<Enqueued, SbError> {
        self.validate_send(&inc)?;
        let session_id = inc.message.properties.as_ref().and_then(|p| p.group_id.clone());
        if self.is_duplicate(&inc.message, now) {
            return Ok(Enqueued::Duplicate);
        }
        let seq = reserved.unwrap_or_else(|| self.main.alloc_seq());
        let ttl_ms = effective_ttl(&inc.message, self.settings.default_ttl_ms);
        let mut message = inc.message;
        strip_server_annotations(&mut message);
        let scheduled = inc.scheduled_ms.filter(|t| *t > now);
        let (state, enqueued_ms) = match scheduled {
            Some(_) => (MsgState::Scheduled, now),
            None => (MsgState::Ready, now),
        };
        let msg = Msg {
            seq,
            enq_seq: inc.enq_seq.unwrap_or(0),
            state,
            message: Arc::new(message),
            enqueued_ms,
            scheduled_ms: scheduled,
            ttl_ms,
            expires_ms: if scheduled.is_some() { None } else { ttl_ms.map(|t| enqueued_ms.saturating_add(t)) },
            delivery_count: 0,
            lock: None,
            session_id,
            size: inc.size,
            dead_letter_source: inc.dead_letter_source,
            deferred: false,
        };
        self.main.insert(msg);
        if scheduled.is_none() {
            self.wake(SubQueue::Main);
        }
        Ok(Enqueued::Stored(seq))
    }

    // ---- time ------------------------------------------------------------------------------

    /// Apply everything that is due at `now`. Returns messages to forward to other entities.
    pub fn tick(&mut self, now: i64) -> Vec<Forward> {
        let mut out = std::mem::take(&mut self.stash);
        if !out.is_empty() {
            self.extra_rev = next_revision();
        }
        let mut woke_main = false;
        let mut woke_dlq = false;
        let mut woke_tdlq = false;

        // scheduled messages that are due become ready
        while let Some(seq) = self.main.pop_due_scheduled(now) {
            let ttl = self.main.get(seq).and_then(|m| m.ttl_ms);
            let scheduled = self.main.get(seq).and_then(|m| m.scheduled_ms).unwrap_or(now);
            self.main.update(seq, |m| {
                m.state = MsgState::Ready;
                m.enqueued_ms = scheduled;
                m.expires_ms = ttl.map(|t| scheduled.saturating_add(t));
            });
            woke_main = true;
        }

        // message locks that expired
        for sq in [SubQueue::Main, SubQueue::DeadLetter, SubQueue::TransferDeadLetter] {
            while let Some(seq) = self.store_mut(sq).pop_due_lock(now) {
                if let Some(f) = self.unlock_failed(sq, seq) {
                    out.push(f);
                }
                match sq {
                    SubQueue::Main => woke_main = true,
                    SubQueue::DeadLetter => woke_dlq = true,
                    SubQueue::TransferDeadLetter => woke_tdlq = true,
                }
            }
        }

        // session locks that expired
        let expired: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.lock.as_ref().map(|l| l.until_ms <= now).unwrap_or(false))
            .map(|(k, _)| k.clone())
            .collect();
        for sid in expired {
            if let Some(s) = self.sessions.get_mut(&sid) {
                s.lock = None;
            }
            self.forget_idle_session(&sid);
            out.extend(self.release_session_messages(&sid, true));
            woke_main = true;
        }

        // messages whose time to live ran out (not while locked)
        while let Some(seq) = self.main.pop_due_expiry(now) {
            let Some(m) = self.main.remove(seq) else { continue };
            if self.settings.dead_letter_on_expiry {
                let desc = "The message expired and was dead lettered.";
                if let Some(f) = self.dead_letter_msg(m, Some("TTLExpiredException".into()), Some(desc.into()), None, now) {
                    out.push(f);
                }
                woke_dlq = true;
            }
        }

        if woke_main {
            self.wake(SubQueue::Main);
        }
        if woke_dlq {
            self.wake(SubQueue::DeadLetter);
        }
        if woke_tdlq {
            self.wake(SubQueue::TransferDeadLetter);
        }
        out
    }

    /// Apply what is due, for an operation that has no way to forward messages: whatever has to
    /// be forwarded is kept for the next tick of the background ticker.
    fn tick_keeping_forwards(&mut self, now: i64) {
        let f = self.tick(now);
        if !f.is_empty() {
            self.extra_rev = next_revision();
        }
        self.stash.extend(f);
    }

    /// Earliest time this entity needs a tick, if any.
    pub fn next_due(&self) -> Option<i64> {
        if !self.stash.is_empty() {
            return Some(i64::MIN);
        }
        let sess = self.sessions.values().filter_map(|s| s.lock.as_ref().map(|l| l.until_ms)).min();
        [self.main.next_due(), self.dlq.next_due(), self.tdlq.next_due(), sess].into_iter().flatten().min()
    }

    /// A lock expired or was abandoned: count the attempt, dead-letter when the limit is reached.
    fn unlock_failed(&mut self, sq: SubQueue, seq: u64) -> Option<Forward> {
        let max = self.settings.max_delivery_count;
        let count = {
            let store = self.store_mut(sq);
            store.update(seq, |m| {
                m.delivery_count += 1;
                m.lock = None;
                m.state = if m.deferred { MsgState::Deferred } else { MsgState::Ready };
                m.delivery_count
            })?
        };
        if sq == SubQueue::Main && max > 0 && count >= max {
            let m = self.main.remove(seq)?;
            let desc = format!("Message could not be consumed after {count} delivery attempts.");
            return self.dead_letter_msg(m, Some("MaxDeliveryCountExceeded".into()), Some(desc), None, 0);
        }
        None
    }

    /// Unlock the messages locked through a session lock.
    fn release_session_messages(&mut self, sid: &str, count_attempt: bool) -> Vec<Forward> {
        let seqs: Vec<u64> = self
            .main
            .msgs
            .values()
            .filter(|m| {
                m.state == MsgState::Locked
                    && m.session_id.as_deref() == Some(sid)
                    && m.lock.as_ref().map(|l| l.via_session).unwrap_or(false)
            })
            .map(|m| m.seq)
            .collect();
        let mut out = Vec::new();
        for seq in seqs {
            if count_attempt {
                if let Some(f) = self.unlock_failed(SubQueue::Main, seq) {
                    out.push(f);
                }
            } else {
                self.main.update(seq, |m| {
                    m.lock = None;
                    m.state = MsgState::Ready;
                });
            }
        }
        out
    }

    // ---- dead-lettering --------------------------------------------------------------------

    /// Put an already removed message in the dead-letter queue (or forward it). `now` is only
    /// used for the new enqueue time bookkeeping and may be 0 when unknown.
    fn dead_letter_msg(
        &mut self,
        mut m: Msg,
        reason: Option<String>,
        description: Option<String>,
        modify: Option<Vec<(Value, Value)>>,
        _now: i64,
    ) -> Option<Forward> {
        apply_modifications(Arc::make_mut(&mut m.message), modify);
        let props = Arc::make_mut(&mut m.message).application_properties.get_or_insert_with(Vec::new);
        set_prop(props, PROP_DEAD_LETTER_REASON, reason.map(Value::String).unwrap_or(Value::Null));
        set_prop(props, PROP_DEAD_LETTER_DESCRIPTION, description.map(Value::String).unwrap_or(Value::Null));
        if let Some(target) = self.settings.forward_dead_lettered_to.clone() {
            return Some(Forward {
                target,
                incoming: Incoming {
                    size: m.size,
                    enq_seq: Some(if m.enq_seq == 0 { m.seq } else { m.enq_seq }),
                    dead_letter_source: Some(self.path.rsplit('/').next().unwrap_or(&self.path).to_string()),
                    scheduled_ms: None,
                    message: Arc::unwrap_or_clone(m.message),
                },
            });
        }
        let seq = self.dlq.alloc_seq();
        let session_id = m.session_id.clone();
        self.dlq.insert(Msg {
            seq,
            state: MsgState::Ready,
            lock: None,
            expires_ms: None,
            ttl_ms: None,
            scheduled_ms: None,
            session_id,
            dead_letter_source: None,
            deferred: false,
            ..m
        });
        // every way a message reaches the dead-letter queue ends here, so receivers waiting on it
        // are woken for all of them (a lock that ran out, abandon at the delivery limit, time to
        // live, an explicit dead-letter)
        self.wake(SubQueue::DeadLetter);
        None
    }

    // ---- receiving -------------------------------------------------------------------------

    /// Take the next message for a receiver. `session` is the session the receiver is bound to.
    pub fn pull(
        &mut self,
        sq: SubQueue,
        session: Option<&str>,
        owner: Owner,
        mode: ReceiveMode,
        now: i64,
        waiter: Option<((u64, u64), LinkHandle)>,
    ) -> Result<(Option<Delivered>, Vec<Forward>), SbError> {
        let fwd = self.tick(now);
        let mut session_until = None;
        if let Some(sid) = session {
            match self.sessions.get(sid).and_then(|s| s.lock.as_ref()) {
                Some(l) if l.owner == owner && l.until_ms > now => session_until = Some(l.until_ms),
                _ => return Err(SbError::session_lock_lost()),
            }
        }
        let Some(seq) = self.store(sq).first_ready(session) else {
            if let Some((k, h)) = waiter {
                self.add_waiter(k, h, sq);
            }
            return Ok((None, fwd));
        };
        let lock_ms = self.settings.lock_duration_ms;
        let store = self.store_mut(sq);
        let delivered = match mode {
            ReceiveMode::ReceiveAndDelete => {
                let m = store.remove(seq).ok_or_else(|| SbError::argument("message vanished"))?;
                Delivered { msg: m, token: None }
            }
            ReceiveMode::PeekLock => {
                let token = new_token();
                let until = session_until.unwrap_or(now + lock_ms);
                let m = store
                    .update(seq, |m| {
                        m.state = MsgState::Locked;
                        m.lock = Some(Lock { token, until_ms: until, owner, via_session: session_until.is_some() });
                        m.clone()
                    })
                    .ok_or_else(|| SbError::argument("message vanished"))?;
                Delivered { msg: m, token: Some(token) }
            }
        };
        Ok((Some(delivered), fwd))
    }

    /// Find the store holding a lock token.
    fn locate(&self, token: &Token) -> Option<(SubQueue, u64)> {
        for sq in [SubQueue::Main, SubQueue::DeadLetter, SubQueue::TransferDeadLetter] {
            if let Some(seq) = self.store(sq).seq_for_token(token) {
                return Some((sq, seq));
            }
        }
        None
    }

    /// Settle a locked message by lock token.
    pub fn settle(&mut self, token: &Token, outcome: Outcome, now: i64) -> Result<Vec<Forward>, SbError> {
        let mut fwd = self.tick(now);
        let (sq, seq) = self.locate(token).ok_or_else(SbError::message_lock_lost)?;
        let (via_session, sid) = {
            let m = self.store(sq).get(seq).ok_or_else(SbError::message_lock_lost)?;
            (m.lock.as_ref().map(|l| l.via_session).unwrap_or(false), m.session_id.clone())
        };
        if via_session {
            let ok =
                sid.as_ref().and_then(|s| self.sessions.get(s)).and_then(|s| s.lock.as_ref()).map(|l| l.until_ms > now).unwrap_or(false);
            if !ok {
                return Err(SbError::session_lock_lost());
            }
        }
        match outcome {
            Outcome::Complete => {
                self.store_mut(sq).remove(seq);
            }
            Outcome::Release => {
                self.store_mut(sq).update(seq, |m| {
                    m.lock = None;
                    m.state = if m.deferred { MsgState::Deferred } else { MsgState::Ready };
                });
                self.wake(sq);
            }
            Outcome::Abandon { modify } => {
                self.store_mut(sq).update(seq, |m| apply_modifications(Arc::make_mut(&mut m.message), modify));
                if let Some(f) = self.unlock_failed(sq, seq) {
                    fwd.push(f);
                }
                self.wake(sq);
            }
            Outcome::Defer { modify } => {
                self.store_mut(sq).update(seq, |m| {
                    apply_modifications(Arc::make_mut(&mut m.message), modify);
                    m.lock = None;
                    m.deferred = true;
                    m.state = MsgState::Deferred;
                });
            }
            Outcome::DeadLetter { reason, description, modify } => {
                if sq == SubQueue::Main {
                    if let Some(m) = self.main.remove(seq) {
                        if let Some(f) = self.dead_letter_msg(m, reason, description, modify, now) {
                            fwd.push(f);
                        }
                    }
                    self.wake(SubQueue::DeadLetter);
                } else {
                    // a message in a dead-letter queue cannot be dead-lettered again
                    self.store_mut(sq).update(seq, |m| {
                        m.lock = None;
                        m.state = MsgState::Ready;
                    });
                    self.wake(sq);
                }
            }
        }
        Ok(fwd)
    }

    /// Would a settlement of this lock token succeed now? Changes nothing but what is due at `now`
    /// (a transaction asks when a disposition is made, and again when it commits).
    pub fn check_lock(&mut self, token: &Token, now: i64) -> Result<(), SbError> {
        self.tick_keeping_forwards(now);
        let (sq, seq) = self.locate(token).ok_or_else(SbError::message_lock_lost)?;
        let m = self.store(sq).get(seq).ok_or_else(SbError::message_lock_lost)?;
        if m.lock.as_ref().map(|l| l.via_session).unwrap_or(false)
            && !m.session_id.as_ref().map(|s| self.session_locked(s, now)).unwrap_or(false)
        {
            return Err(SbError::session_lock_lost());
        }
        Ok(())
    }

    /// Is the session locked (by anyone) at `now`?
    pub fn session_locked(&self, sid: &str, now: i64) -> bool {
        self.sessions.get(sid).and_then(|s| s.lock.as_ref()).map(|l| l.until_ms > now).unwrap_or(false)
    }

    /// Renew the lock of a message.
    pub fn renew_lock(&mut self, token: &Token, now: i64) -> Result<i64, SbError> {
        self.tick_keeping_forwards(now);
        let (sq, seq) = self.locate(token).ok_or_else(SbError::message_lock_lost)?;
        let until = now + self.settings.lock_duration_ms;
        let via_session = self.store(sq).get(seq).and_then(|m| m.lock.as_ref().map(|l| l.via_session)).unwrap_or(false);
        if via_session {
            return Err(SbError::message_lock_lost());
        }
        self.store_mut(sq).update(seq, |m| {
            if let Some(l) = &mut m.lock {
                l.until_ms = until;
            }
        });
        Ok(until)
    }

    /// Fetch deferred messages by sequence number.
    pub fn receive_deferred(&mut self, seqs: &[i64], mode: ReceiveMode, owner: Owner, now: i64) -> Result<Vec<Delivered>, SbError> {
        self.tick_keeping_forwards(now);
        let mut out = Vec::new();
        for s in seqs {
            let seq = *s as u64;
            match self.main.get(seq) {
                Some(m) if m.state == MsgState::Deferred => {}
                _ => return Err(SbError::message_not_found(*s)),
            }
            let lock_ms = self.settings.lock_duration_ms;
            let delivered = match mode {
                ReceiveMode::ReceiveAndDelete => match self.main.remove(seq) {
                    Some(m) => Delivered { msg: m, token: None },
                    None => continue,
                },
                ReceiveMode::PeekLock => {
                    let token = new_token();
                    let m = self.main.update(seq, |m| {
                        m.state = MsgState::Locked;
                        m.lock = Some(Lock { token, until_ms: now + lock_ms, owner, via_session: false });
                        m.clone()
                    });
                    match m {
                        Some(m) => Delivered { msg: m, token: Some(token) },
                        None => continue,
                    }
                }
            };
            out.push(delivered);
        }
        Ok(out)
    }

    /// Browse messages starting at `from_seq` (does not lock or remove anything).
    pub fn peek(&mut self, sq: SubQueue, from_seq: u64, count: usize, session: Option<&str>, now: i64) -> Vec<Msg> {
        self.peek_limited(sq, from_seq, count, session, now, usize::MAX)
    }

    /// Like [`Core::peek`], but stops after `max_bytes` of messages (the message that crosses the
    /// limit is still included, so a peek always makes progress). Only what is returned is cloned.
    pub fn peek_limited(
        &mut self,
        sq: SubQueue,
        from_seq: u64,
        count: usize,
        session: Option<&str>,
        now: i64,
        max_bytes: usize,
    ) -> Vec<Msg> {
        self.tick_keeping_forwards(now);
        let mut total = 0usize;
        let mut out = Vec::new();
        for m in self
            .store(sq)
            .msgs
            .range(from_seq..)
            .map(|(_, m)| m)
            .filter(|m| session.map(|s| m.session_id.as_deref() == Some(s)).unwrap_or(true))
            .take(count)
        {
            if total >= max_bytes {
                break;
            }
            total = total.saturating_add(m.size);
            out.push(m.clone());
        }
        out
    }

    pub fn cancel_scheduled(&mut self, seq: u64) -> Result<(), SbError> {
        match self.main.get(seq) {
            Some(m) if m.state == MsgState::Scheduled => {
                self.main.remove(seq);
                Ok(())
            }
            _ => Err(SbError::message_not_found(seq as i64)),
        }
    }

    // ---- sessions --------------------------------------------------------------------------

    pub fn accept_session(&mut self, sid: &str, owner: Owner, now: i64) -> Result<i64, SbError> {
        self.tick_keeping_forwards(now);
        let until = now + self.settings.lock_duration_ms;
        let info = self.sessions.entry(sid.to_string()).or_default();
        if let Some(l) = &info.lock {
            if l.until_ms > now && l.owner != owner {
                return Err(SbError::new(
                    crate::errors::SESSION_CANNOT_BE_LOCKED,
                    410,
                    format!("The requested session '{sid}' cannot be accepted. It may be locked by another receiver."),
                ));
            }
        }
        info.lock = Some(SessionLock { owner, until_ms: until });
        info.last_updated_ms = now;
        Ok(until)
    }

    /// Pick a session with ready messages that nobody holds and lock it.
    pub fn accept_next_session(&mut self, owner: Owner, now: i64) -> Option<(String, i64)> {
        self.tick_keeping_forwards(now);
        let mut candidates = self.main.sessions_with_ready();
        candidates.sort_by_key(|(_, seq)| *seq);
        for (sid, _) in candidates {
            let locked = self.sessions.get(&sid).and_then(|s| s.lock.as_ref()).map(|l| l.until_ms > now).unwrap_or(false);
            if !locked {
                if let Ok(until) = self.accept_session(&sid, owner, now) {
                    return Some((sid, until));
                }
            }
        }
        None
    }

    pub fn renew_session_lock(&mut self, sid: &str, now: i64) -> Result<i64, SbError> {
        self.tick_keeping_forwards(now);
        let until = now + self.settings.lock_duration_ms;
        match self.sessions.get_mut(sid).and_then(|s| s.lock.as_mut()) {
            Some(l) if l.until_ms > now => {
                l.until_ms = until;
                Ok(until)
            }
            _ => Err(SbError::session_lock_lost()),
        }
    }

    /// Release a session lock held by `owner` (the receiver detached). Locked messages go back
    /// to the queue without counting a delivery attempt.
    pub fn release_session(&mut self, sid: &str, owner: Owner) -> Vec<Forward> {
        let held = self.sessions.get(sid).and_then(|s| s.lock.as_ref()).map(|l| l.owner == owner).unwrap_or(false);
        if !held {
            return vec![];
        }
        if let Some(s) = self.sessions.get_mut(sid) {
            s.lock = None;
        }
        let out = self.release_session_messages(sid, false);
        self.forget_idle_session(sid);
        self.wake(SubQueue::Main);
        out
    }

    /// A session record with no lock and no state says nothing (a session id that was only accepted once):
    /// drop it, so accepting made-up session ids does not make records for ever.
    fn forget_idle_session(&mut self, sid: &str) {
        if self.sessions.get(sid).map(|s| s.lock.is_none() && s.state.is_none()).unwrap_or(false) {
            self.sessions.remove(sid);
        }
    }

    pub fn session_state(&self, sid: &str) -> Option<Vec<u8>> {
        self.sessions.get(sid).and_then(|s| s.state.clone())
    }

    pub fn set_session_state(&mut self, sid: &str, state: Option<Vec<u8>>, now: i64) -> Result<(), SbError> {
        if let Some(j) = &mut self.journal {
            let before = self.sessions.get(sid).map(|s| (s.state.clone(), s.last_updated_ms));
            j.sessions.entry(sid.to_string()).or_insert(before);
        }
        let info = self.sessions.entry(sid.to_string()).or_default();
        let locked = info.lock.as_ref().map(|l| l.until_ms > now).unwrap_or(false);
        if !locked {
            let idle = info.lock.is_none() && info.state.is_none();
            if idle {
                self.sessions.remove(sid);
            }
            return Err(SbError::session_lock_lost());
        }
        if state.as_ref().map(|s| s.len()).unwrap_or(0) > MAX_SESSION_STATE_BYTES {
            return Err(SbError::new(
                crate::errors::ARGUMENT_ERROR,
                400,
                format!("The session state is larger than the {MAX_SESSION_STATE_BYTES} bytes (256 KB) a session may hold."),
            ));
        }
        info.state = state.filter(|s| !s.is_empty());
        info.last_updated_ms = now;
        self.extra_rev = next_revision();
        Ok(())
    }

    /// Ids of sessions that have messages, state or a lock.
    pub fn session_ids(&self, now: i64) -> Vec<String> {
        let mut ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for m in self.main.msgs.values() {
            if let Some(s) = &m.session_id {
                ids.insert(s.clone());
            }
        }
        for (k, v) in &self.sessions {
            if v.state.is_some() || v.lock.as_ref().map(|l| l.until_ms > now).unwrap_or(false) {
                ids.insert(k.clone());
            }
        }
        ids.into_iter().collect()
    }

    /// Everything gone: used when the entity is deleted.
    pub fn clear(&mut self) {
        self.main = Store::new();
        self.dlq = Store::new();
        self.tdlq = Store::new();
        self.sessions.clear();
        self.dup.clear();
        self.extra_rev = next_revision();
    }
}

impl Core {
    /// After a restart no receiver holds anything: every message that was locked comes back as it does when its
    /// lock runs out (one more failed delivery attempt, and the dead-letter queue once the delivery limit is
    /// reached). Messages that have to be forwarded because of it wait in the stash for the next tick.
    pub fn release_locks_after_restart(&mut self) {
        for sq in [SubQueue::Main, SubQueue::DeadLetter, SubQueue::TransferDeadLetter] {
            let locked: Vec<u64> = self.store(sq).msgs.values().filter(|m| m.state == MsgState::Locked).map(|m| m.seq).collect();
            for seq in locked {
                let has_lock = self.store(sq).get(seq).map(|m| m.lock.is_some()).unwrap_or(false);
                if has_lock {
                    if let Some(f) = self.unlock_failed(sq, seq) {
                        self.stash.push(f);
                    }
                } else {
                    self.store_mut(sq).update(seq, |m| m.state = if m.deferred { MsgState::Deferred } else { MsgState::Ready });
                }
            }
        }
        self.sessions.retain(|_, s| s.state.is_some());
        for s in self.sessions.values_mut() {
            s.lock = None;
        }
    }
}

pub use crate::msgutil::*;

#[cfg(test)]
mod dup_tests {
    use super::*;
    use crate::model::Settings;

    #[test]
    fn duplicate_ids_are_not_purged_on_every_send() {
        let mut s = Settings::queue_default(0);
        s.requires_duplicate_detection = true;
        s.duplicate_window_ms = 7 * 86_400_000;
        let mut core = Core::new("q", s);
        for i in 0..120_000 {
            let m = msg_with_id(&format!("id-{i}"));
            assert!(!core.is_duplicate(&m, 1000));
        }
        // nothing had expired, so each purge found nothing to drop and the next one waits until the
        // map has doubled: 50k, 100k, then 200k (not a scan of the whole map on every send)
        assert!(core.dup_purge_at >= 200_000, "next purge at {}", core.dup_purge_at);
        assert_eq!(core.dup.len(), 120_000);
        // expired ids go when the purge runs
        let later = 1000 + 8 * 86_400_000;
        core.dup_purge_at = 0;
        assert!(!core.is_duplicate(&msg_with_id("fresh"), later));
        assert_eq!(core.dup.len(), 1);
    }

    fn msg_with_id(id: &str) -> Message {
        Message {
            properties: Some(azemu_amqp::message::Properties { message_id: Some(azemu_amqp::types::Value::str(id)), ..Default::default() }),
            ..Message::data(vec![])
        }
    }
}
