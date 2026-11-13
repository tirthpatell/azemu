//! Authorization rules (shared access policies) of Service Bus and Event Hubs.
//!
//! The data plane does not check rights or keys; the rules exist so templates and
//! `listKeys` calls work and give out a connection string that reaches the emulator.

use azemu_core::Response;
use serde_json::{json, Value};

use super::{bump_generation, generation, stored_child};
use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::state::Stored;
use crate::util;

pub const VALID_RIGHTS: &[&str] = &["Listen", "Send", "Manage"];

/// Check `properties.rights` and fill the default.
pub fn prepare(res: &mut Stored) -> ArmResult<()> {
    let props = util::props_mut(&mut res.body);
    let rights = props.get("rights").cloned().unwrap_or(json!([]));
    let list = rights.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return Err(ArmError::bad_request("BadRequest", "Authorization rule must have at least one right (Listen, Send or Manage)."));
    }
    for r in &list {
        if !r.as_str().map(|s| VALID_RIGHTS.contains(&s)).unwrap_or(false) {
            return Err(ArmError::bad_request("BadRequest", format!("The right '{r}' is not valid. Use Listen, Send or Manage.")));
        }
    }
    Ok(())
}

/// The root policy every namespace has.
pub fn root_rule(ns: &Stored) -> Stored {
    stored_child(ns, "authorizationRules", "RootManageSharedAccessKey", json!({"properties": {"rights": ["Listen", "Manage", "Send"]}}))
}

fn key_of(rule: &Stored, which: &str) -> String {
    util::derive_key(&format!("sas/{}/{which}", rule.id.to_ascii_lowercase()), generation(rule, which))
}

/// `label` is the host label of the service (`servicebus`, `eventhub`).
pub fn keys_json(cx: &Cx<'_>, rule: &Stored, label: &str, namespace: &str, entity: Option<&str>) -> Value {
    let primary = key_of(rule, "primary");
    let secondary = key_of(rule, "secondary");
    let endpoint = format!("sb://{namespace}.{label}.localhost{}/", cx.port_suffix());
    let conn = |key: &str| {
        let mut c = format!("Endpoint={endpoint};SharedAccessKeyName={};SharedAccessKey={key};UseDevelopmentEmulator=true", rule.name());
        if let Some(e) = entity {
            c.push_str(&format!(";EntityPath={e}"));
        }
        c
    };
    json!({
        "primaryConnectionString": conn(&primary),
        "secondaryConnectionString": conn(&secondary),
        "primaryKey": primary,
        "secondaryKey": secondary,
        "keyName": rule.name(),
    })
}

pub fn list_keys(cx: &Cx<'_>, rule: &Stored, label: &str, namespace: &str, entity: Option<&str>) -> Response {
    util::json_ok(200, &keys_json(cx, rule, label, namespace, entity))
}

pub fn regenerate(cx: &Cx<'_>, rule: &Stored, body: &Value, label: &str, namespace: &str, entity: Option<&str>) -> ArmResult<Response> {
    let kt = body.get("keyType").and_then(Value::as_str).unwrap_or("");
    let which = match kt {
        "PrimaryKey" => "primary",
        "SecondaryKey" => "secondary",
        _ => return Err(ArmError::bad_request("BadRequest", "keyType must be PrimaryKey or SecondaryKey.")),
    };
    bump_generation(cx, &rule.id, which);
    let fresh = cx.inner.state.lock().resources.get(&crate::state::key(&rule.id)).cloned().unwrap_or_else(|| rule.clone());
    Ok(list_keys(cx, &fresh, label, namespace, entity))
}
