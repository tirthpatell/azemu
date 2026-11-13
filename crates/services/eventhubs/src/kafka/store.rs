//! Appending a Kafka batch to a partition, with the bookkeeping of idempotent producers.
//!
//! A Kafka offset is the event's sequence number in its partition (they start at 0, never have gaps and are never
//! reused), so what a producer is told as the base offset is the sequence number of its first event.

use std::collections::VecDeque;

use crate::model::{key, PreparedEvent};
use crate::state::State;

/// Batches of one producer a partition remembers, to answer a retry of a batch that was already stored. Kafka keeps 5.
const RECENT_BATCHES: usize = 5;
/// Producers one partition keeps state for; past that the oldest are forgotten (their next batch starts a new record).
pub const MAX_PRODUCERS_PER_PARTITION: usize = 10_000;

#[derive(Debug, Default)]
pub struct ProducerSeq {
    pub epoch: i16,
    /// The sequence number the next batch must start with.
    pub next_seq: i32,
    /// (first sequence, record count, base offset) of the latest stored batches.
    pub recent: VecDeque<(i32, i32, i64)>,
}

/// The idempotent producer fields of a batch (`producerId >= 0`).
#[derive(Debug, Clone, Copy)]
pub struct ProducerBatch {
    pub id: i64,
    pub epoch: i16,
    pub base_sequence: i32,
}

#[derive(Debug, PartialEq)]
pub enum Appended {
    Stored {
        base_offset: i64,
    },
    /// The batch was stored before (a retry): nothing was added, this is where it is.
    Duplicate {
        base_offset: i64,
    },
}

#[derive(Debug, PartialEq)]
pub enum AppendError {
    UnknownTopic,
    UnknownPartition,
    /// A newer epoch of this producer id exists.
    ProducerFenced,
    /// A gap in the sequence numbers.
    OutOfOrder,
    /// An old sequence number that is no longer remembered.
    DuplicateSequence,
}

fn advance(seq: i32, count: i32) -> i32 {
    let n = seq as i64 + count as i64;
    if n > i32::MAX as i64 {
        (n - i32::MAX as i64 - 1) as i32
    } else {
        n as i32
    }
}

impl State {
    /// Store the events of one batch in one partition, atomically, under the producer's sequence rules:
    /// - a producer id the partition has not seen starts at whatever sequence it sends (lenient: after a reset of the
    ///   emulator a running producer carries on);
    /// - the next batch of a known producer starts at the expected sequence; a batch that was stored before is answered
    ///   with its original offset and not stored again; a gap is `OutOfOrder`; an older epoch is `ProducerFenced`.
    pub fn kafka_append(
        &mut self,
        ns: &str,
        topic: &str,
        partition: usize,
        producer: Option<ProducerBatch>,
        events: Vec<PreparedEvent>,
        now: i64,
    ) -> Result<Appended, AppendError> {
        let nsk = self.resolve(ns);
        let hub = self.namespaces.get_mut(&nsk).and_then(|n| n.hubs.get_mut(&key(topic))).ok_or(AppendError::UnknownTopic)?;
        if partition >= hub.partitions.len() {
            return Err(AppendError::UnknownPartition);
        }
        hub.trim_all(now);
        let p = &mut hub.partitions[partition];
        let count = events.len() as i32;
        if let Some(pb) = producer {
            if let Some(st) = p.producers.get_mut(&pb.id) {
                if pb.epoch < st.epoch {
                    return Err(AppendError::ProducerFenced);
                }
                if pb.epoch > st.epoch {
                    *st = ProducerSeq { epoch: pb.epoch, next_seq: pb.base_sequence, recent: VecDeque::new() };
                }
                if let Some(&(_, _, base)) = st.recent.iter().find(|(first, c, _)| *first == pb.base_sequence && *c == count) {
                    return Ok(Appended::Duplicate { base_offset: base });
                }
                if pb.base_sequence != st.next_seq {
                    // behind the expected sequence: an old batch; ahead of it: a gap
                    let behind = (st.next_seq as i64 - pb.base_sequence as i64).rem_euclid(1 << 32) < (1 << 31);
                    return Err(if behind { AppendError::DuplicateSequence } else { AppendError::OutOfOrder });
                }
            } else if p.producers.len() >= MAX_PRODUCERS_PER_PARTITION {
                let first = p.producers.keys().next().copied();
                if let Some(k) = first {
                    p.producers.remove(&k);
                }
            }
        }
        let base_offset = p.next_seq;
        p.rev = crate::model::next_revision();
        for e in events {
            p.append_prepared(e, now);
        }
        if let Some(pb) = producer {
            let st = p.producers.entry(pb.id).or_insert_with(|| ProducerSeq {
                epoch: pb.epoch,
                next_seq: pb.base_sequence,
                recent: VecDeque::new(),
            });
            st.epoch = pb.epoch;
            st.next_seq = advance(pb.base_sequence, count);
            st.recent.push_back((pb.base_sequence, count, base_offset));
            while st.recent.len() > RECENT_BATCHES {
                st.recent.pop_front();
            }
        }
        self.wake(&nsk, topic, Some(partition));
        Ok(Appended::Stored { base_offset })
    }
}
