//! Delivery properties: `deliveryAttributeMappings` on a destination ("Set custom headers on delivered events" on
//! Learn). A mapping has a name and either a fixed value (`Static`, optionally secret) or the path of a field of the
//! event (`Dynamic`, `data.key1`). They become HTTP headers on a webhook or an Azure Function, and application
//! properties on a Service Bus message or an event hub event; a few names have a meaning of their own there.
//!
//! What a real topic did with them was tried in November 2026 (Service Bus queues and a session queue):
//!
//! * a dynamic value that is missing, `null`, an object or an array leaves the property out; the delivery still happens
//!   (for `MessageId` the message gets an id of Service Bus's making);
//! * a string is taken as it is, a number as its decimal text (`1.0` stays `1.0`, `1E+21` is written out), a boolean as
//!   `True` or `False`;
//! * a value longer than 4096 bytes leaves the property out;
//! * the mapped `MessageId`, `SessionId` and `PartitionKey` are the message's properties, other names are application
//!   properties.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::events::Event;

/// Learn: "You can set up to 10 headers when creating an event subscription. Each header value shouldn't be greater
/// than 4,096 (4K) bytes."
pub const MAX_MAPPINGS: usize = 10;
pub const MAX_VALUE_BYTES: usize = 4096;

/// What a Service Bus message has a place for (Learn, "Service Bus example"), in the spelling Learn uses.
pub const SERVICE_BUS_NAMES: [&str; 9] =
    ["MessageId", "PartitionKey", "SessionId", "CorrelationId", "Label", "ReplyTo", "ReplyToSessionId", "To", "ViaPartitionKey"];
/// What an event hub event has a place for.
pub const EVENT_HUB_NAMES: [&str; 1] = ["PartitionKey"];

/// One mapping, as stored (flat). The ARM shape (`properties: {value, isSecret}` or `{sourceField}`) is accepted on input.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "MappingIn")]
pub struct DeliveryAttribute {
    pub name: String,
    /// `Static` or `Dynamic`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Static: the value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Static: the value is not shown when the subscription is read.
    pub is_secret: bool,
    /// Dynamic: the field of the event, `data.key1`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_field: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct MappingIn {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    is_secret: bool,
    #[serde(default)]
    source_field: Option<String>,
    #[serde(default)]
    properties: Map<String, Value>,
}

impl From<MappingIn> for DeliveryAttribute {
    fn from(m: MappingIn) -> Self {
        let get = |k: &str| m.properties.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v);
        DeliveryAttribute {
            name: m.name,
            kind: m.kind,
            value: m.value.or_else(|| get("value").and_then(Value::as_str).map(str::to_string)),
            is_secret: m.is_secret || get("isSecret").and_then(Value::as_bool).unwrap_or(false),
            source_field: m.source_field.or_else(|| get("sourceField").and_then(Value::as_str).map(str::to_string)),
        }
    }
}

impl DeliveryAttribute {
    fn is_static(&self) -> bool {
        self.kind.eq_ignore_ascii_case("static")
    }

    /// The mapping as Azure shows it when a subscription is read: a secret static value is left out.
    pub fn to_arm_json(&self, show_secret: bool) -> Value {
        let mut props = Map::new();
        if self.is_static() {
            if show_secret || !self.is_secret {
                if let Some(v) = &self.value {
                    props.insert("value".into(), Value::String(v.clone()));
                }
            }
            props.insert("isSecret".into(), Value::Bool(self.is_secret));
        } else if let Some(f) = &self.source_field {
            props.insert("sourceField".into(), Value::String(f.clone()));
        }
        let mut o = Map::new();
        o.insert("name".into(), Value::String(self.name.clone()));
        o.insert("type".into(), Value::String(if self.is_static() { "Static" } else { "Dynamic" }.into()));
        o.insert("properties".into(), Value::Object(props));
        Value::Object(o)
    }
}

/// The names that belong to the connection and the framing of the request, not to the event: a mapping with one of these would
/// send a second `Host` line or replace `Content-Length`. Learn names only the `aeg-` prefix as reserved (see [`validate`]); these
/// are the headers of RFC 9110 and RFC 9112 that an HTTP client sets itself (`Proxy-` for the ones a proxy reads).
pub fn is_transport_header(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    n.starts_with("proxy-")
        || matches!(
            n.as_str(),
            "host" | "content-length" | "transfer-encoding" | "connection" | "upgrade" | "te" | "trailer" | "expect" | "keep-alive"
        )
}

/// Check the mappings of a subscription. `Err` is the text of a 400. The limits are those Learn documents (ten
/// mappings, 4096 bytes); the texts are not Azure's, and `aeg-` (the prefix of the properties Event Grid sets itself,
/// which Learn says not to use) is refused here. So are the names of [`is_transport_header`]: Learn gives no list of them and no
/// error text (not tried against Azure), so they are refused in the same form as `aeg-`.
pub fn validate(list: &[DeliveryAttribute]) -> Result<(), String> {
    if list.len() > MAX_MAPPINGS {
        return Err(format!("A subscription can have at most {MAX_MAPPINGS} delivery attribute mappings, not {}.", list.len()));
    }
    for m in list {
        let name = m.name.trim();
        if name.is_empty() {
            return Err("A delivery attribute mapping needs a name.".into());
        }
        if name.len() >= 4 && name[..4].eq_ignore_ascii_case("aeg-") {
            return Err(format!(
                "The delivery attribute name '{name}' is not allowed: names that start with 'aeg-' are used by Event Grid."
            ));
        }
        if is_transport_header(name) {
            return Err(format!(
                "The delivery attribute name '{name}' is not allowed: it is a header of the connection (Host, Content-Length, Transfer-Encoding, Connection, Upgrade, TE, Trailer, Expect, Keep-Alive and Proxy-*) that Event Grid sets itself."
            ));
        }
        if m.is_static() {
            let Some(v) = &m.value else {
                return Err(format!("The static delivery attribute '{name}' needs a value."));
            };
            if v.len() > MAX_VALUE_BYTES {
                return Err(format!(
                    "The value of the delivery attribute '{name}' is {} bytes; at most {MAX_VALUE_BYTES} are allowed.",
                    v.len()
                ));
            }
        } else if m.kind.eq_ignore_ascii_case("dynamic") {
            if m.source_field.as_deref().map(str::trim).unwrap_or("").is_empty() {
                return Err(format!("The dynamic delivery attribute '{name}' needs a sourceField."));
            }
        } else {
            return Err(format!("The delivery attribute '{name}' has the type '{}'; use Static or Dynamic.", m.kind));
        }
    }
    Ok(())
}

/// The text of a JSON value as Azure writes it into a property: strings as they are, numbers in decimal, booleans
/// as `True` / `False`; `null`, objects and arrays have none.
fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(if *b { "True" } else { "False" }.into()),
        Value::Number(n) => {
            let t = n.to_string();
            if t.contains(['e', 'E']) {
                // `1e21` is written out in full, as a decimal would be
                n.as_f64().map(|f| format!("{f}")).or(Some(t))
            } else {
                Some(t)
            }
        }
        _ => None,
    }
}

/// The name/value pairs one event gets: fixed values, and the dynamic ones whose field is there and is a string, a
/// number or a boolean of at most 4096 bytes.
pub fn resolve(list: &[DeliveryAttribute], ev: &Event) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for m in list {
        let value = if m.is_static() {
            m.value.clone()
        } else {
            let field = m.source_field.as_deref().unwrap_or("").trim();
            let field = field.strip_prefix("$.").or_else(|| field.strip_prefix('$')).unwrap_or(field);
            ev.lookup(field).and_then(scalar_text).filter(|t| t.len() <= MAX_VALUE_BYTES)
        };
        if let Some(v) = value {
            out.push((m.name.trim().to_string(), v));
        }
    }
    out
}

/// The canonical spelling of `name` when it is one of `known` (matched without regard to case).
pub fn canonical(name: &str, known: &[&'static str]) -> Option<&'static str> {
    known.iter().copied().find(|k| k.eq_ignore_ascii_case(name))
}

/// A name and its value.
pub type Pair = (String, String);

/// Split resolved pairs into the properties a destination gives a meaning to (canonical names) and the rest.
pub fn split(pairs: Vec<Pair>, known: &[&'static str]) -> (Vec<Pair>, Vec<Pair>) {
    let (mut system, mut rest) = (Vec::new(), Vec::new());
    for (n, v) in pairs {
        match canonical(&n, known) {
            Some(c) => system.push((c.to_string(), v)),
            None => rest.push((n, v)),
        }
    }
    (system, rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Schema;
    use serde_json::json;

    fn event(data: Value) -> Event {
        Event::new(
            Schema::EventGrid,
            json!({"id":"e1","subject":"s","eventType":"T","eventTime":"2026-01-01T00:00:00Z","dataVersion":"1","data":data}),
        )
    }

    fn map(v: Value) -> Vec<DeliveryAttribute> {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn arm_and_flat_shapes_read_the_same() {
        let arm = map(json!([
            {"name":"a","type":"Static","properties":{"value":"abc","isSecret":true}},
            {"name":"b","type":"Dynamic","properties":{"sourceField":"data.key1"}}]));
        let flat = map(json!([
            {"name":"a","type":"Static","value":"abc","isSecret":true},
            {"name":"b","type":"Dynamic","sourceField":"data.key1"}]));
        assert_eq!(arm, flat);
        assert!(arm[0].is_secret && arm[0].value.as_deref() == Some("abc"));
    }

    #[test]
    fn dynamic_values_follow_what_azure_did() {
        let list = map(json!([
            {"name":"s","type":"Static","value":"abc"},
            {"name":"d","type":"Dynamic","sourceField":"data.key1"}]));
        let get = |data: Value| resolve(&list, &event(data));
        assert_eq!(get(json!({"key1":"v"})), vec![("s".into(), "abc".into()), ("d".into(), "v".into())]);
        assert_eq!(get(json!({"key1":42}))[1].1, "42");
        assert_eq!(get(json!({"key1":1.0}))[1].1, "1.0");
        assert_eq!(get(json!({"key1":1e21}))[1].1, "1000000000000000000000");
        assert_eq!(get(json!({"key1":true}))[1].1, "True");
        assert_eq!(get(json!({"key1":false}))[1].1, "False");
        assert_eq!(get(json!({"key1":""}))[1].1, "");
        for gone in [json!({}), json!({"key1":null}), json!({"key1":{"a":1}}), json!({"key1":[1]}), json!({"key1":"x".repeat(4097)})] {
            assert_eq!(get(gone).len(), 1, "only the static value");
        }
        assert_eq!(get(json!({"key1":"x".repeat(4096)}))[1].1.len(), 4096);
    }

    #[test]
    fn json_path_forms_and_top_level_fields() {
        let list = map(json!([
            {"name":"a","type":"Dynamic","sourceField":"$.data.k"},
            {"name":"b","type":"Dynamic","sourceField":"subject"},
            {"name":"c","type":"Dynamic","sourceField":"data.list[1]"}]));
        let got = resolve(&list, &event(json!({"k":"x","list":[1,2]})));
        assert_eq!(got, vec![("a".into(), "x".into()), ("b".into(), "s".into()), ("c".into(), "2".into())]);
    }

    #[test]
    fn limits_and_names() {
        let ten: Vec<_> = (0..10).map(|i| json!({"name":format!("h{i}"),"type":"Static","value":"v"})).collect();
        assert!(validate(&map(Value::Array(ten.clone()))).is_ok());
        let mut eleven = ten;
        eleven.push(json!({"name":"h10","type":"Static","value":"v"}));
        assert!(validate(&map(Value::Array(eleven))).unwrap_err().contains("at most 10"));
        assert!(validate(&map(json!([{"name":"aeg-x","type":"Static","value":"v"}]))).unwrap_err().contains("aeg-"));
        assert!(validate(&map(json!([{"name":"AEG-x","type":"Static","value":"v"}]))).is_err());
        for name in [
            "Host",
            "host",
            "Content-Length",
            "Transfer-Encoding",
            "Connection",
            "Upgrade",
            "TE",
            "Trailer",
            "Expect",
            "Keep-Alive",
            "Proxy-Authorization",
            "proxy-x",
            " Host ",
        ] {
            let e = validate(&map(json!([{"name":name,"type":"Static","value":"v"}]))).unwrap_err();
            assert!(e.contains("header of the connection"), "{name}: {e}");
            assert!(validate(&map(json!([{"name":name,"type":"Dynamic","sourceField":"data.k"}]))).is_err(), "{name}");
        }
        // the headers a destination takes by name, and the documented example, are fine
        for name in ["Authorization", "Content-Type", "X-Host", "Hosting", "Contentlength", "Accept", "x-proxy"] {
            assert!(validate(&map(json!([{"name":name,"type":"Static","value":"v"}]))).is_ok(), "{name}");
        }
        assert!(validate(&map(json!([{"name":"a","type":"Static","value":"v".repeat(4097)}]))).unwrap_err().contains("4096"));
        assert!(validate(&map(json!([{"name":"a","type":"Static","value":"v".repeat(4096)}]))).is_ok());
        assert!(validate(&map(json!([{"name":"a","type":"Dynamic"}]))).unwrap_err().contains("sourceField"));
        assert!(validate(&map(json!([{"name":"a","type":"Other","value":"x"}]))).unwrap_err().contains("Static or Dynamic"));
        assert!(validate(&map(json!([{"name":"","type":"Static","value":"x"}]))).is_err());
    }

    #[test]
    fn secret_values_are_not_shown() {
        let m = map(json!([{"name":"a","type":"Static","value":"abc","isSecret":true},{"name":"b","type":"Static","value":"xyz"}]));
        assert!(m[0].to_arm_json(false)["properties"].get("value").is_none());
        assert_eq!(m[0].to_arm_json(true)["properties"]["value"], "abc");
        assert_eq!(m[1].to_arm_json(false)["properties"]["value"], "xyz");
    }

    #[test]
    fn split_known_names_without_regard_to_case() {
        let (sys, rest) = split(vec![("sessionid".into(), "s".into()), ("x".into(), "y".into())], &SERVICE_BUS_NAMES);
        assert_eq!(sys, vec![("SessionId".to_string(), "s".to_string())]);
        assert_eq!(rest, vec![("x".to_string(), "y".to_string())]);
    }
}
