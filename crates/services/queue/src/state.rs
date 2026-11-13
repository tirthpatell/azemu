//! Everything the service remembers, and the rules for message visibility and expiry.
//!
//! Nothing runs on a timer. Every message stores the moment it becomes visible and
//! the moment it expires; each operation looks at the request time and decides.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};

use azemu_core::time::{Date, Duration, Month, OffsetDateTime, PrimitiveDateTime, Time};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::props::ServiceProps;

pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub const DEFAULT_TTL_SECS: i64 = 7 * 24 * 3600;
pub const MAX_VISIBILITY_SECS: i64 = 7 * 24 * 3600;

/// What `messagettl=-1` means: the last second of year 9999.
pub fn never_expires() -> OffsetDateTime {
    let date = Date::from_calendar_date(9999, Month::December, 31).unwrap_or(Date::MAX);
    let time = Time::from_hms(23, 59, 59).unwrap_or(Time::MIDNIGHT);
    PrimitiveDateTime::new(date, time).assume_utc()
}

#[derive(Default)]
pub struct State {
    pub accounts: HashMap<String, Account>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Account {
    pub queues: BTreeMap<String, Queue>,
    pub props: ServiceProps,
}

#[derive(Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedId {
    pub id: String,
    /// Kept as sent, so that "Permission" comes back exactly as stored.
    pub start: Option<String>,
    pub expiry: Option<String>,
    pub permission: Option<String>,
    /// True when the request had an AccessPolicy element.
    pub has_policy: bool,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Queue {
    /// (name as sent, value), in the order given.
    pub metadata: Vec<(String, String)>,
    pub acl: Vec<SignedId>,
    /// Insertion order. Visible messages are handed out front to back.
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub text: String,
    pub inserted: OffsetDateTime,
    pub expires: OffsetDateTime,
    pub next_visible: OffsetDateTime,
    pub dequeue_count: u32,
    pub pop_receipt: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MessageError {
    NotFound,
    PopReceiptMismatch,
}

static RECEIPT_COUNTER: AtomicU32 = AtomicU32::new(1);

/// A pop receipt looks like the ones Azure hands out: base64 of a small binary
/// record (version, a counter, a Windows FILETIME of when it was made). The counter
/// makes every receipt unique.
pub fn new_pop_receipt(version: u8, now: OffsetDateTime) -> String {
    let n = RECEIPT_COUNTER.fetch_add(1, Ordering::Relaxed);
    // FILETIME: 100 ns ticks since 1601-01-01. Unix epoch is 11644473600 s later.
    let nanos = now.unix_timestamp_nanos();
    let ticks = (nanos / 100 + 11_644_473_600i128 * 10_000_000).max(0) as u64;
    let mut raw = Vec::with_capacity(24);
    raw.extend_from_slice(&[version, 0, 0, 0, 3, 0, 0, 0]);
    raw.extend_from_slice(&n.to_le_bytes());
    raw.extend_from_slice(&ticks.to_le_bytes());
    if version == 3 {
        raw.extend_from_slice(&[1, 0, 0, 0]);
    }
    B64.encode(raw)
}

impl Message {
    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        self.expires <= now
    }

    pub fn is_visible(&self, now: OffsetDateTime) -> bool {
        self.next_visible <= now && !self.is_expired(now)
    }
}

impl Queue {
    /// Forget messages whose time to live is over.
    pub fn purge_expired(&mut self, now: OffsetDateTime) {
        self.messages.retain(|m| !m.is_expired(now));
    }

    /// Messages that are neither expired nor deleted (hidden ones included).
    pub fn approximate_count(&mut self, now: OffsetDateTime) -> usize {
        self.purge_expired(now);
        self.messages.len()
    }

    pub fn put(&mut self, text: String, visibility_secs: i64, ttl_secs: i64, now: OffsetDateTime) -> Message {
        self.purge_expired(now);
        let expires = expiry_after(now, ttl_secs);
        let msg = Message {
            id: uuid::Uuid::new_v4().to_string(),
            text,
            inserted: now,
            expires,
            next_visible: now + Duration::seconds(visibility_secs),
            dequeue_count: 0,
            pop_receipt: new_pop_receipt(2, now),
        };
        self.messages.push(msg.clone());
        msg
    }

    /// Hand out up to `count` visible messages and hide them for `visibility_secs`.
    pub fn dequeue(&mut self, count: usize, visibility_secs: i64, now: OffsetDateTime) -> Vec<Message> {
        self.purge_expired(now);
        let hidden_until = now + Duration::seconds(visibility_secs);
        let mut out = Vec::new();
        for m in self.messages.iter_mut().filter(|m| m.is_visible(now)).take(count) {
            m.dequeue_count += 1;
            m.next_visible = hidden_until;
            m.pop_receipt = new_pop_receipt(2, now);
            out.push(m.clone());
        }
        out
    }

    /// Look at up to `count` visible messages without changing them.
    pub fn peek(&mut self, count: usize, now: OffsetDateTime) -> Vec<Message> {
        self.purge_expired(now);
        self.messages.iter().filter(|m| m.is_visible(now)).take(count).cloned().collect()
    }

    fn find_checked(&mut self, id: &str, pop_receipt: &str, now: OffsetDateTime) -> Result<usize, MessageError> {
        self.purge_expired(now);
        let idx = self.messages.iter().position(|m| m.id == id).ok_or(MessageError::NotFound)?;
        if self.messages[idx].pop_receipt != pop_receipt {
            return Err(MessageError::PopReceiptMismatch);
        }
        Ok(idx)
    }

    /// Change when a message becomes visible and, optionally, its text. Returns the
    /// new pop receipt and the new visibility time.
    pub fn update(
        &mut self,
        id: &str,
        pop_receipt: &str,
        new_text: Option<String>,
        visibility_secs: i64,
        now: OffsetDateTime,
    ) -> Result<(String, OffsetDateTime), MessageError> {
        let idx = self.find_checked(id, pop_receipt, now)?;
        let m = &mut self.messages[idx];
        m.next_visible = now + Duration::seconds(visibility_secs);
        if let Some(t) = new_text {
            m.text = t;
        }
        m.pop_receipt = new_pop_receipt(3, now);
        Ok((m.pop_receipt.clone(), m.next_visible))
    }

    pub fn delete(&mut self, id: &str, pop_receipt: &str, now: OffsetDateTime) -> Result<(), MessageError> {
        let idx = self.find_checked(id, pop_receipt, now)?;
        self.messages.remove(idx);
        Ok(())
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }
}

/// `now` plus a time to live in seconds; -1 or anything that would pass year 9999
/// is "never".
pub fn expiry_after(now: OffsetDateTime, ttl_secs: i64) -> OffsetDateTime {
    let max = never_expires();
    if ttl_secs < 0 {
        return max;
    }
    let room = (max - now).whole_seconds();
    if ttl_secs >= room {
        max
    } else {
        now + Duration::seconds(ttl_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use azemu_core::time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-01-01 12:00:00 UTC);

    fn at(secs: i64) -> OffsetDateTime {
        T0 + Duration::seconds(secs)
    }

    #[test]
    fn hidden_until_visibility_timeout_then_back_with_higher_count() {
        let mut q = Queue::default();
        q.put("a".into(), 0, DEFAULT_TTL_SECS, T0);
        let got = q.dequeue(1, 30, at(1));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].dequeue_count, 1);
        assert!(q.dequeue(1, 30, at(30)).is_empty(), "still hidden 29 s later");
        let again = q.dequeue(1, 30, at(31));
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].dequeue_count, 2);
        assert_ne!(again[0].pop_receipt, got[0].pop_receipt);
    }

    #[test]
    fn initial_visibility_delay_hides_new_message() {
        let mut q = Queue::default();
        q.put("later".into(), 60, DEFAULT_TTL_SECS, T0);
        assert!(q.peek(10, at(59)).is_empty());
        assert_eq!(q.peek(10, at(60)).len(), 1);
    }

    #[test]
    fn expired_messages_are_never_returned_or_counted() {
        let mut q = Queue::default();
        q.put("short".into(), 0, 10, T0);
        q.put("long".into(), 0, 100, T0);
        assert_eq!(q.approximate_count(at(9)), 2);
        assert_eq!(q.approximate_count(at(10)), 1, "expiry is exclusive of the expiry second");
        let got = q.dequeue(10, 30, at(11));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "long");
    }

    #[test]
    fn hidden_message_expires_even_while_invisible() {
        let mut q = Queue::default();
        q.put("x".into(), 0, 20, T0);
        q.dequeue(1, 3600, at(1));
        assert_eq!(q.approximate_count(at(5)), 1, "hidden messages are counted");
        assert_eq!(q.approximate_count(at(20)), 0);
    }

    #[test]
    fn fifo_among_visible_and_peek_does_not_change_anything() {
        let mut q = Queue::default();
        for i in 0..5 {
            q.put(format!("m{i}"), 0, DEFAULT_TTL_SECS, at(i));
        }
        let peeked = q.peek(3, at(10));
        assert_eq!(peeked.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), ["m0", "m1", "m2"]);
        assert!(peeked.iter().all(|m| m.dequeue_count == 0));
        let got = q.dequeue(2, 30, at(10));
        assert_eq!(got.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), ["m0", "m1"]);
        let rest = q.dequeue(10, 30, at(11));
        assert_eq!(rest.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), ["m2", "m3", "m4"]);
    }

    #[test]
    fn update_and_delete_need_the_current_receipt() {
        let mut q = Queue::default();
        let put = q.put("x".into(), 0, DEFAULT_TTL_SECS, T0);
        let got = q.dequeue(1, 30, at(1)).remove(0);
        assert_eq!(q.delete(&put.id, &put.pop_receipt, at(2)), Err(MessageError::PopReceiptMismatch));
        let (receipt, visible) = q.update(&got.id, &got.pop_receipt, Some("y".into()), 0, at(2)).unwrap();
        assert_eq!(visible, at(2));
        assert_eq!(q.delete(&got.id, &got.pop_receipt, at(3)), Err(MessageError::PopReceiptMismatch));
        assert_eq!(q.delete("nope", &receipt, at(3)), Err(MessageError::NotFound));
        assert_eq!(q.delete(&got.id, &receipt, at(3)), Ok(()));
        assert_eq!(q.approximate_count(at(3)), 0);
    }

    #[test]
    fn update_keeps_dequeue_count_and_replaces_text() {
        let mut q = Queue::default();
        q.put("old".into(), 0, DEFAULT_TTL_SECS, T0);
        let got = q.dequeue(1, 30, at(1)).remove(0);
        q.update(&got.id, &got.pop_receipt, Some("new".into()), 0, at(2)).unwrap();
        let again = q.dequeue(1, 30, at(3)).remove(0);
        assert_eq!(again.text, "new");
        assert_eq!(again.dequeue_count, 2);
    }

    #[test]
    fn ttl_minus_one_and_huge_ttl_cap_at_year_9999() {
        assert_eq!(expiry_after(T0, -1), never_expires());
        assert_eq!(expiry_after(T0, i64::MAX), never_expires());
        assert_eq!(expiry_after(T0, 60), at(60));
    }

    #[test]
    fn pop_receipts_are_unique_base64() {
        let a = new_pop_receipt(2, T0);
        let b = new_pop_receipt(2, T0);
        assert_ne!(a, b);
        assert!(B64.decode(&a).is_ok());
        assert!(new_pop_receipt(3, T0).len() > a.len());
    }
}
