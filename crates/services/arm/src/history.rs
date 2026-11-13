//! Deployment history is bounded. Azure keeps at most 800 deployments in each resource group (and at most 800 at
//! subscription scope) and deletes the oldest automatically (Microsoft Learn, "Azure subscription and service limits":
//! "Deployments per resource group per location ... 800"; deployments are deleted from the history automatically when the
//! count reaches the limit). The emulator keeps a deployment's template once (in `extra`) and also bounds the bytes of all
//! the deployment records of one subscription (`AZEMU_ARM_DEPLOYMENT_HISTORY_MB`, default 64), dropping the oldest first.

use serde_json::Value;

use crate::state::{group_key, State, Stored};

/// Deployments one resource group (or the subscription scope) keeps.
pub const MAX_DEPLOYMENTS_PER_SCOPE: usize = 800;
const DEFAULT_HISTORY_MB: usize = 64;

/// Bytes of deployment records one subscription keeps: the setting `AZEMU_ARM_DEPLOYMENT_HISTORY_MB`, read (and described
/// for `docs/SETTINGS.md`) when the service is made.
pub fn history_bytes_from_settings() -> usize {
    azemu_core::settings::describe(
        "AZEMU_ARM_DEPLOYMENT_HISTORY_MB",
        "64",
        "ARM: megabytes of deployment records one subscription keeps; the oldest are dropped first (Azure also keeps at most 800 per group).",
    );
    (azemu_core::settings::number("AZEMU_ARM_DEPLOYMENT_HISTORY_MB", DEFAULT_HISTORY_MB as u64) as usize).saturating_mul(1 << 20)
}

/// The default, for a service made with `ArmService::with_strict`.
pub fn default_history_bytes() -> usize {
    DEFAULT_HISTORY_MB << 20
}

/// Rough size of a JSON value in memory: its text, with a fixed amount for every node.
pub fn approx_bytes(v: &Value) -> usize {
    match v {
        Value::Null | Value::Bool(_) | Value::Number(_) => 16,
        Value::String(s) => 24 + s.len(),
        Value::Array(a) => 24 + a.iter().map(approx_bytes).sum::<usize>(),
        Value::Object(m) => 24 + m.iter().map(|(k, v)| 24 + k.len() + approx_bytes(v)).sum::<usize>(),
    }
}

fn is_deployment(s: &Stored) -> bool {
    s.ns == "Microsoft.Resources" && s.types.len() == 1 && s.types[0] == "deployments"
}

/// Bytes a stored deployment counts: its record and its private data.
pub fn record_bytes(s: &Stored) -> usize {
    approx_bytes(&s.body) + approx_bytes(&s.extra)
}

/// After a deployment record was stored in `rg` (empty: the subscription scope): drop the oldest records of the scope while it
/// has more than 800, then the oldest of the whole subscription while all together are over `byte_limit`. The record stored just now
/// (`keep`, a state key) is never dropped.
pub fn trim(st: &mut State, sub: &str, rg: &str, keep: &str, byte_limit: usize) {
    let scope_prefix = if rg.is_empty() {
        format!("/subscriptions/{}/providers/microsoft.resources/deployments/", sub.to_ascii_lowercase())
    } else {
        format!("{}/providers/microsoft.resources/deployments/", group_key(sub, rg))
    };
    let mut in_scope: Vec<(time_key::T, String)> = st
        .resources
        .range(scope_prefix.clone()..)
        .take_while(|(k, _)| k.starts_with(&scope_prefix))
        .filter(|(k, s)| !k[scope_prefix.len()..].contains('/') && is_deployment(s))
        .map(|(k, s)| (time_key::of(s), k.clone()))
        .collect();
    if in_scope.len() > MAX_DEPLOYMENTS_PER_SCOPE {
        in_scope.sort();
        let drop = in_scope.len() - MAX_DEPLOYMENTS_PER_SCOPE;
        for (_, k) in in_scope.iter().filter(|(_, k)| k != keep).take(drop) {
            st.resources.remove(k);
        }
    }
    let sub_prefix = format!("/subscriptions/{}/", sub.to_ascii_lowercase());
    let mut all: Vec<(time_key::T, String, usize)> = st
        .resources
        .range(sub_prefix.clone()..)
        .take_while(|(k, _)| k.starts_with(&sub_prefix))
        .filter(|(_, s)| is_deployment(s))
        .map(|(k, s)| (time_key::of(s), k.clone(), record_bytes(s)))
        .collect();
    let mut total: usize = all.iter().map(|(_, _, b)| *b).sum();
    if total > byte_limit {
        all.sort();
        for (_, k, b) in all {
            if total <= byte_limit {
                break;
            }
            if k == keep {
                continue;
            }
            st.resources.remove(&k);
            total -= b;
        }
    }
}

mod time_key {
    use crate::state::Stored;
    /// Age order: when it was stored, then by id.
    pub type T = (i128, String);
    pub fn of(s: &Stored) -> T {
        (s.created.unix_timestamp_nanos(), s.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::key;
    use azemu_core::time::OffsetDateTime;
    use serde_json::json;

    fn deployment(sub: &str, rg: &str, name: &str, secs: i64, size: usize) -> Stored {
        let id = crate::ids::resource_id(sub, rg, "Microsoft.Resources", &["deployments".to_string()], &[name.to_string()]);
        Stored {
            id,
            sub: sub.into(),
            rg: rg.into(),
            ns: "Microsoft.Resources".into(),
            types: vec!["deployments".into()],
            names: vec![name.into()],
            body: json!({"properties": {}}),
            extra: json!({"template": {"p": "x".repeat(size)}}),
            created: OffsetDateTime::from_unix_timestamp(1_800_000_000 + secs).unwrap(),
        }
    }

    #[test]
    fn the_subscription_keeps_a_bounded_number_of_bytes_of_history_and_the_newest_stays() {
        let sub = "11111111-2222-3333-4444-555555555555";
        let mut st = State::default();
        for (i, rg) in ["a", "b", "a", "b", "a"].iter().enumerate() {
            let d = deployment(sub, rg, &format!("d{i}"), i as i64, 100_000);
            let k = key(&d.id);
            st.resources.insert(k.clone(), d);
            trim(&mut st, sub, rg, &k, 250_000);
        }
        let mut left: Vec<String> = st.resources.values().map(|s| s.name().to_string()).collect();
        left.sort();
        assert_eq!(left, ["d3", "d4"], "two of 100 KB fit into 250 KB, and they are the newest across both groups");
        // a single record over the limit is kept (it was just stored)
        let big = deployment(sub, "a", "big", 10, 400_000);
        let k = key(&big.id);
        st.resources.insert(k.clone(), big);
        trim(&mut st, sub, "a", &k, 250_000);
        assert_eq!(st.resources.len(), 1);
    }

    #[test]
    fn a_template_is_counted_once() {
        let d = deployment("s", "g", "d", 0, 1_000_000);
        let n = record_bytes(&d);
        assert!((1_000_000..1_000_300).contains(&n), "{n}");
    }
}
