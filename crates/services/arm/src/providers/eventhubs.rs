//! Microsoft.EventHub: namespaces, event hubs, consumer groups and authorization rules.
//! Event hubs are created in the Event Hubs emulator's own state, so they work over AMQP
//! at once (use host `{namespace}.eventhub.localhost`).

use azemu_core::{async_trait, Response};
use azemu_eventhubs::model::{key as eh_key, DEFAULT_PARTITIONS, DEFAULT_RETENTION_MS, MAX_PARTITIONS};
use azemu_eventhubs::state::now_ms;
use serde_json::{json, Value};

use super::{sas, stored_child};
use crate::backends::Backends;
use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::provider::{ActionTarget, Provider, Statuses};
use crate::state::Stored;
use crate::util;

pub struct EventHubs {
    pub b: Backends,
}

const TYPES: &[&str] = &[
    "namespaces",
    "namespaces/eventhubs",
    "namespaces/eventhubs/consumergroups",
    "namespaces/authorizationRules",
    "namespaces/eventhubs/authorizationRules",
];

const PASSTHROUGH: &[&str] = &["namespaces/networkRuleSets"];

fn valid_namespace_name(n: &str) -> bool {
    let b = n.as_bytes();
    (6..=50).contains(&b.len())
        && b[0].is_ascii_alphabetic()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Microsoft Learn, "Naming rules and restrictions": hubs 1 to 256 characters, consumer groups and authorization rules 1 to 50, letters,
/// digits, periods, hyphens and underscores, starting and ending with a letter or digit.
fn valid_entity_name(n: &str, max: usize) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= max
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

fn bad(msg: impl Into<String>) -> ArmError {
    ArmError::bad_request("BadRequest", msg)
}

fn display_segment(s: &str) -> String {
    match s {
        "eventhubs" => "eventhubs".to_string(),
        "consumergroups" => "consumergroups".to_string(),
        other => {
            let mut c = other.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        }
    }
}

impl EventHubs {
    fn put_namespace(&self, cx: &Cx<'_>, res: &mut Stored, existed: bool, children: &mut Vec<Stored>) {
        let now = cx.now_iso();
        let sku_name = res.body.get("sku").and_then(|s| s.get("name")).and_then(Value::as_str).unwrap_or("Standard").to_string();
        let capacity = res.body.get("sku").and_then(|s| s.get("capacity")).cloned().unwrap_or(json!(1));
        res.body["sku"] = json!({"name": sku_name, "tier": sku_name, "capacity": capacity});
        let sub = res.sub.clone();
        let name = res.name().to_string();
        let p = util::props_mut(&mut res.body);
        util::set_default(p, "status", json!("Active"));
        if !existed {
            p.insert("createdAt".into(), json!(now));
        }
        p.insert("updatedAt".into(), json!(now));
        p.insert("metricId".into(), json!(format!("{sub}:{}", name.to_ascii_lowercase())));
        util::set_default(p, "isAutoInflateEnabled", json!(false));
        util::set_default(p, "maximumThroughputUnits", json!(0));
        util::set_default(p, "kafkaEnabled", json!(false));
        util::set_default(p, "zoneRedundant", json!(false));
        util::set_default(p, "disableLocalAuth", json!(false));
        util::set_default(p, "minimumTlsVersion", json!("1.2"));
        util::set_default(p, "publicNetworkAccess", json!("Enabled"));
        util::set_default(p, "privateEndpointConnections", json!([]));
        if let Some(eh) = &self.b.eventhubs {
            eh.state.lock().declare_namespace(&name);
        }
        if !existed {
            children.push(sas::root_rule(res));
            children.push(stored_child(
                res,
                "networkRuleSets",
                "default",
                json!({"properties": {"publicNetworkAccess": "Enabled", "defaultAction": "Allow", "trustedServiceAccessEnabled": false, "virtualNetworkRules": [], "ipRules": []}}),
            ));
        }
    }

    fn put_hub(&self, cx: &Cx<'_>, res: &mut Stored, existed: bool, children: &mut Vec<Stored>) -> ArmResult<()> {
        let now_iso = cx.now_iso();
        let ns = res.name_at(0).to_string();
        let name = res.name().to_string();
        let props = res.body.get("properties").cloned().unwrap_or(json!({}));
        let asked_partitions = props.get("partitionCount").and_then(Value::as_u64);
        let days = props.get("messageRetentionInDays").and_then(Value::as_i64);
        if let Some(n) = asked_partitions {
            if n == 0 || n > MAX_PARTITIONS as u64 {
                return Err(bad(format!("partitionCount must be between 1 and {MAX_PARTITIONS}.")));
            }
        }
        if let Some(d) = days {
            if d < 1 {
                return Err(bad("messageRetentionInDays must be at least 1."));
            }
        }
        let retention_ms = days.map(|d| d * 86_400_000).unwrap_or(DEFAULT_RETENTION_MS);
        let mut partitions = asked_partitions.map(|n| n as u32).unwrap_or(DEFAULT_PARTITIONS);
        if let Some(eh) = &self.b.eventhubs {
            let mut st = eh.state.lock();
            let now = now_ms();
            if let Some(h) = st.hub_mut(&ns, &name) {
                let have = h.partitions.len() as u32;
                if existed && asked_partitions.is_some() && have != partitions {
                    return Err(bad("The partition count of an existing event hub cannot be changed."));
                }
                partitions = have;
                if days.is_some() {
                    h.retention_ms = retention_ms;
                }
            } else {
                st.declare_namespace(&ns);
                st.create_hub(&ns, &name, partitions, retention_ms, now).map_err(|e| bad(e.description))?;
            }
        }
        let p = util::props_mut(&mut res.body);
        p.insert("partitionCount".into(), json!(partitions));
        p.insert("partitionIds".into(), json!((0..partitions).map(|i| i.to_string()).collect::<Vec<_>>()));
        util::set_default(p, "messageRetentionInDays", json!(retention_ms / 86_400_000));
        util::set_default(p, "status", json!("Active"));
        util::set_default(p, "createdAt", json!(now_iso));
        p.insert("updatedAt".into(), json!(now_iso));
        if !existed {
            children.push(stored_child(
                res,
                "consumergroups",
                "$Default",
                json!({"properties": {"createdAt": now_iso, "updatedAt": now_iso}}),
            ));
        }
        Ok(())
    }

    fn put_consumer_group(&self, cx: &Cx<'_>, res: &mut Stored) -> ArmResult<()> {
        let now_iso = cx.now_iso();
        let ns = res.name_at(0).to_string();
        let hub = res.name_at(1).to_string();
        let name = res.name().to_string();
        if let Some(eh) = &self.b.eventhubs {
            let mut st = eh.state.lock();
            match st.hub_mut(&ns, &hub) {
                Some(h) => {
                    if !h.has_consumer_group(&name) && h.consumer_groups.len() >= azemu_eventhubs::model::MAX_CONSUMER_GROUPS {
                        return Err(bad(format!(
                            "The event hub '{hub}' already has the most consumer groups it can have ({}).",
                            azemu_eventhubs::model::MAX_CONSUMER_GROUPS
                        )));
                    }
                    h.add_consumer_group(&name);
                }
                None => return Err(bad(format!("The event hub '{hub}' does not exist in the Event Hubs emulator."))),
            }
        }
        let p = util::props_mut(&mut res.body);
        util::set_default(p, "createdAt", json!(now_iso));
        p.insert("updatedAt".into(), json!(now_iso));
        Ok(())
    }
}

#[async_trait]
impl Provider for EventHubs {
    fn namespace(&self) -> &'static str {
        "Microsoft.EventHub"
    }

    fn types(&self) -> &'static [&'static str] {
        TYPES
    }

    fn passthrough(&self) -> &'static [&'static str] {
        PASSTHROUGH
    }

    fn statuses(&self, _type_path: &str) -> Statuses {
        Statuses { create: 200, update: 200, delete: 200, delete_missing: 204 }
    }

    fn validate_name(&self, type_path: &str, name: &str) -> ArmResult<()> {
        if type_path == "namespaces" && !valid_namespace_name(name) {
            return Err(bad(format!(
                "The specified service namespace is invalid. The name '{name}' must be 6 to 50 characters, start with a letter, end with a letter or number and use only letters, numbers and hyphens."
            )));
        }
        // the name goes into the Event Hubs service and into its addresses: only what Azure allows
        let ok = match type_path {
            "namespaces/eventhubs" => valid_entity_name(name, 256),
            "namespaces/eventhubs/consumergroups" => valid_entity_name(name, 50) || name == "$Default",
            "namespaces/authorizationRules" | "namespaces/eventhubs/authorizationRules" => valid_entity_name(name, 50),
            _ => true,
        };
        if !ok {
            return Err(bad(format!(
                "The name '{name}' is not valid. Use letters, numbers, periods, hyphens and underscores, start and end with a letter or number (event hubs up to 256 characters, consumer groups and authorization rules up to 50)."
            )));
        }
        Ok(())
    }

    async fn put(&self, cx: &Cx<'_>, res: &mut Stored, existed: bool, children: &mut Vec<Stored>) -> ArmResult<()> {
        match res.type_path().as_str() {
            "namespaces" => {
                self.put_namespace(cx, res, existed, children);
                Ok(())
            }
            "namespaces/eventhubs" => self.put_hub(cx, res, existed, children),
            "namespaces/eventhubs/consumergroups" => self.put_consumer_group(cx, res),
            "namespaces/authorizationRules" | "namespaces/eventhubs/authorizationRules" => sas::prepare(res),
            _ => Ok(()),
        }
    }

    async fn delete(&self, _cx: &Cx<'_>, res: &Stored, _force: bool) -> ArmResult<()> {
        let Some(eh) = &self.b.eventhubs else { return Ok(()) };
        let ns = res.name_at(0).to_string();
        let name = res.name().to_string();
        match res.type_path().as_str() {
            // the event hubs of the namespace, with what Kafka remembers of them
            "namespaces" => eh.delete_namespace(&ns),
            "namespaces/eventhubs" => {
                eh.delete_hub(&ns, &name);
            }
            "namespaces/eventhubs/consumergroups" => {
                let hub = res.name_at(1).to_string();
                let mut st = eh.state.lock();
                if let Some(h) = st.hub_mut(&ns, &hub) {
                    if !name.eq_ignore_ascii_case("$Default") {
                        h.consumer_groups.remove(&eh_key(&name));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn render(&self, cx: &Cx<'_>, res: &Stored) -> Value {
        let mut v = res.body.clone();
        v["type"] = json!(format!("{}/{}", res.ns, res.types.iter().map(|t| display_segment(t)).collect::<Vec<_>>().join("/")));
        if res.type_path() == "namespaces" {
            let n = res.name();
            util::props_mut(&mut v).insert("serviceBusEndpoint".into(), json!(format!("{}/", cx.data_url("https", n, "eventhub"))));
        }
        v
    }

    async fn action(
        &self,
        cx: &Cx<'_>,
        target: ActionTarget<'_>,
        type_path: &str,
        action: &str,
        body: &Value,
    ) -> ArmResult<Option<Response>> {
        let a = action.to_ascii_lowercase();
        match target {
            ActionTarget::Provider if a == "checknameavailability" => {
                let name = body.get("name").and_then(Value::as_str).unwrap_or("");
                let v = if !valid_namespace_name(name) {
                    util::name_availability(false, "InvalidName", "The specified service namespace is invalid.")
                } else if cx
                    .inner
                    .state
                    .lock()
                    .resources
                    .values()
                    .any(|r| r.ns == "Microsoft.EventHub" && r.types.len() == 1 && r.name().eq_ignore_ascii_case(name))
                {
                    util::name_availability(false, "NameInUse", "The specified service namespace is already in use.")
                } else {
                    util::name_availability(true, "", "")
                };
                Ok(Some(util::json_ok(200, &v)))
            }
            ActionTarget::Resource(rule) if type_path.ends_with("authorizationRules") => {
                let ns = rule.name_at(0);
                let entity = if rule.types.len() == 3 { Some(rule.name_at(1)) } else { None };
                match a.as_str() {
                    "listkeys" => Ok(Some(sas::list_keys(cx, rule, "eventhub", ns, entity))),
                    "regeneratekeys" => Ok(Some(sas::regenerate(cx, rule, body, "eventhub", ns, entity)?)),
                    _ => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }
}
