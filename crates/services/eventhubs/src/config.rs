//! The JSON config file, in the format of Microsoft's Event Hubs emulator (`Config.json`):
//!
//! ```json
//! { "UserConfig": { "NamespaceConfig": [ { "Type": "EventHub", "Name": "emulator",
//!     "Entities": [ { "Name": "eh1", "PartitionCount": "2",
//!                     "ConsumerGroups": [ { "Name": "cg1" } ] } ] } ] } }
//! ```
//!
//! Two additions of ours on an entity: `MessageRetentionInDays` (a number) and
//! `MessageRetentionInSeconds` (for tests that need a short retention).
//!
//! When the file names exactly one namespace, the name `default` (what `sb://localhost` and
//! `sb://default.eventhub.localhost` resolve to) is an alias of it.

use std::path::Path;

use serde_json::Value;

use crate::model::{DEFAULT_PARTITIONS, DEFAULT_RETENTION_MS};
use crate::state::{State, DEFAULT_NAMESPACE};

pub struct Config {
    pub raw: Value,
}

pub fn load(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {e}"))?;
    parse_str(&text)
}

pub fn parse_str(text: &str) -> Result<Config, String> {
    Ok(Config { raw: serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))? })
}

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_object()?.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
}

fn list<'a>(v: &'a Value, key: &str) -> Vec<&'a Value> {
    get(v, key).and_then(|x| x.as_array()).map(|a| a.iter().collect()).unwrap_or_default()
}

fn name_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        other => get(other, "Name").and_then(|n| n.as_str()).map(str::to_string),
    }
    .filter(|s| !s.is_empty())
}

fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn is_eventhubs_namespace(n: &Value) -> bool {
    match get(n, "Type").and_then(|t| t.as_str()) {
        Some(t) => t.to_ascii_lowercase().replace(' ', "").starts_with("eventhub"),
        None => true,
    }
}

/// Create everything in the config. Event hubs that already exist are left alone.
pub fn apply(state: &mut State, cfg: &Config, now: i64) -> Result<(), String> {
    let user = get(&cfg.raw, "UserConfig").unwrap_or(&cfg.raw);
    let namespaces: Vec<&Value> =
        list(user, "NamespaceConfig").into_iter().chain(list(user, "Namespaces")).filter(|n| is_eventhubs_namespace(n)).collect();
    if namespaces.len() == 1 {
        if let Some(n) = name_of(namespaces[0]) {
            if !n.eq_ignore_ascii_case(DEFAULT_NAMESPACE) {
                state.default_alias = Some(n.to_lowercase());
            }
        }
    }
    for n in namespaces {
        let ns_name = name_of(n).ok_or("namespace without a name")?;
        state.declare_namespace(&ns_name);
        for e in list(n, "Entities").into_iter().chain(list(n, "EventHubs")) {
            let name = name_of(e).ok_or("entity without a name")?;
            let partitions = match get(e, "PartitionCount").and_then(as_f64) {
                Some(p) if p >= 1.0 => p as u32,
                Some(_) => return Err(format!("event hub '{name}': PartitionCount must be at least 1")),
                None => DEFAULT_PARTITIONS,
            };
            let retention = match (get(e, "MessageRetentionInSeconds").and_then(as_f64), get(e, "MessageRetentionInDays").and_then(as_f64))
            {
                (Some(s), _) => (s * 1000.0) as i64,
                (None, Some(d)) => (d * 86_400_000.0) as i64,
                _ => DEFAULT_RETENTION_MS,
            };
            if state.hub(&ns_name, &name).is_some() {
                continue;
            }
            let hub = state
                .create_hub(&ns_name, &name, partitions, retention, now)
                .map_err(|e| format!("event hub '{name}': {}", e.description))?;
            for cg in list(e, "ConsumerGroups") {
                if let Some(c) = name_of(cg) {
                    hub.add_consumer_group(&c);
                }
            }
        }
    }
    Ok(())
}
