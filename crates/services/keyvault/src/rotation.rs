//! Key rotation policy. The merge rule is copied from recorded behaviour: an update
//! replaces the Rotate action with what was sent, and replaces the Notify action
//! only when one was sent (otherwise the old one, `P30D` by default, stays).

use serde_json::{json, Map, Value};

use crate::common::{body_object, json_response, now_secs};
use crate::errors::*;
use crate::KeyVaultService;
use azemu_core::{Request, Response};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LifetimeAction {
    /// "Rotate" or "Notify"
    pub action: String,
    /// "timeAfterCreate" or "timeBeforeExpiry"
    pub trigger_kind: String,
    pub trigger_value: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RotationPolicy {
    pub lifetime_actions: Vec<LifetimeAction>,
    pub expiry_time: Option<String>,
    pub created: i64,
    pub updated: i64,
}

impl RotationPolicy {
    /// The policy every key starts with: notify 30 days before expiry.
    pub fn default_at(now: i64) -> RotationPolicy {
        RotationPolicy {
            lifetime_actions: vec![LifetimeAction {
                action: "Notify".into(),
                trigger_kind: "timeBeforeExpiry".into(),
                trigger_value: "P30D".into(),
            }],
            expiry_time: None,
            created: now,
            updated: now,
        }
    }

    pub fn to_json(&self, base: &str, name: &str) -> Value {
        let actions: Vec<Value> = self
            .lifetime_actions
            .iter()
            .map(|a| json!({"trigger": {a.trigger_kind.clone(): a.trigger_value}, "action": {"type": a.action}}))
            .collect();
        let mut attrs = Map::new();
        if let Some(e) = &self.expiry_time {
            attrs.insert("expiryTime".into(), json!(e));
        }
        attrs.insert("created".into(), json!(self.created));
        attrs.insert("updated".into(), json!(self.updated));
        json!({
            "id": format!("{base}/keys/{name}/rotationpolicy"),
            "lifetimeActions": actions,
            "attributes": attrs,
        })
    }
}

fn parse_actions(body: &Map<String, Value>) -> Result<Vec<LifetimeAction>, Response> {
    let mut out = Vec::new();
    match body.get("lifetimeActions") {
        None | Some(Value::Null) => {}
        Some(Value::Array(a)) => {
            for item in a {
                let action = item.pointer("/action/type").and_then(|v| v.as_str());
                let trigger = item.get("trigger").and_then(|t| t.as_object());
                let (Some(action), Some(trigger)) = (action, trigger) else {
                    return Err(bad_parameter("Every lifetime action needs an 'action.type' and a 'trigger'."));
                };
                let action = match action.to_ascii_lowercase().as_str() {
                    "rotate" => "Rotate",
                    "notify" => "Notify",
                    other => return Err(bad_parameter(&format!("'{other}' is not a valid lifetime action."))),
                };
                let Some((kind, value)) = trigger.iter().find_map(|(k, v)| match (k.as_str(), v.as_str()) {
                    ("timeAfterCreate", Some(v)) => Some(("timeAfterCreate", v)),
                    ("timeBeforeExpiry", Some(v)) => Some(("timeBeforeExpiry", v)),
                    _ => None,
                }) else {
                    return Err(bad_parameter("A lifetime action trigger needs 'timeAfterCreate' or 'timeBeforeExpiry'."));
                };
                out.push(LifetimeAction { action: action.into(), trigger_kind: kind.into(), trigger_value: value.into() });
            }
        }
        Some(_) => return Err(bad_parameter("'lifetimeActions' must be an array.")),
    }
    Ok(out)
}

impl KeyVaultService {
    pub fn get_rotation_policy(&self, req: &Request, name: &str) -> Response {
        if !crate::common::valid_name(name) {
            return crate::common::invalid_name(name);
        }
        let base = req.ctx.endpoint.base_url();
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        let Some(item) = vault.keys.active(name) else {
            return not_found(Kind::Key, name);
        };
        let policy = item
            .extra
            .policy
            .clone()
            .unwrap_or_else(|| RotationPolicy::default_at(item.versions.first().map(|v| v.attrs.created).unwrap_or(0)));
        json_response(200, &policy.to_json(&base, &item.name))
    }

    pub fn update_rotation_policy(&self, req: &Request, name: &str) -> Response {
        if !crate::common::valid_name(name) {
            return crate::common::invalid_name(name);
        }
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let sent = match parse_actions(&body) {
            Ok(a) => a,
            Err(r) => return r,
        };
        let expiry = match body.get("attributes") {
            Some(Value::Object(a)) => match a.get("expiryTime") {
                Some(Value::String(s)) => Some(s.clone()),
                None | Some(Value::Null) => None,
                Some(_) => return bad_parameter("'expiryTime' must be an ISO 8601 duration string."),
            },
            _ => None,
        };
        let now = now_secs(req);
        let base = req.ctx.endpoint.base_url();
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now);
        let Some(item) = vault.keys.active_mut(name) else {
            return not_found(Kind::Key, name);
        };
        let created = item.versions.first().map(|v| v.attrs.created).unwrap_or(now);
        let old = item.extra.policy.clone().unwrap_or_else(|| RotationPolicy::default_at(created));
        let mut actions: Vec<LifetimeAction> = sent.iter().filter(|a| a.action == "Rotate").cloned().collect();
        let mut notify: Vec<LifetimeAction> = sent.iter().filter(|a| a.action == "Notify").cloned().collect();
        if notify.is_empty() {
            notify = old.lifetime_actions.iter().filter(|a| a.action == "Notify").cloned().collect();
        }
        actions.append(&mut notify);
        let policy = RotationPolicy { lifetime_actions: actions, expiry_time: expiry, created: old.created, updated: now };
        let json = policy.to_json(&base, &item.name);
        item.extra.policy = Some(policy);
        json_response(200, &json)
    }
}
