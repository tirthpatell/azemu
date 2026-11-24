//! A message and its values as JSON, for the inspector of Service Bus and Event Hubs (`docs/inspector.md`). Read-only helpers: nothing
//! here changes a message.

use azemu_core::inspect::{body_view, rfc3339};
use azemu_core::serde_json::{json, Map, Number, Value as Json};
use azemu_core::time::OffsetDateTime;

use crate::message::{Body, Message};
use crate::types::Value;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn uuid_text(b: &[u8; 16]) -> String {
    let h = hex(b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn millis(ms: i64) -> Json {
    match OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000) {
        Ok(t) => Json::String(rfc3339(t)),
        Err(_) => json!(ms),
    }
}

/// An AMQP value as JSON. Numbers stay numbers (a `ulong` or a `long` too big for a JSON number is text), times are RFC 3339, a UUID is its
/// text, binary is `{"binary": "<hex>"}`, a described value is `{"descriptor", "value"}`, and a map with text keys is an object (any other map is a list of
/// `[key, value]` pairs).
pub fn value_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => json!(b),
        Value::Ubyte(n) => json!(n),
        Value::Ushort(n) => json!(n),
        Value::Uint(n) => json!(n),
        Value::Ulong(n) if *n <= (1 << 53) => json!(n),
        Value::Ulong(n) => json!(n.to_string()),
        Value::Byte(n) => json!(n),
        Value::Short(n) => json!(n),
        Value::Int(n) => json!(n),
        Value::Long(n) if n.unsigned_abs() <= (1 << 53) => json!(n),
        Value::Long(n) => json!(n.to_string()),
        Value::Float(f) => Number::from_f64(f64::from(*f)).map_or_else(|| json!(f.to_string()), Json::Number),
        Value::Double(f) => Number::from_f64(*f).map_or_else(|| json!(f.to_string()), Json::Number),
        Value::Decimal32(b) => json!({"decimal": hex(b)}),
        Value::Decimal64(b) => json!({"decimal": hex(b)}),
        Value::Decimal128(b) => json!({"decimal": hex(b)}),
        Value::Char(c) => json!(c.to_string()),
        Value::Timestamp(ms) => millis(*ms),
        Value::Uuid(b) => json!(uuid_text(b)),
        Value::Binary(b) => json!({"binary": hex(b)}),
        Value::String(s) | Value::Symbol(s) => json!(s),
        Value::List(l) | Value::Array(l) => Json::Array(l.iter().map(value_json).collect()),
        Value::Map(m) => map_json(m),
        Value::Described(d) => json!({"descriptor": value_json(&d.descriptor), "value": value_json(&d.value)}),
    }
}

fn key_text(k: &Value) -> Option<String> {
    match k {
        Value::String(s) | Value::Symbol(s) => Some(s.clone()),
        _ => None,
    }
}

/// A map of the message (annotations, application properties).
pub fn map_json(m: &[(Value, Value)]) -> Json {
    if m.iter().all(|(k, _)| key_text(k).is_some()) {
        Json::Object(m.iter().filter_map(|(k, v)| key_text(k).map(|k| (k, value_json(v)))).collect())
    } else {
        Json::Array(m.iter().map(|(k, v)| json!([value_json(k), value_json(v)])).collect())
    }
}

/// The body as the page shows it: text when it is text, base64 otherwise (first 64 KiB), or the AMQP value when the body is not bytes.
pub fn body_json(body: &Body, content_type: Option<&str>) -> Json {
    match body {
        Body::Empty | Body::NullData => body_view(&[], content_type),
        Body::Data(parts) => {
            // the first 64 KiB are enough: do not join what will not be shown
            let mut joined: Vec<u8> = Vec::new();
            let mut total = 0usize;
            for p in parts {
                total += p.len();
                if joined.len() <= azemu_core::inspect::BODY_LIMIT {
                    joined.extend_from_slice(p);
                }
            }
            let mut v = body_view(&joined, content_type);
            if let Json::Object(m) = &mut v {
                m.insert("size".into(), json!(total));
                m.insert("truncated".into(), json!(total > azemu_core::inspect::BODY_LIMIT));
                if parts.len() > 1 {
                    m.insert("dataSections".into(), json!(parts.len()));
                }
            }
            v
        }
        Body::Value(Value::String(s)) => {
            let mut v = body_view(s.as_bytes(), content_type);
            if let Json::Object(m) = &mut v {
                m.insert("amqpValue".into(), json!("string"));
            }
            v
        }
        Body::Value(other) => json!({"amqpValue": value_json(other)}),
        Body::Sequence(s) => json!({"amqpSequence": s.iter().map(|l| Json::Array(l.iter().map(value_json).collect())).collect::<Vec<_>>()}),
    }
}

/// The sections of a message that are not the body: the standard properties, the header, the annotations and the application
/// properties, each left out when the message has none; and `body`.
pub fn message_json(m: &Message) -> Map<String, Json> {
    let mut out = Map::new();
    let mut put = |k: &str, v: Json| {
        if !v.is_null() {
            out.insert(k.to_string(), v);
        }
    };
    let mut content_type = None;
    if let Some(p) = &m.properties {
        content_type = p.content_type.clone();
        put("messageId", p.message_id.as_ref().map_or(Json::Null, value_json));
        put("to", json!(p.to));
        put("subject", json!(p.subject));
        put("replyTo", json!(p.reply_to));
        put("correlationId", p.correlation_id.as_ref().map_or(Json::Null, value_json));
        put("contentType", json!(p.content_type));
        put("contentEncoding", json!(p.content_encoding));
        put("absoluteExpiryTime", p.absolute_expiry_time.map_or(Json::Null, millis));
        put("creationTime", p.creation_time.map_or(Json::Null, millis));
        put("groupId", json!(p.group_id));
        put("groupSequence", json!(p.group_sequence));
        put("replyToGroupId", json!(p.reply_to_group_id));
        put("userId", p.user_id.as_ref().map_or(Json::Null, |u| json!({"binary": hex(u)})));
    }
    if let Some(h) = &m.header {
        put(
            "header",
            json!({"durable": h.durable, "priority": h.priority, "ttl": h.ttl, "firstAcquirer": h.first_acquirer, "deliveryCount": h.delivery_count}),
        );
    }
    put("annotations", m.message_annotations.as_ref().map_or(Json::Null, |a| map_json(a)));
    put("applicationProperties", m.application_properties.as_ref().map_or(Json::Null, |a| map_json(a)));
    put("body", body_json(&m.body, content_type.as_deref()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_become_json_the_page_can_show() {
        assert_eq!(value_json(&Value::Ulong(7)), json!(7));
        assert_eq!(value_json(&Value::Ulong(u64::MAX)), json!("18446744073709551615"));
        assert_eq!(value_json(&Value::Binary(vec![1, 255])), json!({"binary": "01ff"}));
        assert_eq!(value_json(&Value::Timestamp(0)), json!("1970-01-01T00:00:00.000Z"));
        assert_eq!(value_json(&Value::Uuid([0; 16])), json!("00000000-0000-0000-0000-000000000000"));
        let m = Value::Map(vec![(Value::sym("a"), Value::Int(1)), (Value::str("b"), Value::Bool(true))]);
        assert_eq!(value_json(&m), json!({"a": 1, "b": true}));
        let odd = Value::Map(vec![(Value::Int(1), Value::Int(2))]);
        assert_eq!(value_json(&odd), json!([[1, 2]]));
        assert_eq!(value_json(&Value::Double(f64::NAN)), json!("NaN"));
    }

    #[test]
    fn a_body_of_text_is_text_and_two_sections_are_one_body() {
        let m = Message { body: Body::Data(vec![b"he".to_vec(), b"llo".to_vec()]), ..Default::default() };
        let v = message_json(&m);
        assert_eq!(v["body"]["text"], "hello");
        assert_eq!(v["body"]["dataSections"], 2);
        let binary = Message::data(vec![0, 1, 2, 255]);
        assert!(message_json(&binary)["body"].get("base64").is_some());
    }
}
