//! Event hubs, partitions and the events in them.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use azemu_amqp::message::Message;

use crate::errors::EhError;
use crate::selector::StartPosition;

pub const DEFAULT_CONSUMER_GROUP: &str = "$Default";
/// Partitions of an event hub that was created by a client attaching to it.
pub const DEFAULT_PARTITIONS: u32 = 4;
/// Azure's default message retention is one day.
pub const DEFAULT_RETENTION_MS: i64 = 24 * 3600 * 1000;
pub const MAX_PARTITIONS: u32 = 1024;
/// Event hubs in one namespace (Azure: 10 on Standard, 100 per processing unit on Premium).
pub const MAX_HUBS_PER_NAMESPACE: usize = 1000;
/// Consumer groups of one event hub (Azure: 20 on Standard, 100 on Premium).
pub const MAX_CONSUMER_GROUPS: usize = 100;
/// Bytes of events one event hub keeps in memory, whatever the retention period says. Azure keeps
/// 84 GB per throughput unit; this is a development emulator in RAM. The oldest events of a
/// partition are dropped first, as if their retention had ended.
pub const MAX_RETAINED_BYTES_PER_HUB: usize = 512 * 1024 * 1024;
/// ...but every partition may keep at least this much.
const MIN_RETAINED_BYTES_PER_PARTITION: usize = 16 * 1024 * 1024;
/// 0001-01-01T00:00:00Z in milliseconds since 1970 (what Event Hubs reports for "never").
pub const NEVER_MS: i64 = -62_135_596_800_000;
/// Largest message on a link (Standard tier).
pub const MAX_MESSAGE_SIZE: u64 = 1_048_576;
/// What a stored event costs besides its encoded bytes: the `Event`, its `Message` with its optional parts, the
/// `Arc` and the queue slot. Charged for every event against the byte limit of its partition, so a million empty
/// events fill a partition the way they fill memory (the test `event_overhead_covers_the_memory_of_an_event` checks
/// the number against the sizes of the types).
pub const EVENT_OVERHEAD: usize = 576;
/// Events in one batch of one send (an AMQP batch message, a Kafka record batch). A batch is also limited in bytes
/// ([`MAX_MESSAGE_SIZE`]); this stops a batch of nothing but empty events, which is a million of them in 7 MB.
pub const MAX_EVENTS_PER_BATCH: usize = 50_000;
/// Added to the size of an event to get the offset of the next one, so offsets grow like byte positions.
const OFFSET_OVERHEAD: i64 = 32;

static REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A number nobody has had before in this process. Every change to something that is saved to disk takes one (see
/// `persist.rs`): the highest number handed out is the service's `persist_generation`, and a partition whose number
/// differs from the one at the last save has changed since. Numbers are never reused, so a partition that is deleted
/// and made again cannot look like the one that was saved.
pub fn next_revision() -> u64 {
    REVISION.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// The number the last change took.
pub fn current_revision() -> u64 {
    REVISION.load(std::sync::atomic::Ordering::SeqCst)
}

pub fn key(name: &str) -> String {
    name.to_lowercase()
}

/// One stored event. The broker annotations are not part of `message`; they are added on delivery.
#[derive(Debug)]
pub struct Event {
    pub seq: i64,
    pub offset: i64,
    pub enqueued_ms: i64,
    pub partition_key: Option<String>,
    pub message: Message,
    pub size: usize,
}

impl Event {
    /// What the event counts against the byte limit of its partition.
    pub fn charge(&self) -> usize {
        self.size + EVENT_OVERHEAD
    }
}

#[derive(Debug)]
pub struct Partition {
    pub events: VecDeque<Arc<Event>>,
    pub next_seq: i64,
    pub next_offset: i64,
    pub last_offset: Option<i64>,
    pub last_ms: Option<i64>,
    /// Events ever stored (not reduced by retention).
    pub total_events: u64,
    /// Bytes the events in `events` are charged (their sizes plus [`EVENT_OVERHEAD`] each).
    pub bytes_held: usize,
    /// Retained bytes above which the oldest events are dropped; 0 means no limit.
    pub max_bytes: usize,
    /// Kafka idempotent producers: what the partition remembers of each producer id (see `kafka::store`).
    pub producers: std::collections::HashMap<i64, crate::kafka::store::ProducerSeq>,
    /// Changes with every change to the events, the counters or the producers (see [`next_revision`]).
    pub rev: u64,
}

impl Default for Partition {
    fn default() -> Partition {
        Partition {
            events: VecDeque::new(),
            next_seq: 0,
            next_offset: 0,
            last_offset: None,
            last_ms: None,
            total_events: 0,
            bytes_held: 0,
            max_bytes: 0,
            producers: Default::default(),
            rev: next_revision(),
        }
    }
}

/// An event ready to store: stripped of what only the broker sets, and measured. Making one costs
/// time in proportion to the size of the event, so it is done before the shared lock is taken.
pub struct PreparedEvent {
    pub message: Message,
    /// The `x-opt-partition-key` the sender gave (routing), if any and not empty.
    pub route_key: Option<String>,
    partition_key: Option<String>,
    size: usize,
}

pub fn prepare(mut message: Message) -> PreparedEvent {
    let route_key = message_partition_key(&message);
    let partition_key = strip_broker_annotations(&mut message);
    let size = message.encode().len();
    PreparedEvent { message, route_key, partition_key, size }
}

impl Partition {
    pub fn first_seq(&self) -> i64 {
        self.events.front().map(|e| e.seq).unwrap_or(self.next_seq)
    }

    /// -1 for a partition that never had an event.
    pub fn last_seq(&self) -> i64 {
        self.next_seq - 1
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes_held
    }

    /// `begin_sequence_number` as Event Hubs reports it: -1 for a partition that never had an event.
    pub fn begin_seq_reported(&self) -> i64 {
        if self.next_seq == 0 {
            -1
        } else {
            self.first_seq()
        }
    }

    /// `last_enqueued_time_utc` as the management node reports it: 0001-01-01 for a partition that never had an event.
    pub fn last_time_reported(&self) -> i64 {
        self.last_ms.unwrap_or(NEVER_MS)
    }

    pub fn last_offset_string(&self) -> String {
        self.last_offset.map(|o| o.to_string()).unwrap_or_else(|| "-1".to_string())
    }

    pub fn append(&mut self, message: Message, now: i64) -> Arc<Event> {
        self.append_prepared(prepare(message), now)
    }

    pub fn append_prepared(&mut self, p: PreparedEvent, now: i64) -> Arc<Event> {
        let PreparedEvent { message, partition_key, size, .. } = p;
        let enqueued_ms = now.max(self.last_ms.unwrap_or(0));
        let ev = Arc::new(Event { seq: self.next_seq, offset: self.next_offset, enqueued_ms, partition_key, message, size });
        self.next_seq += 1;
        self.next_offset += size as i64 + OFFSET_OVERHEAD;
        self.last_offset = Some(ev.offset);
        self.last_ms = Some(enqueued_ms);
        self.total_events += 1;
        self.events.push_back(ev.clone());
        self.rev = next_revision();
        self.bytes_held += size + EVENT_OVERHEAD;
        // keep at least the newest event, so a receiver at the end always finds it
        while self.max_bytes > 0 && self.bytes_held > self.max_bytes && self.events.len() > 1 {
            if let Some(old) = self.events.pop_front() {
                self.bytes_held -= old.charge();
            }
        }
        ev
    }

    /// Lazy retention: drop what is older than the retention period. Called by every operation.
    pub fn trim(&mut self, now: i64, retention_ms: i64) {
        while let Some(front) = self.events.front() {
            if front.enqueued_ms.saturating_add(retention_ms) <= now {
                self.bytes_held -= front.charge();
                self.events.pop_front();
                self.rev = next_revision();
            } else {
                break;
            }
        }
    }

    pub fn get(&self, seq: i64) -> Option<Arc<Event>> {
        let first = self.events.front()?.seq;
        if seq < first {
            return None;
        }
        self.events.get((seq - first) as usize).cloned()
    }

    /// The sequence number of the first event a receiver with this start position gets. It may
    /// be the next sequence number to be assigned (nothing there yet).
    pub fn cursor_for(&self, pos: StartPosition) -> Result<i64, EhError> {
        let first = self.first_seq();
        Ok(match pos {
            StartPosition::Beginning => first,
            StartPosition::Latest => self.next_seq,
            StartPosition::Offset { value, inclusive } => {
                let idx = self.events.partition_point(|e| if inclusive { e.offset < value } else { e.offset <= value });
                first + idx as i64
            }
            StartPosition::Time { ms, inclusive } => {
                let idx = self.events.partition_point(|e| if inclusive { e.enqueued_ms < ms } else { e.enqueued_ms <= ms });
                first + idx as i64
            }
            StartPosition::Sequence { value, inclusive } => {
                let c = if inclusive { value } else { value.saturating_add(1) };
                if c > self.next_seq {
                    return Err(EhError::out_of_range(format!(
                        "The supplied sequence number '{value}' is invalid. The last sequence number in the system is '{}'. TrackingId:{}",
                        self.last_seq(),
                        crate::errors::tracking_id()
                    )));
                }
                c.max(first)
            }
        })
    }
}

/// Remove what only the broker may set; return the partition key.
fn strip_broker_annotations(m: &mut Message) -> Option<String> {
    let mut key = None;
    if let Some(ann) = m.message_annotations.as_mut() {
        ann.retain(|(k, _)| !matches!(k.as_str(), Some("x-opt-sequence-number" | "x-opt-offset" | "x-opt-enqueued-time")));
        key = ann.iter().find(|(k, _)| k.as_str() == Some("x-opt-partition-key")).and_then(|(_, v)| match v {
            azemu_amqp::Value::Binary(b) => Some(String::from_utf8_lossy(b).into_owned()),
            other => other.as_str().map(str::to_string),
        });
        if ann.is_empty() {
            m.message_annotations = None;
        }
    }
    key
}

/// The partition a partition key goes to, as Azure's service chooses it. Microsoft's SDKs carry the same function so
/// that a buffered producer picks the partition the service would (JavaScript: `@azure/event-hubs`,
/// `impl/partitionKeyToIdMapper.js`; .NET: `PartitionResolver`): Bob Jenkins' lookup3 (`hashlittle2`) over the UTF-8
/// bytes of the key with both seeds 0, the two result words `c` and `b` combined with XOR and cut to 16 bits as a
/// signed number, and the absolute value of that modulo the number of partitions.
pub fn partition_for_key(key: &str, partitions: usize) -> usize {
    if partitions == 0 {
        return 0;
    }
    (hash_key(key) as i32 % partitions as i32).unsigned_abs() as usize
}

/// The 16-bit hash of a partition key (`hashPartitionKey` in the JavaScript SDK).
pub fn hash_key(key: &str) -> i16 {
    let (b, c) = lookup3(key.as_bytes());
    (c ^ b) as u16 as i16
}

fn read_u32(d: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]])
}

/// lookup3 as written in the JavaScript SDK (`computeHash`): returns `(b, c)`.
///
/// The algorithm is Bob Jenkins' lookup3 (`hashlittle2`, public domain, https://burtleburtle.net/bob/c/lookup3.c). Microsoft's
/// Event Hubs SDKs (JavaScript, .NET `PartitionResolver`, Rust `azure_messaging_eventhubs`; MIT, Copyright Microsoft) carry the same
/// function so that a client picks the partition the service picks; this version follows them. The mixing steps are
/// the ones lookup3 defines and so read the same in every port (they match the Rust SDK's line for line). See NOTICE.
fn lookup3(data: &[u8]) -> (u32, u32) {
    let init = 0xdead_beef_u32.wrapping_add(data.len() as u32);
    let (mut a, mut b, mut c) = (init, init, init);
    let mut index = 0;
    let mut size = data.len();
    while size > 12 {
        a = a.wrapping_add(read_u32(data, index));
        b = b.wrapping_add(read_u32(data, index + 4));
        c = c.wrapping_add(read_u32(data, index + 8));
        a = a.wrapping_sub(c);
        a ^= c.rotate_left(4);
        c = c.wrapping_add(b);
        b = b.wrapping_sub(a);
        b ^= a.rotate_left(6);
        a = a.wrapping_add(c);
        c = c.wrapping_sub(b);
        c ^= b.rotate_left(8);
        b = b.wrapping_add(a);
        a = a.wrapping_sub(c);
        a ^= c.rotate_left(16);
        c = c.wrapping_add(b);
        b = b.wrapping_sub(a);
        b ^= a.rotate_left(19);
        a = a.wrapping_add(c);
        c = c.wrapping_sub(b);
        c ^= b.rotate_left(4);
        b = b.wrapping_add(a);
        index += 12;
        size -= 12;
    }
    let byte = |i: usize| data[index + i] as u32;
    match size {
        0 => return (b, c),
        1 => a = a.wrapping_add(byte(0)),
        2 => a = a.wrapping_add(byte(0) | byte(1) << 8),
        3 => a = a.wrapping_add(byte(0) | byte(1) << 8 | byte(2) << 16),
        4 => a = a.wrapping_add(read_u32(data, index)),
        5 => {
            a = a.wrapping_add(read_u32(data, index));
            b = b.wrapping_add(byte(4));
        }
        6 => {
            a = a.wrapping_add(read_u32(data, index));
            b = b.wrapping_add(byte(4) | byte(5) << 8);
        }
        7 => {
            a = a.wrapping_add(read_u32(data, index));
            b = b.wrapping_add(byte(4) | byte(5) << 8 | byte(6) << 16);
        }
        8 => {
            a = a.wrapping_add(read_u32(data, index));
            b = b.wrapping_add(read_u32(data, index + 4));
        }
        9 => {
            a = a.wrapping_add(read_u32(data, index));
            b = b.wrapping_add(read_u32(data, index + 4));
            c = c.wrapping_add(byte(8));
        }
        10 => {
            a = a.wrapping_add(read_u32(data, index));
            b = b.wrapping_add(read_u32(data, index + 4));
            c = c.wrapping_add(byte(8) | byte(9) << 8);
        }
        11 => {
            a = a.wrapping_add(read_u32(data, index));
            b = b.wrapping_add(read_u32(data, index + 4));
            c = c.wrapping_add(byte(8) | byte(9) << 8 | byte(10) << 16);
        }
        _ => {
            a = a.wrapping_add(read_u32(data, index));
            b = b.wrapping_add(read_u32(data, index + 4));
            c = c.wrapping_add(read_u32(data, index + 8));
        }
    }
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(14));
    a ^= c;
    a = a.wrapping_sub(c.rotate_left(11));
    b ^= a;
    b = b.wrapping_sub(a.rotate_left(25));
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(16));
    a ^= c;
    a = a.wrapping_sub(c.rotate_left(4));
    b ^= a;
    b = b.wrapping_sub(a.rotate_left(14));
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(24));
    (b, c)
}

#[derive(Debug)]
pub struct Hub {
    pub name: String,
    /// Unique per creation, so a receiver can tell the hub it attached to from a new one of the same name.
    pub id: u64,
    pub created_ms: i64,
    pub partitions: Vec<Partition>,
    pub retention_ms: i64,
    /// lower-case name to name as created
    pub consumer_groups: BTreeMap<String, String>,
    pub autocreated: bool,
    rr: usize,
}

impl Hub {
    pub fn new(name: &str, id: u64, partitions: u32, retention_ms: i64, now: i64) -> Hub {
        let mut consumer_groups = BTreeMap::new();
        consumer_groups.insert(key(DEFAULT_CONSUMER_GROUP), DEFAULT_CONSUMER_GROUP.to_string());
        Hub {
            name: name.to_string(),
            id,
            created_ms: now,
            partitions: (0..partitions)
                .map(|_| Partition {
                    max_bytes: (MAX_RETAINED_BYTES_PER_HUB / partitions.max(1) as usize).max(MIN_RETAINED_BYTES_PER_PARTITION),
                    ..Default::default()
                })
                .collect(),
            retention_ms,
            consumer_groups,
            autocreated: false,
            rr: 0,
        }
    }

    pub fn partition_ids(&self) -> Vec<String> {
        (0..self.partitions.len()).map(|i| i.to_string()).collect()
    }

    pub fn parse_pid(&self, pid: &str) -> Result<usize, EhError> {
        pid.trim().parse::<usize>().ok().filter(|p| *p < self.partitions.len()).ok_or_else(|| EhError::bad_partition(pid))
    }

    pub fn has_consumer_group(&self, cg: &str) -> bool {
        self.consumer_groups.contains_key(&key(cg))
    }

    pub fn add_consumer_group(&mut self, cg: &str) -> bool {
        let k = key(cg);
        if self.consumer_groups.contains_key(&k) {
            return false;
        }
        self.consumer_groups.insert(k, cg.to_string());
        next_revision();
        true
    }

    /// Partition for an event without a partition id: by key hash, else round-robin.
    pub fn route(&mut self, partition_key: Option<&str>) -> usize {
        match partition_key {
            Some(k) => partition_for_key(k, self.partitions.len()),
            None => self.next_round_robin(),
        }
    }

    pub fn next_round_robin(&mut self) -> usize {
        let p = self.rr % self.partitions.len();
        self.rr = self.rr.wrapping_add(1);
        p
    }

    /// The counter the round robin of events without a partition goes by (saved, so the order goes on).
    pub fn round_robin(&self) -> usize {
        self.rr
    }

    pub fn set_round_robin(&mut self, rr: usize) {
        self.rr = rr;
    }

    pub fn trim_all(&mut self, now: i64) {
        let r = self.retention_ms;
        for p in &mut self.partitions {
            p.trim(now, r);
        }
    }
}

/// The partition key annotation of a message, if it has one.
pub fn message_partition_key(m: &Message) -> Option<String> {
    let v = m.annotation("x-opt-partition-key")?;
    match v {
        azemu_amqp::Value::Binary(b) => Some(String::from_utf8_lossy(b).into_owned()),
        other => other.as_str().map(str::to_string),
    }
    .filter(|k| !k.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_overhead_covers_the_memory_of_an_event() {
        // the event itself, its Arc counts, and the pointer in the queue
        let per_event = std::mem::size_of::<Event>() + 2 * std::mem::size_of::<usize>() + std::mem::size_of::<Arc<Event>>();
        assert!(per_event <= EVENT_OVERHEAD, "an event takes {per_event} bytes besides its data: raise EVENT_OVERHEAD");
    }

    #[test]
    fn every_event_is_charged_its_overhead_whatever_it_holds() {
        let mut p = Partition { max_bytes: 100 * EVENT_OVERHEAD, ..Default::default() };
        for _ in 0..1000 {
            p.append(Message::default(), 1);
        }
        assert!(p.bytes() <= 100 * EVENT_OVERHEAD, "{} bytes charged", p.bytes());
        assert_eq!(p.events.len(), 100, "empty events fill a partition by their overhead");
        assert_eq!(p.first_seq(), 900);
        p.trim(i64::MAX, 1);
        assert_eq!(p.bytes(), 0);
    }
}
