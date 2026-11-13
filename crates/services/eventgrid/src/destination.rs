//! Destinations other than webhooks: a Storage queue, a Service Bus queue or topic, an
//! event hub.
//!
//! This crate does not depend on the services behind them. The unified binary registers one
//! handler per kind ([`EventGridService::register_destination`](crate::EventGridService::register_destination));
//! when an event is due for such a subscription, the handler is called with the finished
//! message ([`DestinationDelivery`]). A handler that returns `Err` makes the delivery fail, and
//! Event Grid's retry and dead-letter rules apply as for a webhook.
//!
//! In a subscription the destination is written as in the ARM API:
//!
//! ```json
//! { "destination": { "endpointType": "StorageQueue",
//!                    "properties": { "resourceId": ".../storageAccounts/acct", "queueName": "q",
//!                                    "queueMessageTimeToLiveInSeconds": 3600 } } }
//! ```
//!
//! or flat, with names instead of resource ids (easier to write by hand):
//!
//! ```json
//! { "destination": { "endpointType": "StorageQueue", "storageAccount": "acct", "queueName": "q" } }
//! { "destination": { "endpointType": "ServiceBusQueue", "namespace": "ns", "queueName": "q" } }
//! { "destination": { "endpointType": "ServiceBusTopic", "namespace": "ns", "topicName": "t" } }
//! { "destination": { "endpointType": "EventHub", "namespace": "ns", "eventHubName": "hub" } }
//! ```

use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The kinds of destination that need a registered handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DestinationKind {
    StorageQueue,
    ServiceBusQueue,
    ServiceBusTopic,
    EventHub,
}

impl DestinationKind {
    /// The `endpointType` name used by the ARM API.
    pub fn endpoint_type(self) -> &'static str {
        match self {
            DestinationKind::StorageQueue => "StorageQueue",
            DestinationKind::ServiceBusQueue => "ServiceBusQueue",
            DestinationKind::ServiceBusTopic => "ServiceBusTopic",
            DestinationKind::EventHub => "EventHub",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<DestinationKind> {
        match s.to_ascii_lowercase().as_str() {
            "storagequeue" => Some(DestinationKind::StorageQueue),
            "servicebusqueue" => Some(DestinationKind::ServiceBusQueue),
            "servicebustopic" => Some(DestinationKind::ServiceBusTopic),
            "eventhub" => Some(DestinationKind::EventHub),
            _ => None,
        }
    }
}

/// Where a message goes: the storage account or namespace, and the queue, topic or hub in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DestinationTarget {
    /// Storage account name (Storage queue) or namespace name (Service Bus, Event Hubs).
    pub namespace: String,
    /// Queue, topic or event hub name.
    pub entity: String,
}

/// A destination as the user wrote it (ARM style or flat).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DestinationConfig {
    #[serde(default, alias = "type")]
    pub endpoint_type: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub properties: Map<String, Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A destination that has been checked: what to call and where.
#[derive(Clone, Debug, PartialEq)]
pub enum Resolved {
    /// `endpointType: WebHook`; the URL goes to `webhookUrl` of the subscription.
    WebHook(String),
    Handler {
        kind: DestinationKind,
        target: DestinationTarget,
        /// Storage queue only: message time to live in seconds (`-1`: never expires).
        ttl_seconds: Option<i64>,
    },
}

impl DestinationConfig {
    fn text(&self, names: &[&str]) -> Option<String> {
        for n in names {
            for map in [&self.properties, &self.extra] {
                if let Some(v) = map.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v) {
                    if let Some(s) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        return Some(s.to_string());
                    }
                }
            }
        }
        None
    }

    fn number(&self, names: &[&str]) -> Option<i64> {
        for n in names {
            for map in [&self.properties, &self.extra] {
                if let Some(v) = map.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v) {
                    if let Some(i) = v.as_i64() {
                        return Some(i);
                    }
                    if let Some(i) = v.as_str().and_then(|s| s.trim().parse().ok()) {
                        return Some(i);
                    }
                }
            }
        }
        None
    }

    /// Check the destination and turn it into something deliverable. `Err` is the text of a 400.
    pub fn resolve(&self) -> Result<Resolved, String> {
        let kind_name = self.endpoint_type.trim();
        if kind_name.eq_ignore_ascii_case("webhook") {
            let url = self.text(&["endpointUrl", "url"]).ok_or("A WebHook destination needs an endpointUrl.")?;
            return Ok(Resolved::WebHook(url));
        }
        let kind = DestinationKind::parse(kind_name).ok_or_else(|| {
            if kind_name.is_empty() {
                "A destination needs an endpointType (WebHook, StorageQueue, ServiceBusQueue, ServiceBusTopic or EventHub).".to_string()
            } else {
                format!("The destination type '{kind_name}' is not supported (use WebHook, StorageQueue, ServiceBusQueue, ServiceBusTopic or EventHub).")
            }
        })?;
        let resource = self.text(&["resourceId"]);
        let (namespace, entity) = match kind {
            DestinationKind::StorageQueue => (
                resource
                    .as_deref()
                    .and_then(|r| segment_after(r, "storageAccounts"))
                    .or_else(|| self.text(&["storageAccount", "accountName", "account"])),
                self.text(&["queueName", "queue"]),
            ),
            DestinationKind::ServiceBusQueue => (
                resource
                    .as_deref()
                    .and_then(|r| segment_after(r, "namespaces"))
                    .or_else(|| self.text(&["namespace", "serviceBusNamespace"])),
                resource.as_deref().and_then(|r| segment_after(r, "queues")).or_else(|| self.text(&["queueName", "queue"])),
            ),
            DestinationKind::ServiceBusTopic => (
                resource
                    .as_deref()
                    .and_then(|r| segment_after(r, "namespaces"))
                    .or_else(|| self.text(&["namespace", "serviceBusNamespace"])),
                resource.as_deref().and_then(|r| segment_after(r, "topics")).or_else(|| self.text(&["topicName", "topic"])),
            ),
            DestinationKind::EventHub => (
                resource.as_deref().and_then(|r| segment_after(r, "namespaces")).or_else(|| self.text(&["namespace", "eventHubNamespace"])),
                resource.as_deref().and_then(|r| segment_after(r, "eventhubs")).or_else(|| self.text(&["eventHubName", "eventHub", "hub"])),
            ),
        };
        let (what_ns, what_entity, ids) = match kind {
            DestinationKind::StorageQueue => ("storage account", "queueName", "'resourceId' (the storage account) or 'storageAccount'"),
            DestinationKind::ServiceBusQueue => {
                ("namespace", "queue", "'resourceId' (.../namespaces/N/queues/Q) or 'namespace' and 'queueName'")
            }
            DestinationKind::ServiceBusTopic => {
                ("namespace", "topic", "'resourceId' (.../namespaces/N/topics/T) or 'namespace' and 'topicName'")
            }
            DestinationKind::EventHub => {
                ("namespace", "event hub", "'resourceId' (.../namespaces/N/eventhubs/H) or 'namespace' and 'eventHubName'")
            }
        };
        let namespace = namespace.ok_or_else(|| format!("The {} destination has no {what_ns}: give {ids}.", kind.endpoint_type()))?;
        let entity = entity.ok_or_else(|| format!("The {} destination has no {what_entity}: give {ids}.", kind.endpoint_type()))?;
        let ttl_seconds = if kind == DestinationKind::StorageQueue {
            match self.number(&["queueMessageTimeToLiveInSeconds", "queueMessageTtlSeconds"]) {
                Some(v) if v == -1 || v > 0 => Some(v),
                Some(_) => return Err("queueMessageTimeToLiveInSeconds must be -1 (never expires) or greater than zero.".into()),
                None => None,
            }
        } else {
            None
        };
        Ok(Resolved::Handler { kind, target: DestinationTarget { namespace, entity }, ttl_seconds })
    }
}

/// The path segment after `key` in a resource id (`.../namespaces/ns1/queues/q` and `namespaces`
/// give `ns1`). The key is matched without regard to case.
fn segment_after(resource_id: &str, key: &str) -> Option<String> {
    let segs: Vec<&str> = resource_id.trim_matches('/').split('/').collect();
    segs.iter().position(|s| s.eq_ignore_ascii_case(key)).and_then(|i| segs.get(i + 1)).filter(|s| !s.is_empty()).map(|s| s.to_string())
}

/// One finished message for a destination handler.
#[derive(Clone, Debug)]
pub struct DestinationDelivery {
    pub kind: DestinationKind,
    pub target: DestinationTarget,
    /// The event as JSON (one event, not an array), in the delivery schema of the subscription.
    pub body: Bytes,
    /// `application/json` for the Event Grid schema, `application/cloudevents+json; charset=utf-8`
    /// for CloudEvents.
    pub content_type: String,
    /// The `aeg-*` properties Event Grid sets: `aeg-subscription-name`, `aeg-delivery-count`
    /// (0 for the first attempt), `aeg-event-type` (`Notification`), `aeg-metadata-version`,
    /// `aeg-data-version`, `aeg-output-event-id`.
    pub properties: Vec<(String, String)>,
    /// The id of the event (Service Bus: the `MessageId`).
    pub event_id: String,
    pub subscription: String,
    /// 1 for the first attempt.
    pub attempt: u32,
    /// Storage queue: message time to live in seconds, `None` for the default (7 days), `-1` for
    /// never.
    pub ttl_seconds: Option<i64>,
}

/// What a destination handler does. `Err` is the reason, kept in the delivery log; the delivery
/// is retried.
pub type DestinationHandler = Arc<dyn Fn(&DestinationDelivery) -> Result<(), String> + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(v: Value) -> DestinationConfig {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn arm_style_storage_queue() {
        let c = cfg(json!({"endpointType":"StorageQueue","properties":{
            "resourceId":"/subscriptions/s/resourceGroups/r/providers/Microsoft.Storage/storageAccounts/acct",
            "queueName":"q1","queueMessageTimeToLiveInSeconds":60}}));
        assert_eq!(
            c.resolve().unwrap(),
            Resolved::Handler {
                kind: DestinationKind::StorageQueue,
                target: DestinationTarget { namespace: "acct".into(), entity: "q1".into() },
                ttl_seconds: Some(60)
            }
        );
    }

    #[test]
    fn flat_service_bus_and_event_hub() {
        let q = cfg(json!({"endpointType":"servicebusqueue","namespace":"ns","queueName":"q"})).resolve().unwrap();
        assert!(matches!(q, Resolved::Handler { kind: DestinationKind::ServiceBusQueue, .. }));
        let t = cfg(json!({"type":"ServiceBusTopic","properties":{"resourceId":"/subscriptions/s/resourceGroups/r/providers/Microsoft.ServiceBus/namespaces/ns1/topics/t1"}}))
            .resolve()
            .unwrap();
        assert_eq!(
            t,
            Resolved::Handler {
                kind: DestinationKind::ServiceBusTopic,
                target: DestinationTarget { namespace: "ns1".into(), entity: "t1".into() },
                ttl_seconds: None
            }
        );
        let h = cfg(json!({"endpointType":"EventHub","properties":{"resourceId":"/subscriptions/s/resourceGroups/r/providers/Microsoft.EventHub/namespaces/n/eventhubs/h"}}))
            .resolve()
            .unwrap();
        assert!(matches!(h, Resolved::Handler { kind: DestinationKind::EventHub, ref target, .. } if target.entity == "h"));
    }

    #[test]
    fn webhook_and_errors() {
        let w = cfg(json!({"endpointType":"WebHook","properties":{"endpointUrl":"http://h/x"}})).resolve().unwrap();
        assert_eq!(w, Resolved::WebHook("http://h/x".into()));
        assert!(cfg(json!({})).resolve().unwrap_err().contains("endpointType"));
        assert!(cfg(json!({"endpointType":"Nope"})).resolve().unwrap_err().contains("not supported"));
        assert!(cfg(json!({"endpointType":"StorageQueue","storageAccount":"a"})).resolve().unwrap_err().contains("queueName"));
        assert!(cfg(json!({"endpointType":"StorageQueue","storageAccount":"a","queueName":"q","queueMessageTimeToLiveInSeconds":0}))
            .resolve()
            .is_err());
    }
}
