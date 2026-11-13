//! App Configuration events for Event Grid: `Microsoft.AppConfiguration.KeyValueModified` and
//! `Microsoft.AppConfiguration.KeyValueDeleted`.
//!
//! The crate does not know Event Grid. The unified binary sets a sink
//! ([`AppConfigService::set_event_sink`]); after a key-value write or delete succeeded, the
//! event is built from the request and the answer and handed over (the store lock is already
//! released). Shape: the examples `keyvalue_modified.json` / `keyvalue_deleted.json` under
//! `refs/specs/specification/eventgrid/data-plane/EventGridSystemEvents/examples/2024-01-01`:
//! the subject is the key-value URL (`https://{store}.azconfig…/kv/{key}?label={label}`), `data`
//! has `key`, `label` (null without a label), `etag` (without quotes) and `syncToken` (the
//! `Sync-Token` of the answer). The Learn page lists `dataVersion` as defined by the publisher;
//! the emulator sends `1`.
//!
//! Not built: snapshot events, and an event when a key-value is locked or unlocked.

use std::sync::Arc;

use azemu_core::time::OffsetDateTime;
use azemu_core::{Request, Response};
use serde_json::{json, Value};

use crate::AppConfigService;

/// One event, before Event Grid wraps it.
#[derive(Clone, Debug)]
pub struct StoreEvent {
    /// Lower-case store name.
    pub store: String,
    /// `Microsoft.AppConfiguration.KeyValueModified` or `...KeyValueDeleted`.
    pub event_type: String,
    pub subject: String,
    pub data: Value,
    pub data_version: String,
    pub id: String,
    pub time: OffsetDateTime,
}

/// Where App Configuration events go. Both methods are called without a store lock held.
pub trait StoreEventSink: Send + Sync {
    /// Does anyone listen to events of this store? Called for every write; keep it cheap.
    fn wants(&self, store: &str) -> bool;
    fn raise(&self, event: StoreEvent);
}

impl AppConfigService {
    /// Send events to `sink`. See the module documentation.
    pub fn set_event_sink(&self, sink: Arc<dyn StoreEventSink>) {
        *self.sink.write() = Some(sink);
    }
}

/// Raise the event for a key-value write or delete that succeeded.
pub(crate) fn after(sink: &dyn StoreEventSink, req: &Request, resp: &Response) {
    if resp.status().as_u16() != 200 {
        return;
    }
    // The key is everything after `/kv/`, also when it holds slashes that the client did not encode (`/kv/app/color`): the
    // service reads such a path as one key, so the event must be raised for it as well. The subject carries the decoded key.
    let raw_key = req.path.trim_start_matches('/').strip_prefix("kv/").filter(|k| !k.is_empty());
    let event_type = match (req.method.as_str(), raw_key) {
        ("PUT", Some(_)) => "Microsoft.AppConfiguration.KeyValueModified",
        ("DELETE", Some(_)) => "Microsoft.AppConfiguration.KeyValueDeleted",
        _ => return,
    };
    let key = match raw_key {
        Some(k) => percent_encoding::percent_decode_str(k).decode_utf8_lossy().into_owned(),
        None => return,
    };
    let key = &key;
    let store = req.account.to_ascii_lowercase();
    if !sink.wants(&store) {
        return;
    }
    let label = req.query.get("label").filter(|l| !l.is_empty() && *l != "\0");
    let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let mut subject = format!("{}/kv/{}", req.ctx.endpoint.base_url(), key);
    if let Some(l) = label {
        // the label is part of a URL: percent-encoded like any query value
        subject.push_str(&format!("?label={}", crate::paging::encode_component(l)));
    }
    sink.raise(StoreEvent {
        store,
        event_type: event_type.into(),
        subject,
        data: json!({
            "key": key,
            "label": label,
            "etag": header("etag").trim_matches('"'),
            "syncToken": header("sync-token"),
        }),
        data_version: "1".into(),
        id: uuid::Uuid::new_v4().to_string(),
        time: req.ctx.time,
    });
}
