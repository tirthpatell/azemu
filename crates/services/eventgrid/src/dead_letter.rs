//! Dead letters written as blobs (`deadLetterDestination` with `endpointType: StorageBlob`).
//!
//! This crate does not depend on Blob Storage. The program that mounts the services registers one writer
//! ([`EventGridService::register_dead_letter_writer`](crate::EventGridService::register_dead_letter_writer)) and is handed
//! finished blobs ([`DeadLetterBlob`]).
//!
//! What Azure wrote was seen in November 2026 on a custom topic whose destination (a Storage queue) had been deleted:
//!
//! * a blob in the named container, content type `application/json`, named
//!   `{topic name}/{SUBSCRIPTION NAME}/{yyyy}/{MM}/{dd}/{HH}/{guid}.json` (UTC, the subscription name in capitals);
//! * the body is a JSON array of the events; the events dead-lettered within the same few minutes share one blob;
//! * each event is the delivered event, in Event Grid schema key order `id`, `eventTime`, `eventType`, `dataVersion`,
//!   `metadataVersion`, `topic`, `subject`, then `deadLetterReason`, `deliveryAttempts`, `lastDeliveryOutcome`, `publishTime`,
//!   `lastDeliveryAttemptTime`, then `data`;
//! * Azure wrote the blob about five minutes after the last attempt. Here it is written at once, one event to a blob.

use std::sync::Arc;

use azemu_core::time::{iso8601_7, OffsetDateTime};
use bytes::Bytes;
use serde_json::{json, Map, Value};

use crate::destination::BlobTarget;
use crate::state::State;
use crate::Shared;

/// One blob to write.
#[derive(Clone, Debug)]
pub struct DeadLetterBlob {
    pub account: String,
    pub container: String,
    /// `{topic}/{SUBSCRIPTION}/{yyyy}/{MM}/{dd}/{HH}/{guid}.json`
    pub name: String,
    pub content_type: String,
    pub body: Bytes,
}

/// Writes a dead-letter blob. `Err` is logged; the event stays in the subscription's dead-letter list either way.
/// The write must not raise a blob event: a subscription on the same storage account would otherwise see its own dead letters.
pub type DeadLetterWriter = Arc<dyn Fn(&DeadLetterBlob) -> Result<(), String> + Send + Sync>;

/// An event that was dead-lettered and has not been written yet (kept in `Sub::outbox` until the state lock is let go).
#[derive(Clone, Debug)]
pub struct Record {
    pub target: BlobTarget,
    pub event: Value,
    pub reason: String,
    pub attempts: u32,
    pub outcome: String,
    pub publish_time: String,
    pub last_attempt: String,
    pub at: OffsetDateTime,
}

/// What Azure calls the last outcome of an attempt. `NotFound` was seen (a Storage queue that did not exist); the others are
/// the HTTP status names and were not tried.
pub fn outcome_name(status: Option<u16>, detail: &str) -> String {
    match status {
        Some(400) => "BadRequest",
        Some(401) => "Unauthorized",
        Some(403) => "Forbidden",
        Some(404) => "NotFound",
        Some(408) => "Timeout",
        Some(413) => "PayloadTooLarge",
        Some(429) | Some(503) => "Busy",
        Some(_) => "Error",
        None if detail.contains("NotFound") || detail.contains("not found") || detail.contains("does not exist") => "NotFound",
        None if detail.to_ascii_lowercase().contains("timed out") => "Timeout",
        None => "Error",
    }
    .to_string()
}

/// The dead-lettered event with the fields Azure adds, in Azure's key order.
pub fn event_with_fields(event: &Value, r: &Record) -> Value {
    let Some(src) = event.as_object() else { return event.clone() };
    let extra = [
        ("deadLetterReason", json!(r.reason)),
        ("deliveryAttempts", json!(r.attempts)),
        ("lastDeliveryOutcome", json!(r.outcome)),
        ("publishTime", json!(r.publish_time)),
        ("lastDeliveryAttemptTime", json!(r.last_attempt)),
    ];
    let mut out = Map::new();
    for k in ["id", "eventTime", "eventType", "dataVersion", "metadataVersion", "topic", "subject"] {
        if let Some(v) = src.get(k) {
            out.insert(k.to_string(), v.clone());
        }
    }
    for (k, v) in extra {
        out.insert(k.to_string(), v);
    }
    for (k, v) in src {
        if k != "data" && !out.contains_key(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    if let Some(d) = src.get("data") {
        out.insert("data".to_string(), d.clone());
    }
    Value::Object(out)
}

pub fn blob_name(topic: &str, subscription: &str, at: OffsetDateTime) -> String {
    format!(
        "{topic}/{}/{:04}/{:02}/{:02}/{:02}/{}.json",
        subscription.to_ascii_uppercase(),
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        uuid::Uuid::new_v4()
    )
}

pub fn blob_of(topic: &str, subscription: &str, r: &Record) -> DeadLetterBlob {
    let body = serde_json::to_vec(&Value::Array(vec![event_with_fields(&r.event, r)])).unwrap_or_default();
    DeadLetterBlob {
        account: r.target.account.clone(),
        container: r.target.container.clone(),
        name: blob_name(topic, subscription, r.at),
        content_type: "application/json".into(),
        body: Bytes::from(body),
    }
}

pub fn time_text(ms: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000).map(iso8601_7).unwrap_or_default()
}

/// The name Azure puts first in the blob name: the topic's own name (for an event source, the resource's name).
fn topic_name(h: &crate::state::Host, topic: Option<&crate::state::Topic>) -> String {
    if let Some(t) = topic {
        return t.name.clone();
    }
    h.resource_id.as_deref().and_then(|r| r.trim_end_matches('/').rsplit('/').next()).unwrap_or(&h.name).to_string()
}

/// Take what is waiting in the subscriptions' outboxes (under the lock).
fn take_all(st: &mut State) -> Vec<(String, String, Record)> {
    let mut out = Vec::new();
    for h in st.hosts.values_mut() {
        let hn = topic_name(h, None);
        for s in h.subs.values_mut() {
            let name = s.name.clone();
            out.extend(std::mem::take(&mut s.outbox).into_iter().map(|r| (hn.clone(), name.clone(), r)));
        }
        for t in h.topics.values_mut() {
            let tn = t.name.clone();
            for s in t.subs.values_mut() {
                let name = s.name.clone();
                out.extend(std::mem::take(&mut s.outbox).into_iter().map(|r| (tn.clone(), name.clone(), r)));
            }
        }
    }
    out
}

/// Write the dead letters that wait. Called after the state lock is let go, wherever an event was dead-lettered.
pub fn flush(shared: &Arc<Shared>) {
    if !crate::state::outbox_may_have_work() {
        return;
    }
    let work = take_all(&mut shared.state.lock());
    if work.is_empty() {
        return;
    }
    let writer = shared.dead_letter_writer.read().clone();
    for (topic, sub, r) in work {
        let blob = blob_of(&topic, &sub, &r);
        match &writer {
            Some(w) => {
                if let Err(e) = w(&blob) {
                    tracing::warn!(
                        "eventgrid: dead letter of subscription '{sub}' could not be written to {}/{}: {e}",
                        blob.account,
                        blob.container
                    );
                }
            }
            None => {
                tracing::warn!("eventgrid: no writer for dead-letter blobs is registered; the dead letter of '{sub}' stays in the list")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Record {
        Record {
            target: BlobTarget { account: "acct".into(), container: "dl".into() },
            event: json!({"id":"e1","topic":"/t","subject":"s","data":{"n":1},"eventType":"T","dataVersion":"1.0","metadataVersion":"1","eventTime":"2026-01-01T00:00:00Z"}),
            reason: "MaxDeliveryAttemptsExceeded".into(),
            attempts: 1,
            outcome: "NotFound".into(),
            publish_time: "2026-01-01T00:00:01.0000000Z".into(),
            last_attempt: "2026-01-01T00:00:02.0000000Z".into(),
            at: OffsetDateTime::from_unix_timestamp(1_795_542_000).unwrap(),
        }
    }

    #[test]
    fn event_has_the_added_fields_in_azures_order() {
        let v = event_with_fields(&record().event, &record());
        let keys: Vec<_> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "id",
                "eventTime",
                "eventType",
                "dataVersion",
                "metadataVersion",
                "topic",
                "subject",
                "deadLetterReason",
                "deliveryAttempts",
                "lastDeliveryOutcome",
                "publishTime",
                "lastDeliveryAttemptTime",
                "data"
            ]
        );
        assert_eq!(v["deliveryAttempts"], 1);
        assert_eq!(v["lastDeliveryOutcome"], "NotFound");
    }

    #[test]
    fn blob_name_and_body() {
        let b = blob_of("mytopic", "sub1", &record());
        let parts: Vec<_> = b.name.split('/').collect();
        assert_eq!(&parts[..2], ["mytopic", "SUB1"]);
        assert_eq!(parts.len(), 7);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 2);
        assert!(parts[6].ends_with(".json") && parts[6].len() == 36 + 5);
        let body: Value = serde_json::from_slice(&b.body).unwrap();
        assert!(body.is_array() && body[0]["id"] == "e1");
        assert_eq!(b.content_type, "application/json");
    }

    #[test]
    fn outcome_names() {
        assert_eq!(outcome_name(Some(404), ""), "NotFound");
        assert_eq!(outcome_name(None, "QueueNotFound: the queue 'q' does not exist"), "NotFound");
        assert_eq!(outcome_name(Some(500), ""), "Error");
    }
}
