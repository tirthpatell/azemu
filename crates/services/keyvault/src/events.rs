//! Key Vault events for Event Grid: `Microsoft.KeyVault.SecretNewVersionCreated` and
//! `Microsoft.KeyVault.KeyNewVersionCreated`.
//!
//! The crate does not know Event Grid. The unified binary sets a sink
//! ([`KeyVaultService::set_event_sink`]); after a request that created a version succeeded,
//! the event is built from the request and the answer and handed over (the vault lock is
//! already released by then). Shape: `refs/specs/specification/eventgrid/data-plane/
//! EventGridSystemEvents/examples/2024-01-01/secret_new_version_created.json` and the Learn
//! page "Azure Key Vault as Event Grid source": `data` has `Id`, `VaultName`, `ObjectType`,
//! `ObjectName`, `Version`, `NBF`, `EXP` (the last two as strings of seconds since 1970, left
//! out when the object has none), the subject is the object name, `dataVersion` is `1`.
//!
//! Not built: certificate events, near-expiry and expiry events, `VaultAccessPolicyChanged`.

use std::sync::Arc;

use azemu_core::time::OffsetDateTime;
use azemu_core::{Request, Response};
use serde_json::{json, Map, Value};

use crate::KeyVaultService;

/// One event, before Event Grid wraps it.
#[derive(Clone, Debug)]
pub struct VaultEvent {
    /// Lower-case vault name.
    pub vault: String,
    /// `Microsoft.KeyVault.SecretNewVersionCreated` and so on.
    pub event_type: String,
    /// The object name.
    pub subject: String,
    pub data: Value,
    pub data_version: String,
    pub id: String,
    pub time: OffsetDateTime,
}

/// Where Key Vault events go. Both methods are called without a vault lock held.
pub trait VaultEventSink: Send + Sync {
    /// Does anyone listen to events of this vault? Called for every write; keep it cheap.
    fn wants(&self, vault: &str) -> bool;
    fn raise(&self, event: VaultEvent);
}

impl KeyVaultService {
    /// Send events to `sink`. See the module documentation.
    pub fn set_event_sink(&self, sink: Arc<dyn VaultEventSink>) {
        *self.sink.write() = Some(sink);
    }
}

fn epoch(attrs: &Value, name: &str) -> Option<String> {
    attrs.get(name).and_then(Value::as_i64).map(|v| v.to_string())
}

/// Raise the event for a request that created a secret or key version, if it was one.
pub(crate) fn after(sink: &dyn VaultEventSink, req: &Request, resp: &Response) {
    if resp.status().as_u16() != 200 {
        return;
    }
    let vault = req.account.to_ascii_lowercase();
    let segs = req.segments();
    let s: Vec<&str> = segs.iter().map(String::as_str).collect();
    let (kind, name, event_type) = match (req.method.as_str(), s.as_slice()) {
        ("PUT", ["secrets", name]) => ("Secret", *name, "Microsoft.KeyVault.SecretNewVersionCreated"),
        ("POST", ["keys", name, "create" | "rotate"]) | ("PUT", ["keys", name]) => {
            ("Key", *name, "Microsoft.KeyVault.KeyNewVersionCreated")
        }
        _ => return,
    };
    if !sink.wants(&vault) {
        return;
    }
    let Ok(body) = serde_json::from_slice::<Value>(resp.body()) else { return };
    // secrets answer with `id`, keys with `key.kid`
    let id = match kind {
        "Secret" => body.get("id"),
        _ => body.get("key").and_then(|k| k.get("kid")),
    }
    .and_then(Value::as_str);
    let Some(id) = id else { return };
    let version = id.rsplit('/').next().unwrap_or("");
    let mut data = Map::new();
    data.insert("Id".into(), json!(id));
    data.insert("VaultName".into(), json!(vault));
    data.insert("ObjectType".into(), json!(kind));
    data.insert("ObjectName".into(), json!(name));
    data.insert("Version".into(), json!(version));
    if let Some(attrs) = body.get("attributes") {
        if let Some(v) = epoch(attrs, "nbf") {
            data.insert("NBF".into(), json!(v));
        }
        if let Some(v) = epoch(attrs, "exp") {
            data.insert("EXP".into(), json!(v));
        }
    }
    sink.raise(VaultEvent {
        vault,
        event_type: event_type.into(),
        subject: name.to_string(),
        data: Value::Object(data),
        data_version: "1".into(),
        id: uuid::Uuid::new_v4().to_string(),
        time: req.ctx.time,
    });
}
