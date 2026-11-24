//! Authorization rules (shared access policies) of Service Bus and Event Hubs.
//!
//! The rules exist so templates and `listKeys` calls work and give out a connection string that reaches the emulator. In strict
//! authorisation mode (`docs/strict-auth.md`) the data planes check shared access signatures against these rules: [`ArmSas`] hands them
//! over, at every check, with the keys [`key_of`] makes, which are the keys `listKeys` gives out (so a regenerated key takes effect at once).

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

/// The key of a rule: the one function behind `listKeys`, `regenerateKeys` and the data planes' check.
pub(crate) fn key_of(rule: &Stored, which: &str) -> String {
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

/// Which service's namespaces a [`ArmSas`] reads.
#[derive(Clone, Copy)]
pub enum Service {
    ServiceBus,
    EventHubs,
}

impl Service {
    fn provider(self) -> &'static str {
        match self {
            Service::ServiceBus => "Microsoft.ServiceBus",
            Service::EventHubs => "Microsoft.EventHub",
        }
    }
    /// The type below a namespace that has its own rules: `queues` and `topics`, or `eventhubs`.
    fn entity_types(self) -> &'static [&'static str] {
        match self {
            Service::ServiceBus => &["queues", "topics"],
            Service::EventHubs => &["eventhubs"],
        }
    }
}

/// The authorization rules Resource Manager holds, as the data plane's `SasSource`.
pub struct ArmSas {
    pub(crate) inner: std::sync::Arc<crate::Inner>,
    pub(crate) service: Service,
}

fn rule_of(rule: &Stored) -> Option<azemu_core::auth::sas::SasRule> {
    use azemu_core::auth::sas::{Rights, SasRule};
    let words: Vec<&str> = rule.body.get("properties")?.get("rights")?.as_array()?.iter().filter_map(Value::as_str).collect();
    // a right Resource Manager would not have taken is no right
    let rights = Rights::parse(words.iter().copied().filter(|w| VALID_RIGHTS.contains(w))).ok()?;
    Some(SasRule { name: rule.name().to_string(), rights, primary: key_of(rule, "primary"), secondary: key_of(rule, "secondary") })
}

impl azemu_core::auth::sas::SasSource for ArmSas {
    fn rules(&self, namespace: &str, entity: Option<&str>) -> Option<Vec<azemu_core::auth::sas::SasRule>> {
        let provider = self.service.provider();
        let st = self.inner.state.lock();
        let ns = st.resources.values().find(|r| r.ns == provider && r.types.len() == 1 && r.name().eq_ignore_ascii_case(namespace))?;
        let in_ns = |r: &Stored| r.sub == ns.sub && r.rg == ns.rg && r.ns == provider && r.name_at(0).eq_ignore_ascii_case(namespace);
        let mut out = Vec::new();
        for r in st.resources.values().filter(|r| in_ns(r)) {
            let is_rule = r.types.last().map(|t| t == "authorizationRules").unwrap_or(false);
            if !is_rule {
                continue;
            }
            let matches = match entity {
                None => r.types.len() == 2,
                Some(e) => {
                    r.types.len() == 3 && self.service.entity_types().contains(&r.types[1].as_str()) && r.name_at(1).eq_ignore_ascii_case(e)
                }
            };
            if matches {
                out.extend(rule_of(r));
            }
        }
        Some(out)
    }
}
