//! What the sink keeps: two bounded lists (telemetry envelopes, Logs Ingestion rows), oldest first.
//!
//! Telemetry is noise by nature, so each list holds a fixed number of items and a fixed number of bytes and drops the
//! oldest when either is passed. The number of items dropped is kept and shown by the admin API. Nothing is written
//! to the data directory.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use serde_json::value::RawValue;

/// One stored item with the sequence number it was given when it arrived.
pub(crate) struct Entry<T> {
    pub seq: u64,
    /// The size counted against the byte limit (the item's own JSON text).
    bytes: usize,
    pub value: T,
}

struct Inner<T> {
    items: VecDeque<Entry<T>>,
    bytes: usize,
    dropped: u64,
}

/// A bounded list. Sequence numbers rise for ever, also across `clear`, so a test that noted `lastSequence` can ask for
/// "what came after" whatever happened in between.
pub(crate) struct Ring<T> {
    max_items: usize,
    max_bytes: usize,
    next: AtomicU64,
    inner: Mutex<Inner<T>>,
}

/// What a read of the list saw, taken under one lock.
pub(crate) struct Snapshot {
    pub stored: usize,
    pub dropped: u64,
    pub last_sequence: u64,
}

impl<T> Ring<T> {
    pub fn new(max_items: usize, max_bytes: usize) -> Ring<T> {
        Ring {
            max_items: max_items.max(1),
            max_bytes: max_bytes.max(1),
            next: AtomicU64::new(1),
            inner: Mutex::new(Inner { items: VecDeque::new(), bytes: 0, dropped: 0 }),
        }
    }

    /// Add items (value and size in bytes) in order; returns how many older items were dropped to make room.
    pub fn push_all(&self, items: Vec<(T, usize)>) -> u64 {
        let mut g = self.inner.lock();
        let before = g.dropped;
        for (value, bytes) in items {
            // the sequence is taken under the lock, so the list is always in sequence order
            let seq = self.next.fetch_add(1, Ordering::SeqCst);
            g.bytes += bytes;
            g.items.push_back(Entry { seq, bytes, value });
            while g.items.len() > self.max_items || (g.bytes > self.max_bytes && g.items.len() > 1) {
                if let Some(old) = g.items.pop_front() {
                    g.bytes -= old.bytes;
                    g.dropped += 1;
                }
            }
        }
        g.dropped - before
    }

    pub fn clear(&self) {
        let mut g = self.inner.lock();
        g.items.clear();
        g.bytes = 0;
        g.dropped = 0;
    }

    pub fn snapshot(&self) -> Snapshot {
        let g = self.inner.lock();
        Snapshot { stored: g.items.len(), dropped: g.dropped, last_sequence: self.next.load(Ordering::SeqCst) - 1 }
    }

    /// Run `f` over the items in order, under the lock. Keep `f` short.
    pub fn with_items<R>(&self, f: impl FnOnce(&VecDeque<Entry<T>>, Snapshot) -> R) -> R {
        let g = self.inner.lock();
        let snap = Snapshot { stored: g.items.len(), dropped: g.dropped, last_sequence: self.next.load(Ordering::SeqCst) - 1 };
        f(&g.items, snap)
    }
}

/// A telemetry envelope as kept.
#[derive(Debug)]
pub(crate) struct Telemetry {
    pub received: String,
    pub ikey: String,
    /// `data.baseType` (`RequestData`, `MetricData`, ...); empty when the envelope has none.
    pub base_type: String,
    /// The envelope's own `name` (`Microsoft.ApplicationInsights.<key>.Request`).
    pub envelope_name: String,
    /// `data.baseData.name` when there is one, and the names of the metrics of a `MetricData`.
    pub names: Vec<String>,
    pub operation_id: Option<String>,
    /// The envelope as compact JSON text. A parsed `Value` takes about thirty times the memory of its text, and the limits
    /// count what is held, so it is kept as text and read when a test asks for it.
    pub envelope: Box<RawValue>,
}

impl Telemetry {
    /// What this item holds, counted against the byte limit of the list: the text and the strings beside it, and a fixed amount
    /// for the structure itself.
    pub fn held_bytes(&self) -> usize {
        self.envelope.get().len()
            + self.received.len()
            + self.ikey.len()
            + self.base_type.len()
            + self.envelope_name.len()
            + self.names.iter().map(|n| n.len() + 24).sum::<usize>()
            + self.operation_id.as_ref().map_or(0, String::len)
            + ITEM_OVERHEAD
    }
}

/// The part of an item that is not its text: the structure, the entry of the list, the allocations' own overhead.
const ITEM_OVERHEAD: usize = 256;

/// A Logs Ingestion row as kept.
pub(crate) struct LogRow {
    pub received: String,
    pub dcr: String,
    pub stream: String,
    /// The row as compact JSON text (see [`Telemetry::envelope`]).
    pub row: Box<RawValue>,
}

impl LogRow {
    /// What this row holds, counted against the byte limit of the list.
    pub fn held_bytes(&self) -> usize {
        self.row.get().len() + self.received.len() + self.dcr.len() + self.stream.len() + ITEM_OVERHEAD
    }
}
