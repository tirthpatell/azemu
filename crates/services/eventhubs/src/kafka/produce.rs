//! Produce: record batches into partitions.

use azemu_kafka::api::err;
use azemu_kafka::records::{self, RecordsError};
use azemu_kafka::{Struct, Value};

use super::convert::record_to_event;
use super::handlers::{Broker, Conn};
use super::store::{AppendError, Appended, ProducerBatch};
use crate::model::{EVENT_OVERHEAD, MAX_EVENTS_PER_BATCH, MAX_MESSAGE_SIZE};
use crate::state::now_ms;

/// What one batch's records may take once inflated: four times the message size (a client batches several messages;
/// Apache Kafka counts the compressed size against `max.message.bytes`, Event Hubs' 1 MiB is what bounds one event).
pub const MAX_BATCH_BYTES: usize = 4 * MAX_MESSAGE_SIZE as usize;
/// What all the batches of one Produce request may cost together: their inflated bytes plus [`EVENT_OVERHEAD`] for every record.
/// This is what bounds the memory a request can make the emulator hold while it is decoded (a frame is up to 16 MiB).
pub const MAX_REQUEST_COST: usize = 32 * 1024 * 1024;

impl Broker {
    /// `None` when the client asked for no answer (`acks=0`).
    pub(crate) fn produce(&self, c: &Conn, b: &Struct) -> Option<Struct> {
        let ns = c.namespace();
        let acks = b.i16("Acks");
        let transactional = b.get("TransactionalId").as_str().is_some();
        let mut responses = Vec::new();
        let mut budget = records::Budget::new(MAX_BATCH_BYTES, MAX_EVENTS_PER_BATCH, MAX_REQUEST_COST, EVENT_OVERHEAD);
        for t in b.structs("TopicData") {
            let topic = t.str("Name").unwrap_or("");
            let mut parts = Vec::new();
            for p in t.structs("PartitionData") {
                let idx = p.i32("Index");
                let (code, base) = if !(-1..=1).contains(&acks) {
                    (err::INVALID_REQUIRED_ACKS, -1)
                } else if transactional {
                    (err::INVALID_TXN_STATE, -1)
                } else {
                    self.produce_partition(&ns, topic, idx, p.bytes("Records").map(|b| &b[..]).unwrap_or(&[]), &mut budget)
                };
                parts.push(
                    Struct::new()
                        .with("Index", idx)
                        .with("ErrorCode", code)
                        .with("BaseOffset", base)
                        .with("LogAppendTimeMs", -1i64)
                        .with("LogStartOffset", self.log_start(&ns, topic, idx))
                        .with("RecordErrors", Vec::<Struct>::new())
                        .with("ErrorMessage", Value::Null),
                );
            }
            responses.push(Struct::new().with("Name", topic.to_string()).with("PartitionResponses", parts));
        }
        if acks == 0 {
            return None;
        }
        Some(Struct::new().with("Responses", responses).with("ThrottleTimeMs", 0))
    }

    fn log_start(&self, ns: &str, topic: &str, idx: i32) -> i64 {
        let st = self.shared.state.lock();
        st.hub(ns, topic).and_then(|h| usize::try_from(idx).ok().and_then(|i| h.partitions.get(i))).map(|p| p.first_seq()).unwrap_or(-1)
    }

    /// (error code, base offset)
    fn produce_partition(&self, ns: &str, topic: &str, idx: i32, data: &[u8], budget: &mut records::Budget) -> (i16, i64) {
        let Ok(partition) = usize::try_from(idx) else { return (err::UNKNOWN_TOPIC_OR_PARTITION, -1) };
        // decoding and converting cost time in proportion to the bytes: done before the shared lock is taken
        let batches = match records::decode_batches(data, budget) {
            Ok(b) => b,
            Err(RecordsError::Corrupt(_)) => return (err::CORRUPT_MESSAGE, -1),
            Err(RecordsError::UnsupportedMagic(_)) | Err(RecordsError::UnsupportedCompression(_)) => {
                return (err::UNSUPPORTED_FOR_MESSAGE_FORMAT, -1)
            }
            Err(RecordsError::TooLarge) => return (err::MESSAGE_TOO_LARGE, -1),
            Err(RecordsError::RequestTooLarge) => return (err::RECORD_LIST_TOO_LARGE, -1),
        };
        let mut prepared = Vec::with_capacity(batches.len());
        for batch in &batches {
            if batch.wire_len > MAX_MESSAGE_SIZE as usize || batch.records.iter().any(|r| r.payload_size() > MAX_MESSAGE_SIZE as usize) {
                return (err::MESSAGE_TOO_LARGE, -1);
            }
            if batch.is_transactional() {
                return (err::INVALID_TXN_STATE, -1);
            }
            if batch.is_control() {
                continue;
            }
            let events: Vec<_> = batch.records.iter().map(record_to_event).collect();
            let producer = (batch.producer_id >= 0).then_some(ProducerBatch {
                id: batch.producer_id,
                epoch: batch.producer_epoch,
                base_sequence: batch.base_sequence,
            });
            prepared.push((producer, events));
        }
        let now = now_ms();
        let mut base = -1;
        let mut st = self.shared.state.lock();
        for (producer, events) in prepared {
            match st.kafka_append(ns, topic, partition, producer, events, now) {
                Ok(Appended::Stored { base_offset }) | Ok(Appended::Duplicate { base_offset }) => {
                    if base < 0 {
                        base = base_offset;
                    }
                }
                Err(AppendError::UnknownTopic) | Err(AppendError::UnknownPartition) => return (err::UNKNOWN_TOPIC_OR_PARTITION, -1),
                Err(AppendError::ProducerFenced) => return (err::INVALID_PRODUCER_EPOCH, -1),
                Err(AppendError::OutOfOrder) => return (err::OUT_OF_ORDER_SEQUENCE_NUMBER, -1),
                Err(AppendError::DuplicateSequence) => return (err::DUPLICATE_SEQUENCE_NUMBER, -1),
            }
        }
        if base < 0 {
            // nothing to store (an empty request): the log end
            base = st.hub(ns, topic).and_then(|h| h.partitions.get(partition)).map(|p| p.next_seq).unwrap_or(0);
        }
        (err::NONE, base)
    }
}
