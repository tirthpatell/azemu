//! Fetch and ListOffsets. A fetch that finds no data waits (up to `MaxWaitMs`) for a notification from the store and
//! looks again; it holds no lock while it waits.

use std::sync::Arc;
use std::time::Duration;

use azemu_kafka::api::err;
use azemu_kafka::records::{encode_batch, Batch};
use azemu_kafka::{Struct, Value};
use bytes::Bytes;
use tokio::time::Instant;

use super::convert::event_to_record;
use super::handlers::{Broker, Conn};
use super::MAX_FETCH_BYTES;
use crate::model::Event;
use crate::state::now_ms;

/// Most events one partition returns in one answer.
const MAX_EVENTS_PER_PARTITION: usize = 20_000;
/// Longest a fetch waits (Kafka's `fetch.max.wait.ms` of the client is capped by the broker too).
const MAX_WAIT_MS: i32 = 30_000;
/// What a record costs beyond its contents, for counting bytes.
const RECORD_OVERHEAD: usize = 32;

/// What one fetch asks of a topic: (partition, fetch offset, max bytes) each.
type Wanted = Vec<(String, Vec<(i32, i64, usize)>)>;

struct PartRaw {
    index: i32,
    error: i16,
    high: i64,
    start: i64,
    events: Vec<Arc<Event>>,
}

struct Round {
    topics: Vec<(String, Vec<PartRaw>)>,
    bytes: usize,
    any_error: bool,
}

impl Broker {
    pub(crate) async fn fetch(&self, c: &Conn, b: &Struct) -> Struct {
        let ns = c.namespace();
        let max_wait = b.i32("MaxWaitMs").clamp(0, MAX_WAIT_MS) as u64;
        let min_bytes = b.i32("MinBytes");
        let max_bytes = (b.i32("MaxBytes").max(1) as usize).min(MAX_FETCH_BYTES);
        let wanted: Wanted = b
            .structs("Topics")
            .map(|t| {
                (
                    t.str("Topic").unwrap_or("").to_string(),
                    t.structs("Partitions")
                        .map(|p| (p.i32("Partition"), p.i64("FetchOffset"), p.i32("PartitionMaxBytes").max(0) as usize))
                        .collect(),
                )
            })
            .collect();
        let deadline = Instant::now() + Duration::from_millis(max_wait);
        let round = loop {
            // take the current notifier before looking, so that nothing stored in between is missed
            let notify = self.shared.state.lock().notify.clone();
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let round = self.fetch_round(&ns, &wanted, max_bytes);
            let ready = round.any_error || max_wait == 0 || min_bytes <= 0 || (round.bytes > 0 && round.bytes as i64 >= min_bytes as i64);
            if ready || Instant::now() >= deadline {
                break round;
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(deadline) => {}
            }
        };
        let topics: Vec<Struct> = round
            .topics
            .into_iter()
            .map(|(name, parts)| {
                let parts: Vec<Struct> = parts
                    .into_iter()
                    .map(|p| {
                        Struct::new()
                            .with("PartitionIndex", p.index)
                            .with("ErrorCode", p.error)
                            .with("HighWatermark", p.high)
                            .with("LastStableOffset", p.high)
                            .with("LogStartOffset", p.start)
                            .with("AbortedTransactions", Value::Null)
                            .with("PreferredReadReplica", -1)
                            .with("Records", records_of(&p.events))
                    })
                    .collect();
                Struct::new().with("Topic", name).with("Partitions", parts)
            })
            .collect();
        // no fetch sessions: session id 0 tells the client to send full requests
        Struct::new().with("ThrottleTimeMs", 0).with("ErrorCode", 0).with("SessionId", 0).with("Responses", topics)
    }

    fn fetch_round(&self, ns: &str, wanted: &Wanted, max_bytes: usize) -> Round {
        let now = now_ms();
        let mut st = self.shared.state.lock();
        let mut round = Round { topics: Vec::new(), bytes: 0, any_error: false };
        for (topic, parts) in wanted {
            let key = crate::model::key(topic);
            let nsk = st.resolve(ns);
            let mut out = Vec::new();
            if let Some(h) = st.namespaces.get_mut(&nsk).and_then(|n| n.hubs.get_mut(&key)) {
                h.trim_all(now);
            }
            for &(idx, offset, pmax) in parts {
                let partition = st.hub(ns, topic).and_then(|h| usize::try_from(idx).ok().and_then(|i| h.partitions.get(i)));
                let Some(p) = partition else {
                    round.any_error = true;
                    out.push(PartRaw { index: idx, error: err::UNKNOWN_TOPIC_OR_PARTITION, high: -1, start: -1, events: Vec::new() });
                    continue;
                };
                let (start, high) = (p.first_seq(), p.next_seq);
                if offset < start || offset > high {
                    round.any_error = true;
                    out.push(PartRaw { index: idx, error: err::OFFSET_OUT_OF_RANGE, high, start, events: Vec::new() });
                    continue;
                }
                let mut events = Vec::new();
                let mut used = 0usize;
                let skip = (offset - start) as usize;
                for ev in p.events.iter().skip(skip).take(MAX_EVENTS_PER_PARTITION) {
                    let size = ev.size + RECORD_OVERHEAD;
                    // always give at least one event overall, so a big event can be read at all
                    let first_of_all = round.bytes + used == 0;
                    if !first_of_all && (used + size > pmax || round.bytes + used + size > max_bytes) {
                        break;
                    }
                    used += size;
                    events.push(ev.clone());
                }
                round.bytes += used;
                out.push(PartRaw { index: idx, error: 0, high, start, events });
            }
            round.topics.push((topic.clone(), out));
        }
        round
    }

    pub(crate) fn list_offsets(&self, c: &Conn, b: &Struct) -> Struct {
        let ns = c.namespace();
        let now = now_ms();
        let mut st = self.shared.state.lock();
        let mut topics = Vec::new();
        for t in b.structs("Topics") {
            let name = t.str("Name").unwrap_or("").to_string();
            let nsk = st.resolve(&ns);
            if let Some(h) = st.namespaces.get_mut(&nsk).and_then(|n| n.hubs.get_mut(&crate::model::key(&name))) {
                h.trim_all(now);
            }
            let mut parts = Vec::new();
            for p in t.structs("Partitions") {
                let idx = p.i32("PartitionIndex");
                let ts = p.i64("Timestamp");
                let part = st.hub(&ns, &name).and_then(|h| usize::try_from(idx).ok().and_then(|i| h.partitions.get(i)));
                let (code, time, offset) = match part {
                    None => (err::UNKNOWN_TOPIC_OR_PARTITION, -1, -1),
                    // latest: the offset the next event gets
                    Some(p) if ts == -1 => (0, -1, p.next_seq),
                    // earliest: the oldest retained event
                    Some(p) if ts == -2 => (0, -1, p.first_seq()),
                    // the event with the greatest timestamp (Kafka 3.0+, version 7: not served, but harmless)
                    Some(p) if ts == -3 => match p.events.back() {
                        Some(e) => (0, e.enqueued_ms, e.seq),
                        None => (0, -1, -1),
                    },
                    // the first event at or after the time
                    Some(p) => {
                        let i = p.events.partition_point(|e| e.enqueued_ms < ts);
                        match p.events.get(i) {
                            Some(e) => (0, e.enqueued_ms, e.seq),
                            None => (0, -1, -1),
                        }
                    }
                };
                parts.push(
                    Struct::new()
                        .with("PartitionIndex", idx)
                        .with("ErrorCode", code)
                        .with("Timestamp", time)
                        .with("Offset", offset)
                        .with("LeaderEpoch", if code == 0 { 0 } else { -1 }),
                );
            }
            topics.push(Struct::new().with("Name", name).with("Partitions", parts));
        }
        Struct::new().with("ThrottleTimeMs", 0).with("Topics", topics)
    }
}

/// One record batch holding the events (null when there are none).
fn records_of(events: &[Arc<Event>]) -> Value {
    let (Some(first), Some(last)) = (events.first(), events.last()) else { return Value::Bytes(Bytes::new()) };
    let records: Vec<_> = events.iter().map(|e| event_to_record(e, first.seq, first.enqueued_ms)).collect();
    let batch = Batch {
        base_offset: first.seq,
        partition_leader_epoch: 0,
        attributes: 0,
        last_offset_delta: (last.seq - first.seq) as i32,
        first_timestamp: first.enqueued_ms,
        max_timestamp: last.enqueued_ms,
        producer_id: -1,
        producer_epoch: -1,
        base_sequence: -1,
        records,
        wire_len: 0,
    };
    Value::Bytes(Bytes::from(encode_batch(&batch, false)))
}
