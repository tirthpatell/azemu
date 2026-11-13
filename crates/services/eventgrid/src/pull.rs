//! The queue behind a pull subscription: lock tokens, lock expiry, release with delay,
//! delivery counts, dead-lettering and time to live.
//!
//! Every operation first runs `sweep`, which applies whatever expired since the last call,
//! so no timer is needed for correctness. Long polls use `next_wake` to know when to look
//! again.
//!
//! An event is delivered at most `max_delivery` times: when a lock is lost (expiry or
//! release) and the delivery count has reached the maximum, the event is dead-lettered
//! instead of being offered again.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::Serialize;
use serde_json::Value;

use crate::model::PullParams;

/// A moment on both clocks: monotonic for deadlines, wall for what is shown to users.
#[derive(Clone, Copy, Debug)]
pub struct Now {
    pub mono: Instant,
    pub wall: azemu_core::time::OffsetDateTime,
}

impl Now {
    #[allow(clippy::self_named_constructors)]
    pub fn now() -> Now {
        Now { mono: Instant::now(), wall: azemu_core::time::OffsetDateTime::now_utc() }
    }
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeadLetter {
    pub event: Value,
    pub reason: String,
    pub description: String,
    pub delivery_count: u32,
    pub dead_lettered_at: String,
}

#[derive(Debug)]
enum State {
    Ready { not_before: Instant },
    Locked { token: String, until: Instant },
}

#[derive(Debug)]
struct Entry {
    event: Value,
    enqueued: Instant,
    delivery_count: u32,
    state: State,
}

/// One received event.
#[derive(Debug)]
pub struct Received {
    pub token: String,
    pub delivery_count: u32,
    pub event: Value,
}

/// Outcome of acknowledge / release / reject / renew: one result per lock token.
#[derive(Debug, Default)]
pub struct TokenResults {
    pub succeeded: Vec<String>,
    /// (token, error code, message)
    pub failed: Vec<(String, &'static str, &'static str)>,
}

#[derive(Debug, Default)]
pub struct Queue {
    next_seq: u64,
    entries: BTreeMap<u64, Entry>,
    locks: HashMap<String, u64>,
    /// Every token ever handed out: a token in here that is not in `locks` is "lost",
    /// one that was never in here is "bad".
    issued: HashSet<String>,
    pub dead: Vec<DeadLetter>,
    /// Size of the events in `dead` (JSON bytes): the list is also capped by bytes.
    dead_bytes: u64,
    dead_sizes: std::collections::VecDeque<u64>,
    pub total_enqueued: u64,
    pub total_acknowledged: u64,
    /// Changes with every change to the queue (see `state::next_revision`).
    pub rev: u64,
}

const MAX_DEAD_LETTERS: usize = 10_000;
/// At most this many bytes of events are kept dead-lettered per subscription; older ones are dropped first.
pub const MAX_DEAD_LETTER_BYTES: u64 = 16 * 1024 * 1024;

fn new_token() -> String {
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    let mut raw = a.as_bytes().to_vec();
    raw.extend_from_slice(&b.as_bytes()[..8]);
    STANDARD.encode(raw)
}

impl Queue {
    pub fn enqueue(&mut self, event: Value, now: &Now) {
        self.rev = crate::state::next_revision();
        let seq = self.next_seq;
        self.next_seq += 1;
        self.total_enqueued += 1;
        self.entries.insert(seq, Entry { event, enqueued: now.mono, delivery_count: 0, state: State::Ready { not_before: now.mono } });
    }

    pub fn dead_letter(&mut self, event: Value, reason: &str, description: &str, delivery_count: u32, now: &Now) {
        let bytes = event.to_string().len() as u64;
        self.dead_letter_sized(event, bytes, reason, description, delivery_count, now);
    }

    /// Same, when the size of the event in JSON bytes is already known. The oldest dead letters are dropped when
    /// there are more than `MAX_DEAD_LETTERS` or more than `MAX_DEAD_LETTER_BYTES`.
    pub fn dead_letter_sized(&mut self, event: Value, bytes: u64, reason: &str, description: &str, delivery_count: u32, now: &Now) {
        self.rev = crate::state::next_revision();
        self.dead.push(DeadLetter {
            event,
            reason: reason.to_string(),
            description: description.to_string(),
            delivery_count,
            dead_lettered_at: azemu_core::time::iso8601_7(now.wall),
        });
        self.dead_sizes.push_back(bytes);
        self.dead_bytes += bytes;
        // keep the newest one even when it alone is over the byte limit
        while self.dead.len() > 1 && (self.dead.len() > MAX_DEAD_LETTERS || self.dead_bytes > MAX_DEAD_LETTER_BYTES) {
            self.dead.remove(0);
            self.dead_bytes = self.dead_bytes.saturating_sub(self.dead_sizes.pop_front().unwrap_or(0));
        }
    }

    /// Apply expired locks and expired events.
    pub fn sweep(&mut self, now: &Now, p: &PullParams) {
        let mut lost: Vec<u64> = Vec::new();
        let mut expired: Vec<u64> = Vec::new();
        for (seq, e) in &self.entries {
            match &e.state {
                State::Locked { until, .. } if *until <= now.mono => lost.push(*seq),
                State::Ready { .. } if e.enqueued + p.ttl <= now.mono => expired.push(*seq),
                _ => {}
            }
        }
        if !lost.is_empty() || !expired.is_empty() {
            self.rev = crate::state::next_revision();
        }
        for seq in lost {
            self.lock_lost(seq, now, p, Duration::ZERO, "MaxDeliveryCountExceeded");
        }
        for seq in expired {
            if let Some(e) = self.entries.remove(&seq) {
                self.dead_letter(e.event, "TimeToLiveExpired", "The event outlived its time to live.", e.delivery_count, now);
            }
        }
    }

    /// The lock on `seq` is gone (expired or released): offer the event again, or
    /// dead-letter it when it has been delivered as often as allowed.
    fn lock_lost(&mut self, seq: u64, now: &Now, p: &PullParams, delay: Duration, reason: &str) {
        let Some(e) = self.entries.get_mut(&seq) else { return };
        if let State::Locked { token, .. } = &e.state {
            self.locks.remove(token);
        }
        if e.delivery_count >= p.max_delivery {
            if let Some(e) = self.entries.remove(&seq) {
                self.dead_letter(
                    e.event,
                    reason,
                    "The event was delivered the maximum number of times without being acknowledged.",
                    e.delivery_count,
                    now,
                );
            }
        } else {
            e.state = State::Ready { not_before: now.mono + delay };
        }
    }

    /// Lock and return up to `max` available events, oldest first.
    pub fn receive(&mut self, max: usize, now: &Now, p: &PullParams) -> Vec<Received> {
        self.sweep(now, p);
        let mut out = Vec::new();
        for e in self.entries.values_mut() {
            if out.len() >= max {
                break;
            }
            let State::Ready { not_before } = e.state else { continue };
            if not_before > now.mono {
                continue;
            }
            self.rev = crate::state::next_revision();
            let token = new_token();
            e.delivery_count += 1;
            e.state = State::Locked { token: token.clone(), until: now.mono + p.lock_duration };
            out.push(Received { token, delivery_count: e.delivery_count, event: e.event.clone() });
        }
        for (seq, e) in &self.entries {
            if let State::Locked { token, .. } = &e.state {
                if !self.locks.contains_key(token) {
                    self.locks.insert(token.clone(), *seq);
                    self.issued.insert(token.clone());
                }
            }
        }
        out
    }

    fn locked_seq(&self, token: &str) -> Result<u64, (&'static str, &'static str)> {
        match self.locks.get(token) {
            Some(seq) => Ok(*seq),
            None if self.issued.contains(token) => Err(("TokenLost", "The lock token has expired or was already used.")),
            None => Err(("BadToken", "The lock token is not valid.")),
        }
    }

    fn each_token(&mut self, tokens: &[String], now: &Now, p: &PullParams, mut f: impl FnMut(&mut Queue, u64)) -> TokenResults {
        self.sweep(now, p);
        let mut res = TokenResults::default();
        for t in tokens {
            match self.locked_seq(t) {
                Ok(seq) => {
                    self.rev = crate::state::next_revision();
                    self.locks.remove(t);
                    f(self, seq);
                    res.succeeded.push(t.clone());
                }
                Err((code, msg)) => res.failed.push((t.clone(), code, msg)),
            }
        }
        res
    }

    pub fn acknowledge(&mut self, tokens: &[String], now: &Now, p: &PullParams) -> TokenResults {
        self.each_token(tokens, now, p, |q, seq| {
            if q.entries.remove(&seq).is_some() {
                q.total_acknowledged += 1;
            }
        })
    }

    pub fn release(&mut self, tokens: &[String], delay: Duration, now: &Now, p: &PullParams) -> TokenResults {
        self.each_token(tokens, now, p, |q, seq| q.lock_lost(seq, now, p, delay, "MaxDeliveryCountExceeded"))
    }

    pub fn reject(&mut self, tokens: &[String], now: &Now, p: &PullParams) -> TokenResults {
        self.each_token(tokens, now, p, |q, seq| {
            if let Some(e) = q.entries.remove(&seq) {
                q.dead_letter(e.event, "Rejected", "The consumer rejected the event.", e.delivery_count, now);
            }
        })
    }

    pub fn renew(&mut self, tokens: &[String], now: &Now, p: &PullParams) -> TokenResults {
        self.sweep(now, p);
        let mut res = TokenResults::default();
        for t in tokens {
            match self.locked_seq(t) {
                Ok(seq) => {
                    self.rev = crate::state::next_revision();
                    if let Some(Entry { state: State::Locked { until, .. }, .. }) = self.entries.get_mut(&seq) {
                        *until = now.mono + p.lock_duration;
                    }
                    res.succeeded.push(t.clone());
                }
                Err((code, msg)) => res.failed.push((t.clone(), code, msg)),
            }
        }
        res
    }

    /// The next moment something changes by itself (a lock expires, a delayed event becomes
    /// available, an event reaches its time to live).
    pub fn next_wake(&self, now: &Now, p: &PullParams) -> Option<Instant> {
        let mut best: Option<Instant> = None;
        let mut consider = |t: Instant| {
            if best.is_none_or(|b| t < b) {
                best = Some(t);
            }
        };
        for e in self.entries.values() {
            match &e.state {
                State::Locked { until, .. } => consider(*until),
                State::Ready { not_before } => {
                    if *not_before > now.mono {
                        consider(*not_before);
                    }
                    consider(e.enqueued + p.ttl);
                }
            }
        }
        best
    }

    /// (available now, locked, waiting for a release delay)
    pub fn counts(&self, now: &Now) -> (usize, usize, usize) {
        let (mut ready, mut locked, mut delayed) = (0, 0, 0);
        for e in self.entries.values() {
            match &e.state {
                State::Locked { .. } => locked += 1,
                State::Ready { not_before } if *not_before > now.mono => delayed += 1,
                State::Ready { .. } => ready += 1,
            }
        }
        (ready, locked, delayed)
    }

    /// Events still in the queue, for the admin API: (event, delivery count, state name).
    pub fn snapshot(&self, now: &Now) -> Vec<(Value, u32, &'static str)> {
        self.entries
            .values()
            .map(|e| {
                let st = match &e.state {
                    State::Locked { .. } => "locked",
                    State::Ready { not_before } if *not_before > now.mono => "delayed",
                    State::Ready { .. } => "available",
                };
                (e.event.clone(), e.delivery_count, st)
            })
            .collect()
    }
}

// ---- saving (see `persist.rs`) ----------------------------------------------------------------------------------

fn wall_ms(now: &Now) -> i64 {
    (now.wall.unix_timestamp_nanos() / 1_000_000) as i64
}

/// A monotonic moment as a time on the wall clock (milliseconds since 1970), for a file.
fn to_wall_ms(now: &Now, t: Instant) -> i64 {
    let w = wall_ms(now);
    if t >= now.mono {
        w.saturating_add((t - now.mono).as_millis().min(i64::MAX as u128 / 2) as i64)
    } else {
        w.saturating_sub((now.mono - t).as_millis().min(i64::MAX as u128 / 2) as i64)
    }
}

/// The monotonic moment a time on the wall clock stands for now. A moment further back than the clock reaches (the
/// machine started later) is as far back as it goes.
fn from_wall_ms(now: &Now, ms: i64) -> Instant {
    let w = wall_ms(now);
    if ms >= w {
        return now.mono + Duration::from_millis((ms - w).min(10 * 365 * 86_400_000) as u64);
    }
    let mut back = Duration::from_millis((w - ms) as u64);
    loop {
        if let Some(t) = now.mono.checked_sub(back) {
            return t;
        }
        back /= 2;
        if back.is_zero() {
            return now.mono;
        }
    }
}

#[derive(Serialize, serde::Deserialize)]
pub struct EntrySnap {
    seq: u64,
    event: Value,
    enqueued_ms: i64,
    delivery_count: u32,
    /// Waiting for a release delay until this time.
    not_before_ms: Option<i64>,
    /// Locked by a receiver: the token and when the lock ends. The token stays good after a restart until then.
    lock: Option<(String, i64)>,
}

/// A queue as it is written to a file: times are on the wall clock.
#[derive(Serialize, serde::Deserialize, Default)]
pub struct QueueSnap {
    next_seq: u64,
    entries: Vec<EntrySnap>,
    dead: Vec<DeadLetter>,
    dead_sizes: Vec<u64>,
    total_enqueued: u64,
    total_acknowledged: u64,
}

impl Queue {
    pub fn export(&self, now: &Now) -> QueueSnap {
        QueueSnap {
            next_seq: self.next_seq,
            entries: self
                .entries
                .iter()
                .map(|(seq, e)| EntrySnap {
                    seq: *seq,
                    event: e.event.clone(),
                    enqueued_ms: to_wall_ms(now, e.enqueued),
                    delivery_count: e.delivery_count,
                    not_before_ms: match &e.state {
                        State::Ready { not_before } if *not_before > now.mono => Some(to_wall_ms(now, *not_before)),
                        _ => None,
                    },
                    lock: match &e.state {
                        State::Locked { token, until } => Some((token.clone(), to_wall_ms(now, *until))),
                        _ => None,
                    },
                })
                .collect(),
            dead: self.dead.clone(),
            dead_sizes: self.dead_sizes.iter().copied().collect(),
            total_enqueued: self.total_enqueued,
            total_acknowledged: self.total_acknowledged,
        }
    }

    pub fn import(snap: QueueSnap, now: &Now) -> Result<Queue, String> {
        let mut q = Queue { total_enqueued: snap.total_enqueued, total_acknowledged: snap.total_acknowledged, ..Queue::default() };
        let mut max_seq = 0;
        for e in snap.entries {
            max_seq = max_seq.max(e.seq.saturating_add(1));
            let state = match e.lock {
                Some((token, until_ms)) => {
                    q.locks.insert(token.clone(), e.seq);
                    q.issued.insert(token.clone());
                    State::Locked { token, until: from_wall_ms(now, until_ms) }
                }
                None => State::Ready { not_before: e.not_before_ms.map(|ms| from_wall_ms(now, ms)).unwrap_or(now.mono) },
            };
            let entry = Entry { event: e.event, enqueued: from_wall_ms(now, e.enqueued_ms), delivery_count: e.delivery_count, state };
            if q.entries.insert(e.seq, entry).is_some() {
                return Err(format!("event {} is listed twice", e.seq));
            }
        }
        q.next_seq = snap.next_seq.max(max_seq);
        if snap.dead.len() != snap.dead_sizes.len() {
            return Err("the dead letters and their sizes do not match".into());
        }
        q.dead_bytes = snap.dead_sizes.iter().sum();
        q.dead = snap.dead;
        q.dead_sizes = snap.dead_sizes.into_iter().collect();
        Ok(q)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(lock_ms: u64, max: u32, ttl_ms: u64) -> PullParams {
        PullParams { lock_duration: Duration::from_millis(lock_ms), max_delivery: max, ttl: Duration::from_millis(ttl_ms) }
    }

    fn at(base: &Now, ms: u64) -> Now {
        Now { mono: base.mono + Duration::from_millis(ms), wall: base.wall }
    }

    fn q_with(n: usize, base: &Now) -> Queue {
        let mut q = Queue::default();
        for i in 0..n {
            q.enqueue(json!({"id": i.to_string()}), base);
        }
        q
    }

    #[test]
    fn fifo_and_locking() {
        let t0 = Now::now();
        let p = params(1000, 10, 100_000);
        let mut q = q_with(3, &t0);
        let r = q.receive(2, &t0, &p);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].event["id"], "0");
        assert_eq!(r[1].event["id"], "1");
        assert_eq!(r[0].delivery_count, 1);
        // locked events are not offered again
        let r2 = q.receive(10, &t0, &p);
        assert_eq!(r2.len(), 1);
        assert_eq!(r2[0].event["id"], "2");
        assert!(q.receive(10, &t0, &p).is_empty());
    }

    #[test]
    fn ack_removes_and_reuse_is_token_lost() {
        let t0 = Now::now();
        let p = params(1000, 10, 100_000);
        let mut q = q_with(1, &t0);
        let tok = q.receive(1, &t0, &p).remove(0).token;
        let res = q.acknowledge(&[tok.clone(), "bogus".into()], &t0, &p);
        assert_eq!(res.succeeded, vec![tok.clone()]);
        assert_eq!(res.failed.len(), 1);
        assert_eq!(res.failed[0].1, "BadToken");
        let again = q.acknowledge(&[tok], &t0, &p);
        assert_eq!(again.failed[0].1, "TokenLost");
        assert!(q.receive(1, &at(&t0, 5000), &p).is_empty());
    }

    #[test]
    fn lock_expiry_redelivers_with_higher_count() {
        let t0 = Now::now();
        let p = params(1000, 10, 100_000);
        let mut q = q_with(1, &t0);
        let first = q.receive(1, &t0, &p).remove(0);
        assert!(q.receive(1, &at(&t0, 999), &p).is_empty());
        let second = q.receive(1, &at(&t0, 1001), &p).remove(0);
        assert_eq!(second.delivery_count, 2);
        assert_ne!(first.token, second.token);
        // the old token no longer works
        let res = q.acknowledge(&[first.token], &at(&t0, 1002), &p);
        assert_eq!(res.failed[0].1, "TokenLost");
        let res = q.acknowledge(&[second.token], &at(&t0, 1002), &p);
        assert_eq!(res.succeeded.len(), 1);
    }

    #[test]
    fn max_delivery_count_dead_letters() {
        let t0 = Now::now();
        let p = params(100, 2, 100_000);
        let mut q = q_with(1, &t0);
        assert_eq!(q.receive(1, &t0, &p)[0].delivery_count, 1);
        assert_eq!(q.receive(1, &at(&t0, 200), &p)[0].delivery_count, 2);
        assert!(q.receive(1, &at(&t0, 400), &p).is_empty());
        assert_eq!(q.dead.len(), 1);
        assert_eq!(q.dead[0].delivery_count, 2);
        assert_eq!(q.dead[0].reason, "MaxDeliveryCountExceeded");
        assert_eq!(q.counts(&at(&t0, 400)), (0, 0, 0));
    }

    #[test]
    fn release_makes_available_again_with_optional_delay() {
        let t0 = Now::now();
        let p = params(10_000, 10, 100_000);
        let mut q = q_with(2, &t0);
        let r = q.receive(2, &t0, &p);
        let res = q.release(&[r[0].token.clone()], Duration::ZERO, &t0, &p);
        assert_eq!(res.succeeded.len(), 1);
        let again = q.receive(5, &t0, &p);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].event["id"], "0");
        assert_eq!(again[0].delivery_count, 2);
        let res = q.release(&[r[1].token.clone()], Duration::from_millis(500), &t0, &p);
        assert_eq!(res.succeeded.len(), 1);
        assert!(q.receive(5, &at(&t0, 100), &p).is_empty());
        assert_eq!(q.counts(&at(&t0, 100)).2, 1);
        assert_eq!(q.receive(5, &at(&t0, 600), &p).len(), 1);
    }

    #[test]
    fn release_at_max_delivery_dead_letters() {
        let t0 = Now::now();
        let p = params(10_000, 1, 100_000);
        let mut q = q_with(1, &t0);
        let tok = q.receive(1, &t0, &p).remove(0).token;
        q.release(&[tok], Duration::ZERO, &t0, &p);
        assert_eq!(q.dead.len(), 1);
        assert!(q.receive(1, &t0, &p).is_empty());
    }

    #[test]
    fn reject_dead_letters() {
        let t0 = Now::now();
        let p = params(10_000, 10, 100_000);
        let mut q = q_with(1, &t0);
        let tok = q.receive(1, &t0, &p).remove(0).token;
        let res = q.reject(&[tok], &t0, &p);
        assert_eq!(res.succeeded.len(), 1);
        assert_eq!(q.dead[0].reason, "Rejected");
        assert!(q.receive(1, &t0, &p).is_empty());
    }

    #[test]
    fn renew_extends_the_lock() {
        let t0 = Now::now();
        let p = params(1000, 10, 100_000);
        let mut q = q_with(1, &t0);
        let tok = q.receive(1, &t0, &p).remove(0).token;
        let res = q.renew(std::slice::from_ref(&tok), &at(&t0, 800), &p);
        assert_eq!(res.succeeded.len(), 1);
        // 1200 > original expiry (1000) but < renewed expiry (1800)
        assert!(q.receive(1, &at(&t0, 1200), &p).is_empty());
        assert_eq!(q.acknowledge(std::slice::from_ref(&tok), &at(&t0, 1200), &p).succeeded.len(), 1);
        // renewing a token that is gone fails
        assert_eq!(q.renew(&[tok], &at(&t0, 1300), &p).failed[0].1, "TokenLost");
        assert_eq!(q.renew(&["x".into()], &at(&t0, 1300), &p).failed[0].1, "BadToken");
    }

    #[test]
    fn ttl_expires_available_events_only() {
        let t0 = Now::now();
        let p = params(10_000, 10, 1000);
        let mut q = q_with(2, &t0);
        let _locked = q.receive(1, &t0, &p);
        // the unlocked one expires, the locked one is kept until its lock ends
        assert!(q.receive(5, &at(&t0, 1500), &p).is_empty());
        assert_eq!(q.dead.len(), 1);
        assert_eq!(q.dead[0].reason, "TimeToLiveExpired");
        assert_eq!(q.counts(&at(&t0, 1500)).1, 1);
    }

    #[test]
    fn next_wake_points_at_the_next_change() {
        let t0 = Now::now();
        let p = params(1000, 10, 50_000);
        let mut q = q_with(1, &t0);
        assert_eq!(q.next_wake(&t0, &p), Some(t0.mono + Duration::from_millis(50_000)));
        q.receive(1, &t0, &p);
        assert_eq!(q.next_wake(&t0, &p), Some(t0.mono + Duration::from_millis(1000)));
        assert_eq!(Queue::default().next_wake(&t0, &p), None);
    }
}
