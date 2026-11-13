//! Putting an event into an event hub from inside the emulator, for other services (Event Grid
//! delivery to an event hub). The same path as an event sent over AMQP: the service picks the
//! partition (round robin) and receivers are woken.

use azemu_amqp::message::{Message, Properties};
use azemu_amqp::types::Value;

use crate::errors::EhError;
use crate::state::{now_ms, SendTarget, Shared};

/// An event to put in. The body is one `data` section.
#[derive(Clone, Debug, Default)]
pub struct EnqueueEvent {
    pub body: Vec<u8>,
    /// AMQP `content-type` (`application/json` for Event Grid events).
    pub content_type: Option<String>,
    /// Application properties, all strings.
    pub application_properties: Vec<(String, String)>,
}

impl Shared {
    /// Put `event` into the event hub `hub` of the namespace `namespace`. The hub must exist
    /// (nothing is created). `Err` carries the condition and description a sender would get.
    pub fn enqueue_event(&self, namespace: &str, hub: &str, event: EnqueueEvent) -> Result<(), EhError> {
        let message = Message {
            properties: Some(Properties { content_type: event.content_type, ..Properties::default() }),
            application_properties: if event.application_properties.is_empty() {
                None
            } else {
                Some(event.application_properties.into_iter().map(|(k, v)| (Value::String(k), Value::String(v))).collect())
            },
            ..Message::data(event.body)
        };
        self.state.lock().store(namespace, hub, SendTarget::Any, vec![message], false, now_ms())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_lands_in_a_partition_with_body_and_properties() {
        let shared = Shared::new(false, None);
        shared.state.lock().create_hub("ns", "hub", 2, 3_600_000, now_ms()).unwrap();
        shared
            .enqueue_event(
                "ns",
                "hub",
                EnqueueEvent {
                    body: b"{}".to_vec(),
                    content_type: Some("application/json".into()),
                    application_properties: vec![("aeg-event-type".into(), "Notification".into())],
                },
            )
            .unwrap();
        let st = shared.state.lock();
        let hub = st.hub("ns", "hub").unwrap();
        let stored: Vec<_> = hub.partitions.iter().filter(|p| !p.is_empty()).collect();
        assert_eq!(stored.len(), 1);
        let ev = stored[0].get(stored[0].first_seq()).unwrap();
        assert_eq!(ev.message.properties.as_ref().unwrap().content_type.as_deref(), Some("application/json"));
        assert_eq!(ev.message.application_properties.as_ref().unwrap()[0].1.as_str(), Some("Notification"));
    }

    #[test]
    fn missing_hub_is_not_found() {
        let shared = Shared::new(true, None); // auto-creation does not apply: nothing is created here
        let e = shared.enqueue_event("ns", "nope", EnqueueEvent::default()).unwrap_err();
        assert_eq!(e.status, 404);
        assert!(shared.state.lock().hub("ns", "nope").is_none());
    }
}
