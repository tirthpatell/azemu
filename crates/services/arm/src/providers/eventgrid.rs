//! Microsoft.EventGrid: custom topics and system topics. Topics are created in the Event Grid emulator
//! through its admin API. A system topic names a source (a storage account, a key vault or an App
//! Configuration store) and holds nothing in the service; its subscriptions are in `eventsubs.rs`,
//! which also handles the subscriptions on topics and on resources.

use azemu_core::{async_trait, Response};
use serde_json::{json, Value};

use super::{bump_generation, dp_error, generation};
use crate::backends::{call, Backends, Call};
use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::provider::{ActionTarget, Provider, Statuses};
use crate::state::Stored;
use crate::util;

pub struct EventGrid {
    pub b: Backends,
}

fn valid_topic_name(n: &str) -> bool {
    (3..=50).contains(&n.len()) && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

impl EventGrid {
    /// `properties.source` (a storage account, a key vault or an App Configuration store that ARM knows) and
    /// `topicType`, which has to match it.
    fn put_system_topic(&self, cx: &Cx<'_>, res: &mut Stored, existed: bool) -> ArmResult<()> {
        let source = res.prop_str("source").unwrap_or("").to_string();
        if source.is_empty() {
            return Err(ArmError::bad_request(
                "InvalidRequest",
                "A system topic needs properties.source, the resource id of the resource that raises the events.",
            ));
        }
        let found = cx.inner.state.lock().resources.get(&crate::state::key(&source)).cloned();
        let source_type = match &found {
            Some(f) => f.type_string(),
            // Not stored: take the type from the id so the error says what is wrong.
            None => {
                let segs: Vec<&str> = source.trim_matches('/').split('/').collect();
                match segs.iter().position(|s| s.eq_ignore_ascii_case("providers")) {
                    Some(i) if segs.len() > i + 2 => format!("{}/{}", segs[i + 1], segs[i + 2]),
                    _ => return Err(ArmError::bad_request("InvalidRequest", format!("'{source}' is not a resource id."))),
                }
            }
        };
        let Some(expected) = crate::eventsubs::topic_type_of(&source_type) else {
            return Err(ArmError::not_implemented(format!(
                "system topics for {source_type}: the emulator raises events for storage accounts, key vaults and App Configuration stores"
            )));
        };
        if found.is_none() {
            return Err(ArmError::not_found("ResourceNotFound", format!("The source resource '{source}' was not found.")));
        }
        let p = util::props_mut(&mut res.body);
        match p.get("topicType").and_then(Value::as_str) {
            Some(t) if !t.eq_ignore_ascii_case(expected) => {
                return Err(ArmError::bad_request(
                    "InvalidRequest",
                    format!("The topic type '{t}' does not fit the source {source_type} (expected '{expected}')."),
                ))
            }
            Some(_) => {}
            None => {
                p.insert("topicType".into(), json!(expected));
            }
        }
        if !existed {
            p.insert("metricResourceId".into(), json!(uuid::Uuid::new_v4().to_string()));
        }
        Ok(())
    }

    fn keys(&self, res: &Stored) -> Value {
        let seed = res.id.to_ascii_lowercase();
        json!({
            "key1": util::derive_key(&format!("eventgrid/{seed}/key1"), generation(res, "key1")),
            "key2": util::derive_key(&format!("eventgrid/{seed}/key2"), generation(res, "key2")),
        })
    }
}

#[async_trait]
impl Provider for EventGrid {
    fn namespace(&self) -> &'static str {
        "Microsoft.EventGrid"
    }

    fn types(&self) -> &'static [&'static str] {
        &["topics", "systemTopics"]
    }

    fn statuses(&self, type_path: &str) -> Statuses {
        match type_path {
            // The API spec lists 200 and 201 for PUT, 200, 202 and 204 for DELETE.
            "systemTopics" => Statuses { create: 201, update: 200, delete: 200, delete_missing: 204 },
            // The API spec lists 201 only for PUT, and 202 or 204 for DELETE.
            _ => Statuses { create: 201, update: 201, delete: 204, delete_missing: 204 },
        }
    }

    /// A custom topic is a host name, so it is unique; system topic names are unique per group.
    fn global_name(&self, type_path: &str) -> bool {
        type_path == "topics"
    }

    fn validate_name(&self, type_path: &str, name: &str) -> ArmResult<()> {
        if type_path == "systemTopics" {
            return if (3..=128).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                Ok(())
            } else {
                Err(ArmError::bad_request(
                    "InvalidSystemTopicName",
                    format!("The system topic name '{name}' is invalid. Use 3 to 128 letters, numbers and hyphens."),
                ))
            };
        }
        if !valid_topic_name(name) {
            return Err(ArmError::bad_request(
                "InvalidTopicName",
                format!("The topic name '{name}' is invalid. Use 3 to 50 letters, numbers and hyphens."),
            ));
        }
        Ok(())
    }

    async fn put(&self, cx: &Cx<'_>, res: &mut Stored, existed: bool, _children: &mut Vec<Stored>) -> ArmResult<()> {
        if res.type_path() == "systemTopics" {
            return self.put_system_topic(cx, res, existed);
        }
        let res_id = res.id.clone();
        let p = util::props_mut(&mut res.body);
        util::set_default(p, "inputSchema", json!("EventGridSchema"));
        util::set_default(p, "publicNetworkAccess", json!("Enabled"));
        util::set_default(p, "disableLocalAuth", json!(false));
        let schema = p.get("inputSchema").and_then(Value::as_str).unwrap_or("EventGridSchema").to_string();
        if !matches!(schema.as_str(), "EventGridSchema" | "CloudEventSchemaV1_0" | "CustomEventSchema") {
            return Err(ArmError::bad_request("InvalidInputSchema", format!("The input schema '{schema}' is not valid.")));
        }
        if !existed {
            p.insert("metricResourceId".into(), json!(uuid::Uuid::new_v4().to_string()));
        }
        if let Some(eg) = self.b.eventgrid_service() {
            let suffix = cx.port_suffix();
            let path = format!("/_azemu/eventgrid/topics/{}", res.name());
            let r = call(
                &eg,
                Call {
                    method: "PUT",
                    path: &path,
                    query: "",
                    headers: vec![("Content-Type", "application/json".into())],
                    body: json!({"kind": "custom", "inputSchema": schema, "resourceId": res_id}).to_string().into_bytes(),
                    label: "eventgrid",
                    account: "",
                    port_suffix: &suffix,
                },
            )
            .await;
            if r.status().as_u16() >= 400 {
                return Err(dp_error(&r));
            }
        }
        Ok(())
    }

    async fn delete(&self, cx: &Cx<'_>, res: &Stored, _force: bool) -> ArmResult<()> {
        if res.type_path().ends_with("eventSubscriptions") {
            return crate::eventsubs::delete_backend(cx, res).await;
        }
        // A system topic holds nothing in the service (its subscriptions were removed with it).
        if res.type_path() == "systemTopics" {
            return Ok(());
        }
        if let Some(eg) = self.b.eventgrid_service() {
            let suffix = cx.port_suffix();
            let path = format!("/_azemu/eventgrid/topics/{}", res.name());
            let r = call(
                &eg,
                Call {
                    method: "DELETE",
                    path: &path,
                    query: "",
                    headers: vec![],
                    body: vec![],
                    label: "eventgrid",
                    account: "",
                    port_suffix: &suffix,
                },
            )
            .await;
            let s = r.status().as_u16();
            if s >= 400 && s != 404 {
                return Err(dp_error(&r));
            }
        }
        Ok(())
    }

    fn render(&self, cx: &Cx<'_>, res: &Stored) -> Value {
        let mut v = res.body.clone();
        if res.type_path() == "systemTopics" {
            return v;
        }
        util::props_mut(&mut v).insert("endpoint".into(), json!(format!("{}/api/events", cx.data_url("https", res.name(), "eventgrid"))));
        v
    }

    async fn action(
        &self,
        cx: &Cx<'_>,
        target: ActionTarget<'_>,
        _type_path: &str,
        action: &str,
        body: &Value,
    ) -> ArmResult<Option<Response>> {
        let ActionTarget::Resource(res) = target else { return Ok(None) };
        match action.to_ascii_lowercase().as_str() {
            "listkeys" | "listsharedaccesskeys" => Ok(Some(util::json_ok(200, &self.keys(res)))),
            "regeneratekey" => {
                let which = body.get("keyName").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
                if which != "key1" && which != "key2" {
                    return Err(ArmError::bad_request("InvalidKeyName", "keyName must be key1 or key2."));
                }
                bump_generation(cx, &res.id, &which);
                let fresh = cx.inner.state.lock().resources.get(&crate::state::key(&res.id)).cloned().unwrap_or_else(|| res.clone());
                Ok(Some(util::json_ok(200, &self.keys(&fresh))))
            }
            _ => Ok(None),
        }
    }
}
