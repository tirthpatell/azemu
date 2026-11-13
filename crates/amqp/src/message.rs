//! The AMQP message format (part 3, section 3.2): bare and annotated message sections.

use crate::codec::{decode_budgeted, encode, value_budget, DecodeError};
use crate::fields::{described, opt, Fields};
use crate::types::Value;

pub const DESC_HEADER: u64 = 0x70;
pub const DESC_DELIVERY_ANNOTATIONS: u64 = 0x71;
pub const DESC_MESSAGE_ANNOTATIONS: u64 = 0x72;
pub const DESC_PROPERTIES: u64 = 0x73;
pub const DESC_APPLICATION_PROPERTIES: u64 = 0x74;
pub const DESC_DATA: u64 = 0x75;
pub const DESC_AMQP_SEQUENCE: u64 = 0x76;
pub const DESC_AMQP_VALUE: u64 = 0x77;
pub const DESC_FOOTER: u64 = 0x78;

/// Message format of a transfer whose payload is a batch of encoded messages, each inside a
/// `data` section (the Service Bus batch format).
pub const BATCH_MESSAGE_FORMAT: u32 = 0x80013700;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Header {
    pub durable: Option<bool>,
    pub priority: Option<u8>,
    /// Milliseconds.
    pub ttl: Option<u32>,
    pub first_acquirer: Option<bool>,
    pub delivery_count: Option<u32>,
}

impl Header {
    pub fn to_value(&self) -> Value {
        described(
            DESC_HEADER,
            vec![
                opt(self.durable),
                self.priority.map(Value::Ubyte).unwrap_or(Value::Null),
                opt(self.ttl),
                opt(self.first_acquirer),
                opt(self.delivery_count),
            ],
        )
    }
    pub fn from_value(v: &Value) -> Result<Header, DecodeError> {
        let f = Fields::of(v, DESC_HEADER, "amqp:header:list")?;
        Ok(Header {
            durable: f.opt_bool(0)?,
            priority: f.opt_u8(1)?,
            ttl: f.opt_u32(2)?,
            first_acquirer: f.opt_bool(3)?,
            delivery_count: f.opt_u32(4)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Properties {
    /// ulong, uuid, binary or string.
    pub message_id: Option<Value>,
    pub user_id: Option<Vec<u8>>,
    pub to: Option<String>,
    pub subject: Option<String>,
    pub reply_to: Option<String>,
    /// Same types as `message_id`.
    pub correlation_id: Option<Value>,
    pub content_type: Option<String>,
    pub content_encoding: Option<String>,
    /// Milliseconds since the epoch.
    pub absolute_expiry_time: Option<i64>,
    pub creation_time: Option<i64>,
    pub group_id: Option<String>,
    pub group_sequence: Option<u32>,
    pub reply_to_group_id: Option<String>,
}

impl Properties {
    pub fn to_value(&self) -> Value {
        described(
            DESC_PROPERTIES,
            vec![
                self.message_id.clone().unwrap_or(Value::Null),
                self.user_id.clone().map(Value::Binary).unwrap_or(Value::Null),
                opt(self.to.clone()),
                opt(self.subject.clone()),
                opt(self.reply_to.clone()),
                self.correlation_id.clone().unwrap_or(Value::Null),
                self.content_type.clone().map(Value::Symbol).unwrap_or(Value::Null),
                self.content_encoding.clone().map(Value::Symbol).unwrap_or(Value::Null),
                self.absolute_expiry_time.map(Value::Timestamp).unwrap_or(Value::Null),
                self.creation_time.map(Value::Timestamp).unwrap_or(Value::Null),
                opt(self.group_id.clone()),
                opt(self.group_sequence),
                opt(self.reply_to_group_id.clone()),
            ],
        )
    }
    pub fn from_value(v: &Value) -> Result<Properties, DecodeError> {
        let f = Fields::of(v, DESC_PROPERTIES, "amqp:properties:list")?;
        Ok(Properties {
            message_id: f.opt_value(0),
            user_id: f.opt_binary(1)?,
            to: f.opt_string(2)?,
            subject: f.opt_string(3)?,
            reply_to: f.opt_string(4)?,
            correlation_id: f.opt_value(5),
            content_type: f.opt_string(6)?,
            content_encoding: f.opt_string(7)?,
            absolute_expiry_time: f.get(8).as_i64(),
            creation_time: f.get(9).as_i64(),
            group_id: f.opt_string(10)?,
            group_sequence: f.opt_u32(11)?,
            reply_to_group_id: f.opt_string(12)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Body {
    #[default]
    Empty,
    /// One or more `data` sections.
    Data(Vec<Vec<u8>>),
    /// One or more `amqp-sequence` sections (each a list).
    Sequence(Vec<Vec<Value>>),
    /// A single `amqp-value` section.
    Value(Value),
    /// One `data` section whose content is AMQP null. Not valid AMQP (a data section holds
    /// binary), but the JavaScript Event Hubs SDK sends it for an event with a null body and Event
    /// Hubs takes it. Kept as it was sent.
    NullData,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Message {
    pub header: Option<Header>,
    pub delivery_annotations: Option<Vec<(Value, Value)>>,
    pub message_annotations: Option<Vec<(Value, Value)>>,
    pub properties: Option<Properties>,
    pub application_properties: Option<Vec<(Value, Value)>>,
    pub body: Body,
    pub footer: Option<Vec<(Value, Value)>>,
}

fn map_of(v: &Value) -> Result<Vec<(Value, Value)>, DecodeError> {
    match v {
        Value::Map(m) => Ok(m.clone()),
        Value::Null => Ok(Vec::new()),
        _ => Err(DecodeError::Invalid("message section is not a map")),
    }
}

impl Message {
    pub fn data(body: Vec<u8>) -> Message {
        Message { body: Body::Data(vec![body]), ..Default::default() }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        if let Some(h) = &self.header {
            encode(&h.to_value(), out);
        }
        if let Some(m) = &self.delivery_annotations {
            encode(&Value::described_code(DESC_DELIVERY_ANNOTATIONS, Value::Map(m.clone())), out);
        }
        if let Some(m) = &self.message_annotations {
            encode(&Value::described_code(DESC_MESSAGE_ANNOTATIONS, Value::Map(m.clone())), out);
        }
        if let Some(p) = &self.properties {
            encode(&p.to_value(), out);
        }
        if let Some(m) = &self.application_properties {
            encode(&Value::described_code(DESC_APPLICATION_PROPERTIES, Value::Map(m.clone())), out);
        }
        match &self.body {
            Body::Empty => {}
            Body::Data(parts) => {
                for p in parts {
                    out.extend_from_slice(&[0x00, 0x53, DESC_DATA as u8]);
                    encode(&Value::Binary(p.clone()), out);
                }
            }
            Body::Sequence(seqs) => {
                for s in seqs {
                    encode(&Value::described_code(DESC_AMQP_SEQUENCE, Value::List(s.clone())), out);
                }
            }
            Body::Value(v) => encode(&Value::described_code(DESC_AMQP_VALUE, v.clone()), out),
            Body::NullData => out.extend_from_slice(&[0x00, 0x53, DESC_DATA as u8, 0x40]),
        }
        if let Some(m) = &self.footer {
            encode(&Value::described_code(DESC_FOOTER, Value::Map(m.clone())), out);
        }
    }

    /// Decode all sections of a message from a transfer payload.
    pub fn decode(payload: &[u8]) -> Result<Message, DecodeError> {
        Message::decode_budgeted(payload, &mut value_budget(payload.len()))
    }

    /// Like `decode`, spending from a budget of values the caller shares with other decodes (all
    /// sections of the message, and for a batch all its messages, draw on one).
    pub fn decode_budgeted(payload: &[u8], budget: &mut usize) -> Result<Message, DecodeError> {
        let mut msg = Message::default();
        let mut rest = payload;
        while !rest.is_empty() {
            let (v, used) = decode_budgeted(rest, budget)?;
            rest = &rest[used..];
            let d = v.as_described().ok_or(DecodeError::Invalid("message section is not described"))?;
            let code = d.descriptor.as_u64().or_else(|| match d.descriptor.as_str() {
                Some("amqp:header:list") => Some(DESC_HEADER),
                Some("amqp:delivery-annotations:map") => Some(DESC_DELIVERY_ANNOTATIONS),
                Some("amqp:message-annotations:map") => Some(DESC_MESSAGE_ANNOTATIONS),
                Some("amqp:properties:list") => Some(DESC_PROPERTIES),
                Some("amqp:application-properties:map") => Some(DESC_APPLICATION_PROPERTIES),
                Some("amqp:data:binary") => Some(DESC_DATA),
                Some("amqp:amqp-sequence:list") => Some(DESC_AMQP_SEQUENCE),
                Some("amqp:amqp-value:*") => Some(DESC_AMQP_VALUE),
                Some("amqp:footer:map") => Some(DESC_FOOTER),
                _ => None,
            });
            match code {
                Some(DESC_HEADER) => msg.header = Some(Header::from_value(&v)?),
                Some(DESC_DELIVERY_ANNOTATIONS) => msg.delivery_annotations = Some(map_of(&d.value)?),
                Some(DESC_MESSAGE_ANNOTATIONS) => msg.message_annotations = Some(map_of(&d.value)?),
                Some(DESC_PROPERTIES) => msg.properties = Some(Properties::from_value(&v)?),
                Some(DESC_APPLICATION_PROPERTIES) => msg.application_properties = Some(map_of(&d.value)?),
                Some(DESC_DATA) => {
                    let bytes = match &d.value {
                        Value::Binary(b) => Some(b.clone()),
                        Value::Null => None,
                        _ => return Err(DecodeError::Invalid("data section is not binary")),
                    };
                    match (&mut msg.body, bytes) {
                        (Body::Data(parts), b) => parts.push(b.unwrap_or_default()),
                        (Body::Empty, Some(b)) => msg.body = Body::Data(vec![b]),
                        (Body::Empty, None) => msg.body = Body::NullData,
                        (Body::NullData, b) => msg.body = Body::Data(vec![Vec::new(), b.unwrap_or_default()]),
                        _ => return Err(DecodeError::Invalid("mixed body section types")),
                    }
                }
                Some(DESC_AMQP_SEQUENCE) => {
                    let items = match &d.value {
                        Value::List(l) => l.clone(),
                        _ => return Err(DecodeError::Invalid("amqp-sequence section is not a list")),
                    };
                    match &mut msg.body {
                        Body::Sequence(parts) => parts.push(items),
                        Body::Empty => msg.body = Body::Sequence(vec![items]),
                        _ => return Err(DecodeError::Invalid("mixed body section types")),
                    }
                }
                Some(DESC_AMQP_VALUE) => {
                    if !matches!(msg.body, Body::Empty) {
                        return Err(DecodeError::Invalid("mixed body section types"));
                    }
                    msg.body = Body::Value(d.value.clone());
                }
                Some(DESC_FOOTER) => msg.footer = Some(map_of(&d.value)?),
                _ => return Err(DecodeError::Invalid("unknown message section")),
            }
        }
        Ok(msg)
    }

    /// Split a payload in the batch format (`BATCH_MESSAGE_FORMAT`) into its messages. Every
    /// `data` section of the envelope holds one encoded message.
    pub fn decode_batch(payload: &[u8]) -> Result<Vec<Message>, DecodeError> {
        let mut budget = value_budget(payload.len());
        let envelope = Message::decode_budgeted(payload, &mut budget)?;
        match envelope.body {
            Body::Data(parts) => parts.iter().map(|p| Message::decode_budgeted(p, &mut budget)).collect(),
            Body::Empty => Ok(Vec::new()),
            _ => Err(DecodeError::Invalid("batch envelope body is not data sections")),
        }
    }

    /// Build a batch payload from already-encoded messages.
    pub fn encode_batch(messages: &[Vec<u8>]) -> Vec<u8> {
        Message { body: Body::Data(messages.to_vec()), ..Default::default() }.encode()
    }

    /// Value of a message annotation by symbol key.
    pub fn annotation(&self, key: &str) -> Option<&Value> {
        self.message_annotations.as_ref()?.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v)
    }
    pub fn set_annotation(&mut self, key: &str, value: Value) {
        let m = self.message_annotations.get_or_insert_with(Vec::new);
        match m.iter_mut().find(|(k, _)| k.as_str() == Some(key)) {
            Some(slot) => slot.1 = value,
            None => m.push((Value::sym(key), value)),
        }
    }
    pub fn app_property(&self, key: &str) -> Option<&Value> {
        self.application_properties.as_ref()?.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v)
    }
    /// Total size of the data sections.
    pub fn body_len(&self) -> usize {
        match &self.body {
            Body::Data(parts) => parts.iter().map(|p| p.len()).sum(),
            _ => 0,
        }
    }
}
