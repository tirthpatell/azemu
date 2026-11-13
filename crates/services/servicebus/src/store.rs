//! One ordered set of messages (a queue's active store, its dead-letter queue, ...) with the
//! indexes the engine needs. All time-dependent transitions are applied lazily by the owner
//! (see `core.rs`): the indexes make "what is due" a pop from the front of an ordered set.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use azemu_amqp::message::Message;

/// A message lock token: a UUID in RFC 4122 byte order (the order the SDK sends in
/// management requests). The delivery tag on the wire is the same GUID in .NET (little-endian
/// first three groups) byte order, see [`token_to_tag`].
pub type Token = [u8; 16];

pub fn new_token() -> Token {
    *uuid::Uuid::new_v4().as_bytes()
}

/// The AMQP delivery tag Service Bus uses for a lock token: `uuid.UUID(bytes_le=tag)` in the SDK.
pub fn token_to_tag(t: &Token) -> Vec<u8> {
    uuid::Uuid::from_bytes(*t).to_bytes_le().to_vec()
}

pub fn tag_to_token(tag: &[u8]) -> Option<Token> {
    let arr: [u8; 16] = tag.try_into().ok()?;
    Some(*uuid::Uuid::from_bytes_le(arr).as_bytes())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgState {
    Ready,
    Locked,
    Deferred,
    Scheduled,
}

/// Who holds a lock: the connection and link that received the message (0/0: nobody in
/// particular, for example a deferred message fetched over `$management`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Owner {
    pub conn: u64,
    pub link: u64,
}

#[derive(Debug, Clone)]
pub struct Lock {
    pub token: Token,
    pub until_ms: i64,
    pub owner: Owner,
    /// Locked through a session lock: the session's expiry applies, not `until_ms`.
    pub via_session: bool,
}

/// What every stored message is charged besides its encoded bytes, against the entity's `MaxSizeInMegabytes`
/// (and shown in `SizeInBytes`): the decoded message and its bookkeeping in memory, and the entries in the
/// indexes. Without it a message with no content costs a few bytes of the limit while it takes hundreds in
/// memory, so one small request could fill the heap. A test checks it against `size_of`.
pub const MESSAGE_OVERHEAD: usize = 1024;

/// What a message counts for in its entity: its encoded size plus [`MESSAGE_OVERHEAD`].
pub fn charged(size: usize) -> u64 {
    size as u64 + MESSAGE_OVERHEAD as u64
}

#[derive(Debug, Clone)]
pub struct Msg {
    pub seq: u64,
    /// Sequence number first assigned (differs from `seq` after forwarding).
    pub enq_seq: u64,
    pub state: MsgState,
    /// Shared, so that a copy of a stored message (a peek, a save to disk) does not copy its content.
    pub message: Arc<Message>,
    pub enqueued_ms: i64,
    pub scheduled_ms: Option<i64>,
    pub ttl_ms: Option<i64>,
    pub expires_ms: Option<i64>,
    pub delivery_count: u32,
    pub lock: Option<Lock>,
    pub session_id: Option<String>,
    pub size: usize,
    pub dead_letter_source: Option<String>,
    /// Was deferred by a receiver: goes back to `Deferred` (not `Ready`) when its lock ends.
    pub deferred: bool,
}

static REVISION: AtomicU64 = AtomicU64::new(1);

/// A number nobody has had before in this process. Every change to something that is saved to disk takes one
/// (see `persist.rs`): the highest number handed out is the service's `persist_generation`, and a store or entity
/// whose number differs from the one at the last save has changed since. Numbers are never reused, so a store
/// that is deleted and made again cannot look like the one that was saved.
pub fn next_revision() -> u64 {
    REVISION.fetch_add(1, Ordering::SeqCst)
}

/// The number the last change took.
pub fn current_revision() -> u64 {
    REVISION.load(Ordering::SeqCst)
}

pub struct Store {
    pub msgs: BTreeMap<u64, Msg>,
    ready: BTreeSet<u64>,
    ready_by_session: HashMap<String, BTreeSet<u64>>,
    scheduled: BTreeSet<(i64, u64)>,
    lock_expiry: BTreeSet<(i64, u64)>,
    expiry: BTreeSet<(i64, u64)>,
    lock_index: HashMap<Token, u64>,
    next_seq: u64,
    pub bytes: u64,
    /// While a transaction commits: what every touched message looked like before (None: it did not
    /// exist), so that the whole commit can be taken back.
    journal: Option<Journal>,
    /// Changes with every change to the messages, the sequence counter or a lock.
    pub rev: u64,
}

impl Default for Store {
    fn default() -> Store {
        Store {
            msgs: BTreeMap::new(),
            ready: BTreeSet::new(),
            ready_by_session: HashMap::new(),
            scheduled: BTreeSet::new(),
            lock_expiry: BTreeSet::new(),
            expiry: BTreeSet::new(),
            lock_index: HashMap::new(),
            next_seq: 0,
            bytes: 0,
            journal: None,
            rev: next_revision(),
        }
    }
}

struct Journal {
    saved: HashMap<u64, Option<Msg>>,
    /// The byte count at the start (messages can change size while they are updated).
    bytes: u64,
}

impl Store {
    pub fn new() -> Store {
        Store { next_seq: 1, ..Default::default() }
    }

    pub fn alloc_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq += 1;
        self.rev = next_revision();
        s
    }

    /// The sequence number the next message gets.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// For loading: the next sequence number is at least `n`.
    pub fn raise_next_seq(&mut self, n: u64) {
        self.next_seq = self.next_seq.max(n);
    }

    fn index(&mut self, seq: u64) {
        let Some(m) = self.msgs.get(&seq) else { return };
        match m.state {
            MsgState::Ready => {
                self.ready.insert(seq);
                if let Some(sid) = &m.session_id {
                    self.ready_by_session.entry(sid.clone()).or_default().insert(seq);
                }
                if let Some(e) = m.expires_ms {
                    self.expiry.insert((e, seq));
                }
            }
            MsgState::Locked => {
                if let Some(l) = &m.lock {
                    if !l.via_session {
                        self.lock_expiry.insert((l.until_ms, seq));
                    }
                    self.lock_index.insert(l.token, seq);
                }
            }
            MsgState::Deferred => {
                if let Some(e) = m.expires_ms {
                    self.expiry.insert((e, seq));
                }
            }
            MsgState::Scheduled => {
                if let Some(t) = m.scheduled_ms {
                    self.scheduled.insert((t, seq));
                }
            }
        }
    }

    fn unindex(&mut self, seq: u64) {
        let Some(m) = self.msgs.get(&seq) else { return };
        self.ready.remove(&seq);
        if let Some(sid) = &m.session_id {
            let empty = self.ready_by_session.get_mut(sid).map(|s| {
                s.remove(&seq);
                s.is_empty()
            });
            if empty == Some(true) {
                self.ready_by_session.remove(sid);
            }
        }
        if let Some(e) = m.expires_ms {
            self.expiry.remove(&(e, seq));
        }
        if let Some(t) = m.scheduled_ms {
            self.scheduled.remove(&(t, seq));
        }
        if let Some(l) = &m.lock {
            self.lock_expiry.remove(&(l.until_ms, seq));
            self.lock_index.remove(&l.token);
        }
    }

    /// Start remembering what changes, so that [`Store::end_journal`] can take it back.
    pub fn begin_journal(&mut self) {
        self.journal = Some(Journal { saved: HashMap::new(), bytes: self.bytes });
    }

    /// Stop remembering. With `undo` every message touched since [`Store::begin_journal`] is put back
    /// as it was (messages that were added go, messages that were removed or changed return).
    /// Sequence numbers that were handed out stay used.
    pub fn end_journal(&mut self, undo: bool) {
        let Some(journal) = self.journal.take() else { return };
        if !undo {
            return;
        }
        for (seq, before) in journal.saved {
            self.remove(seq);
            if let Some(m) = before {
                self.insert(m);
            }
        }
        self.bytes = journal.bytes;
    }

    /// Remember a message before it changes (the first time only).
    fn note(&mut self, seq: u64) {
        if let Some(j) = &mut self.journal {
            j.saved.entry(seq).or_insert_with(|| self.msgs.get(&seq).cloned());
        }
    }

    pub fn insert(&mut self, m: Msg) {
        let seq = m.seq;
        self.rev = next_revision();
        self.note(seq);
        if self.msgs.contains_key(&seq) {
            // never happens (sequence numbers are unique), but the byte count must stay right
            self.remove(seq);
        }
        self.bytes += charged(m.size);
        self.msgs.insert(seq, m);
        self.index(seq);
    }

    pub fn remove(&mut self, seq: u64) -> Option<Msg> {
        self.note(seq);
        self.unindex(seq);
        let m = self.msgs.remove(&seq)?;
        self.rev = next_revision();
        self.bytes = self.bytes.saturating_sub(charged(m.size));
        Some(m)
    }

    /// Change a message and keep the indexes right.
    pub fn update<R>(&mut self, seq: u64, f: impl FnOnce(&mut Msg) -> R) -> Option<R> {
        if !self.msgs.contains_key(&seq) {
            return None;
        }
        self.note(seq);
        self.unindex(seq);
        self.rev = next_revision();
        let r = self.msgs.get_mut(&seq).map(f);
        self.index(seq);
        r
    }

    pub fn get(&self, seq: u64) -> Option<&Msg> {
        self.msgs.get(&seq)
    }

    pub fn seq_for_token(&self, t: &Token) -> Option<u64> {
        self.lock_index.get(t).copied()
    }

    /// Oldest ready message, optionally of one session.
    pub fn first_ready(&self, session: Option<&str>) -> Option<u64> {
        match session {
            Some(sid) => self.ready_by_session.get(sid).and_then(|s| s.first().copied()),
            None => self.ready.first().copied(),
        }
    }

    pub fn has_ready(&self) -> bool {
        !self.ready.is_empty()
    }

    /// Sessions that have ready messages, with the oldest ready sequence of each.
    pub fn sessions_with_ready(&self) -> Vec<(String, u64)> {
        self.ready_by_session.iter().filter_map(|(k, v)| v.first().map(|s| (k.clone(), *s))).collect()
    }

    pub fn pop_due_scheduled(&mut self, now: i64) -> Option<u64> {
        let first = *self.scheduled.first()?;
        if first.0 <= now {
            Some(first.1)
        } else {
            None
        }
    }

    pub fn pop_due_lock(&mut self, now: i64) -> Option<u64> {
        let first = *self.lock_expiry.first()?;
        if first.0 <= now {
            Some(first.1)
        } else {
            None
        }
    }

    pub fn pop_due_expiry(&mut self, now: i64) -> Option<u64> {
        let first = *self.expiry.first()?;
        if first.0 <= now {
            Some(first.1)
        } else {
            None
        }
    }

    /// Earliest time something in this store becomes due.
    pub fn next_due(&self) -> Option<i64> {
        [self.scheduled.first(), self.lock_expiry.first(), self.expiry.first()].into_iter().flatten().map(|(t, _)| *t).min()
    }

    pub fn count_active(&self) -> usize {
        self.msgs.values().filter(|m| m.state != MsgState::Scheduled).count()
    }
    pub fn count_scheduled(&self) -> usize {
        self.scheduled.len()
    }
    pub fn len(&self) -> usize {
        self.msgs.len()
    }
    pub fn is_empty(&self) -> bool {
        self.msgs.is_empty()
    }
    pub fn clear_session_locks(&mut self, owner: Owner) -> Vec<u64> {
        self.msgs
            .values()
            .filter(|m| m.state == MsgState::Locked && m.lock.as_ref().map(|l| l.owner == owner).unwrap_or(false))
            .map(|m| m.seq)
            .collect()
    }
}

#[cfg(test)]
mod journal_tests {
    use super::*;

    fn msg(seq: u64, size: usize, state: MsgState) -> Msg {
        Msg {
            seq,
            enq_seq: seq,
            state,
            message: Message::default().into(),
            enqueued_ms: 0,
            scheduled_ms: None,
            ttl_ms: None,
            expires_ms: Some(1000 + seq as i64),
            delivery_count: 0,
            lock: None,
            session_id: Some("s".into()),
            size,
            dead_letter_source: None,
            deferred: false,
        }
    }

    #[test]
    fn overhead_covers_the_memory_of_a_stored_message() {
        // the message in the map (value and key), its entries in the ordered sets and maps that index it
        // (sequence, expiry or schedule pair, session set, lock token), with room for the trees' own nodes
        let per_message = std::mem::size_of::<Msg>() + 8 + 2 * 16 + 2 * (16 + 8) + 3 * std::mem::size_of::<String>();
        assert!(per_message <= MESSAGE_OVERHEAD, "a message takes {per_message} bytes besides its data: raise MESSAGE_OVERHEAD");
    }

    #[test]
    fn every_message_is_charged_its_overhead_whatever_it_holds() {
        let mut s = Store::new();
        for i in 0..10 {
            s.insert(msg(i, 0, MsgState::Ready));
        }
        assert_eq!(s.bytes, 10 * MESSAGE_OVERHEAD as u64);
        s.remove(3);
        assert_eq!(s.bytes, 9 * MESSAGE_OVERHEAD as u64);
    }

    #[test]
    fn undo_puts_back_messages_indexes_and_bytes() {
        let mut s = Store::new();
        s.insert(msg(1, 10, MsgState::Ready));
        s.insert(msg(2, 20, MsgState::Ready));
        let token = new_token();
        s.update(2, |m| {
            m.state = MsgState::Locked;
            m.lock = Some(Lock { token, until_ms: 500, owner: Owner::default(), via_session: false });
        });
        let before = (s.bytes, s.len(), s.first_ready(None), s.seq_for_token(&token), s.next_due(), s.first_ready(Some("s")));
        s.begin_journal();
        s.remove(1);
        s.update(2, |m| {
            m.state = MsgState::Ready;
            m.lock = None;
            m.size = 99;
        });
        s.insert(msg(3, 5, MsgState::Ready));
        s.remove(3);
        s.insert(msg(4, 7, MsgState::Scheduled));
        assert_ne!(s.bytes, before.0);
        s.end_journal(true);
        assert_eq!((s.bytes, s.len(), s.first_ready(None), s.seq_for_token(&token), s.next_due(), s.first_ready(Some("s"))), before);
        assert!(s.get(3).is_none() && s.get(4).is_none());
        assert_eq!(s.get(2).unwrap().size, 20);
        assert_eq!(s.count_scheduled(), 0);
    }

    #[test]
    fn keeping_the_changes_leaves_them() {
        let mut s = Store::new();
        s.insert(msg(1, 10, MsgState::Ready));
        s.begin_journal();
        s.remove(1);
        s.insert(msg(2, 5, MsgState::Ready));
        s.end_journal(false);
        assert_eq!((s.bytes, s.len()), (5 + MESSAGE_OVERHEAD as u64, 1));
    }
}
