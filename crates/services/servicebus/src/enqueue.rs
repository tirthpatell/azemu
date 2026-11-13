//! Putting a message into a queue or topic from inside the emulator, for other services
//! (Event Grid delivery to a Service Bus queue or topic). The same path as a message sent over
//! AMQP: entity limits, duplicate detection, topic rules and auto-forwarding all apply.

use std::sync::Arc;

use azemu_amqp::message::{Message, Properties};
use azemu_amqp::types::Value;

use crate::core::Incoming;
use crate::errors::SbError;
use crate::model::{Address, SubQueue};
use crate::state::{now_ms, Shared};

/// Where the message goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnqueueTarget<'a> {
    Queue(&'a str),
    Topic(&'a str),
}

/// A message to put in. The body is one `data` section.
#[derive(Clone, Debug, Default)]
pub struct EnqueueMessage {
    pub body: Vec<u8>,
    /// AMQP `content-type` (`application/json` for Event Grid events).
    pub content_type: Option<String>,
    /// AMQP `message-id` (Service Bus `MessageId`).
    pub message_id: Option<String>,
    /// Application properties, all strings.
    pub application_properties: Vec<(String, String)>,
}

impl Shared {
    /// Put `msg` into the queue or topic `target` of the namespace `namespace`. The entity must
    /// exist (nothing is created). `Err` carries the condition and description Service Bus would
    /// answer a sender with.
    pub fn enqueue_message(self: &Arc<Shared>, namespace: &str, target: EnqueueTarget<'_>, msg: EnqueueMessage) -> Result<(), SbError> {
        self.ensure_ticker();
        let message = Message {
            properties: Some(Properties {
                message_id: msg.message_id.map(Value::String),
                content_type: msg.content_type,
                ..Properties::default()
            }),
            application_properties: if msg.application_properties.is_empty() {
                None
            } else {
                Some(msg.application_properties.into_iter().map(|(k, v)| (Value::String(k), Value::String(v))).collect())
            },
            ..Message::data(msg.body)
        };
        let size = message.encode().len();
        let addr = match target {
            EnqueueTarget::Queue(name) => Address::Queue { name: name.to_string(), sub: SubQueue::Main },
            EnqueueTarget::Topic(name) => Address::Topic { name: name.to_string() },
        };
        let mut st = self.state.lock();
        let exists = {
            let ns = st.ns(namespace);
            match target {
                EnqueueTarget::Queue(name) => ns.queue(name).is_some(),
                EnqueueTarget::Topic(name) => ns.topic(name).is_some(),
            }
        };
        if !exists {
            let name = match target {
                EnqueueTarget::Queue(n) | EnqueueTarget::Topic(n) => n,
            };
            return Err(SbError::entity_not_found(name, namespace));
        }
        st.send(namespace, &addr, Incoming::new(message, size), now_ms())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Settings;
    use crate::rules::Rule;

    fn message() -> EnqueueMessage {
        EnqueueMessage {
            body: b"{\"id\":\"e1\"}".to_vec(),
            content_type: Some("application/json".into()),
            message_id: Some("e1".into()),
            application_properties: vec![("aeg-delivery-count".into(), "0".into())],
        }
    }

    #[test]
    fn queue_message_has_body_content_type_id_and_properties() {
        let shared = Shared::new(false, None);
        let now = now_ms();
        shared.state.lock().ns("ns1").create_queue("q", Settings::queue_default(now)).unwrap();
        shared.enqueue_message("NS1", EnqueueTarget::Queue("q"), message()).unwrap();
        let mut st = shared.state.lock();
        let core = &mut st.ns("ns1").queue_mut("q").unwrap().core;
        let msgs = core.peek(SubQueue::Main, 0, 10, None, now_ms());
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0].message;
        let p = m.properties.as_ref().unwrap();
        assert_eq!(p.content_type.as_deref(), Some("application/json"));
        assert_eq!(p.message_id, Some(Value::String("e1".into())));
        let props = m.application_properties.as_ref().unwrap();
        assert_eq!(props[0].0.as_str(), Some("aeg-delivery-count"));
        assert_eq!(props[0].1.as_str(), Some("0"));
    }

    #[test]
    fn topic_fans_out_and_missing_entities_are_not_found() {
        let shared = Shared::new(false, None);
        let now = now_ms();
        {
            let mut st = shared.state.lock();
            let ns = st.ns("n");
            ns.create_topic("t", Settings::topic_default(now)).unwrap();
            ns.create_subscription("t", "s1", Settings::subscription_default(now), vec![Rule::default_rule(now)]).unwrap();
            ns.create_subscription("t", "s2", Settings::subscription_default(now), vec![Rule::default_rule(now)]).unwrap();
        }
        shared.enqueue_message("n", EnqueueTarget::Topic("t"), message()).unwrap();
        let mut st = shared.state.lock();
        for sub in ["s1", "s2"] {
            let core = &mut st.ns("n").topic_mut("t").unwrap().subs.get_mut(sub).unwrap().core;
            assert_eq!(core.peek(SubQueue::Main, 0, 10, None, now_ms()).len(), 1, "subscription {sub}");
        }
        drop(st);
        let e = shared.enqueue_message("n", EnqueueTarget::Queue("missing"), message()).unwrap_err();
        assert_eq!(e.status, 404);
        let e = shared.enqueue_message("n", EnqueueTarget::Topic("nope"), message()).unwrap_err();
        assert_eq!(e.condition, crate::errors::ENTITY_NOT_FOUND);
    }
}
