//! Kafka records and Event Hubs events, both ways (Microsoft Learn: "Exchange events between consumers and
//! producers that use different protocols").
//!
//! - The record value is the event body (one `data` section); a null value is an event with no body section.
//! - The record key is the partition key (`x-opt-partition-key`): a string when the key is UTF-8, else binary.
//! - Record headers are application properties. A Kafka header value is bytes, so it is stored as an AMQP binary
//!   (a null header value as AMQP null) and an AMQP consumer sees raw bytes.
//! - The other way: a binary property comes out as the raw bytes (so what a Kafka producer wrote is what a Kafka
//!   consumer reads); every other property comes out AMQP-encoded, type byte included, as the article says ("a Kafka
//!   consumer sees the raw AMQP-encoded byte sequence"); and the properties `message-id`, `user-id`, `to`,
//!   `reply-to`, `content-type`, `content-encoding`, `creation-time` of the AMQP properties section come out as UTF-8
//!   strings, headers of the same names.
//! - Record timestamps are the time the event was enqueued; offsets are sequence numbers.

use azemu_amqp::message::{Body, Message};
use azemu_amqp::types::Value;
use azemu_kafka::records::{Header, Record};
use bytes::Bytes;

use crate::model::{prepare, Event, PreparedEvent};

pub fn record_to_message(r: &Record) -> Message {
    let mut m = Message {
        body: match &r.value {
            Some(v) => Body::Data(vec![v.to_vec()]),
            None => Body::Empty,
        },
        ..Message::default()
    };
    if let Some(k) = &r.key {
        let v = match std::str::from_utf8(k) {
            Ok(s) => Value::String(s.to_string()),
            Err(_) => Value::Binary(k.to_vec()),
        };
        m.message_annotations = Some(vec![(Value::sym("x-opt-partition-key"), v)]);
    }
    if !r.headers.is_empty() {
        m.application_properties = Some(
            r.headers
                .iter()
                .map(|h| (Value::String(h.key.clone()), h.value.as_ref().map(|v| Value::Binary(v.to_vec())).unwrap_or(Value::Null)))
                .collect(),
        );
    }
    m
}

pub fn record_to_event(r: &Record) -> PreparedEvent {
    prepare(record_to_message(r))
}

fn text_of(v: &Value) -> Option<String> {
    Some(match v {
        Value::String(s) | Value::Symbol(s) => s.clone(),
        Value::Ulong(n) => n.to_string(),
        Value::Long(n) => n.to_string(),
        Value::Uuid(u) => {
            let h: String = u.iter().map(|b| format!("{b:02x}")).collect();
            format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
        }
        Value::Binary(b) => String::from_utf8_lossy(b).into_owned(),
        _ => return None,
    })
}

fn timestamp_text(ms: i64) -> String {
    let t = azemu_core::time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .unwrap_or(azemu_core::time::OffsetDateTime::UNIX_EPOCH);
    azemu_core::time::iso8601_7(t)
}

fn header_value(v: &Value) -> Option<Bytes> {
    match v {
        Value::Null => None,
        Value::Binary(b) => Some(Bytes::copy_from_slice(b)),
        other => Some(Bytes::from(azemu_amqp::codec::encode_to_vec(other))),
    }
}

/// The partition key of a stored message as Kafka key bytes.
pub fn key_of(m: &Message) -> Option<Bytes> {
    match m.annotation("x-opt-partition-key")? {
        Value::Binary(b) => Some(Bytes::copy_from_slice(b)),
        other => other.as_str().filter(|s| !s.is_empty()).map(|s| Bytes::copy_from_slice(s.as_bytes())),
    }
}

pub fn value_of(m: &Message) -> Option<Bytes> {
    match &m.body {
        Body::Empty | Body::NullData => None,
        Body::Data(parts) => {
            if parts.len() == 1 {
                Some(Bytes::copy_from_slice(&parts[0]))
            } else {
                Some(Bytes::from(parts.concat()))
            }
        }
        Body::Value(v) => Some(match v {
            Value::Null => return None,
            Value::Binary(b) => Bytes::copy_from_slice(b),
            Value::String(s) | Value::Symbol(s) => Bytes::copy_from_slice(s.as_bytes()),
            other => Bytes::from(azemu_amqp::codec::encode_to_vec(other)),
        }),
        Body::Sequence(seqs) => {
            let mut out = Vec::new();
            for s in seqs {
                out.extend_from_slice(&azemu_amqp::codec::encode_to_vec(&Value::List(s.clone())));
            }
            Some(Bytes::from(out))
        }
    }
}

pub fn headers_of(m: &Message) -> Vec<Header> {
    let mut out = Vec::new();
    if let Some(p) = &m.properties {
        let mut put = |k: &str, v: Option<String>| {
            if let Some(v) = v {
                out.push(Header { key: k.to_string(), value: Some(Bytes::from(v.into_bytes())) });
            }
        };
        put("message-id", p.message_id.as_ref().and_then(text_of));
        put("user-id", p.user_id.as_ref().map(|u| String::from_utf8_lossy(u).into_owned()));
        put("to", p.to.clone());
        put("reply-to", p.reply_to.clone());
        put("content-type", p.content_type.clone());
        put("content-encoding", p.content_encoding.clone());
        put("creation-time", p.creation_time.map(timestamp_text));
    }
    if let Some(props) = &m.application_properties {
        for (k, v) in props {
            if let Some(k) = k.as_str() {
                out.push(Header { key: k.to_string(), value: header_value(v) });
            }
        }
    }
    out
}

/// The record of a stored event. `base_seq` and `first_ms` are those of the batch it goes into.
pub fn event_to_record(ev: &Event, base_seq: i64, first_ms: i64) -> Record {
    Record {
        offset_delta: (ev.seq - base_seq) as i32,
        timestamp_delta: ev.enqueued_ms - first_ms,
        key: key_of(&ev.message),
        value: value_of(&ev.message),
        headers: headers_of(&ev.message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use azemu_amqp::message::Properties;

    #[test]
    fn kafka_record_becomes_an_event_and_back() {
        let rec = Record {
            key: Some(Bytes::from_static(b"user-1")),
            value: Some(Bytes::from_static(b"{\"a\":1}")),
            headers: vec![
                Header { key: "h".into(), value: Some(Bytes::from_static(&[0, 0, 4, 210])) },
                Header { key: "n".into(), value: None },
            ],
            ..Default::default()
        };
        let m = record_to_message(&rec);
        assert_eq!(m.annotation("x-opt-partition-key"), Some(&Value::String("user-1".into())));
        assert_eq!(m.app_property("h"), Some(&Value::Binary(vec![0, 0, 4, 210])));
        assert_eq!(m.app_property("n"), Some(&Value::Null));
        assert_eq!(key_of(&m).as_deref(), Some(&b"user-1"[..]));
        assert_eq!(value_of(&m).as_deref(), Some(&b"{\"a\":1}"[..]));
        let h = headers_of(&m);
        assert_eq!(h[0].value.as_deref(), Some(&[0u8, 0, 4, 210][..]));
        assert_eq!(h[1].value, None);
    }

    #[test]
    fn amqp_properties_come_out_amqp_encoded_except_the_listed_ones() {
        let m = Message {
            properties: Some(Properties {
                content_type: Some("text/plain".into()),
                message_id: Some(Value::String("m1".into())),
                ..Default::default()
            }),
            application_properties: Some(vec![
                (Value::String("MyStringProperty".into()), Value::String("hello".into())),
                (Value::String("MyIntegerProperty".into()), Value::Int(1234)),
            ]),
            ..Message::data(b"x".to_vec())
        };
        let h = headers_of(&m);
        let get = |k: &str| h.iter().find(|x| x.key == k).and_then(|x| x.value.clone()).unwrap();
        assert_eq!(&get("content-type")[..], b"text/plain");
        assert_eq!(&get("message-id")[..], b"m1");
        assert_eq!(&get("MyStringProperty")[..], &[0xA1, 5, b'h', b'e', b'l', b'l', b'o']);
        assert_eq!(&get("MyIntegerProperty")[..], &[0x71, 0, 0, 4, 210]);
    }

    #[test]
    fn bodies_of_other_shapes() {
        let m = Message { body: Body::Value(Value::String("s".into())), ..Default::default() };
        assert_eq!(value_of(&m).as_deref(), Some(&b"s"[..]));
        let m = Message { body: Body::Data(vec![b"a".to_vec(), b"b".to_vec()]), ..Default::default() };
        assert_eq!(value_of(&m).as_deref(), Some(&b"ab"[..]));
        assert_eq!(value_of(&Message::default()), None);
    }
}
