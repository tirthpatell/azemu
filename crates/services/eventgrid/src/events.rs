//! The event as the service keeps it: a JSON object plus the schema it was published in.
//! Lookup of filter keys, and conversion between the Event Grid and CloudEvents shapes
//! used for delivery.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// The three event shapes Event Grid knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Schema {
    #[serde(rename = "EventGridSchema", alias = "EventGrid", alias = "eventgrid", alias = "eventgridschema")]
    EventGrid,
    #[serde(rename = "CloudEventSchemaV1_0", alias = "CloudEvents", alias = "cloudevents", alias = "CloudEventSchema")]
    CloudEvents,
    #[serde(rename = "CustomInputSchema", alias = "Custom", alias = "custom", alias = "CustomEventSchema")]
    Custom,
}

impl Schema {
    pub fn label(self) -> &'static str {
        match self {
            Schema::EventGrid => "EventGridSchema",
            Schema::CloudEvents => "CloudEventSchemaV1_0",
            Schema::Custom => "CustomInputSchema",
        }
    }
}

/// One published event. `body` is always a JSON object.
#[derive(Clone, Debug)]
pub struct Event {
    pub schema: Schema,
    pub body: Value,
}

/// Fixed id of the (fake) subscription the resource ids in events point at.
pub const FAKE_SUBSCRIPTION: &str = "00000000-0000-0000-0000-000000000000";

/// `/subscriptions/…/providers/Microsoft.EventGrid/topics/{name}` for a custom topic,
/// `…/domains/{d}/topics/{t}` for a domain topic.
pub fn topic_resource_id(host: &str, domain_topic: Option<&str>, is_domain: bool) -> String {
    let base = format!("/subscriptions/{FAKE_SUBSCRIPTION}/resourceGroups/azemu/providers/Microsoft.EventGrid");
    match (is_domain, domain_topic) {
        (true, Some(t)) => format!("{base}/domains/{host}/topics/{t}"),
        (true, None) => format!("{base}/domains/{host}"),
        (false, _) => format!("{base}/topics/{host}"),
    }
}

impl Event {
    pub fn new(schema: Schema, body: Value) -> Event {
        Event { schema, body }
    }

    fn str_field(&self, name: &str) -> Option<&str> {
        self.body.get(name).and_then(Value::as_str)
    }

    pub fn id(&self) -> Option<&str> {
        self.str_field("id")
    }

    pub fn subject(&self) -> Option<&str> {
        self.str_field("subject")
    }

    /// `eventType` (Event Grid schema) or `type` (CloudEvents).
    pub fn event_type(&self) -> Option<&str> {
        match self.schema {
            Schema::EventGrid => self.str_field("eventType"),
            Schema::CloudEvents => self.str_field("type"),
            Schema::Custom => self.str_field("eventType").or_else(|| self.str_field("type")),
        }
    }

    /// Value for a filter key: `id`, `subject`, `eventType`, `data.a.b`, `data.list[0]`...
    /// The first segment is matched case-insensitively; `eventType` and `type` are
    /// treated as the same key.
    pub fn lookup(&self, key: &str) -> Option<&Value> {
        let segs = split_key(key)?;
        let obj = self.body.as_object()?;
        let first = match &segs[0] {
            Seg::Name(n) => n.as_str(),
            Seg::Index(_) => return None,
        };
        let mut cur = lookup_top(obj, first)?;
        for s in &segs[1..] {
            cur = match s {
                Seg::Name(n) => {
                    let o = cur.as_object()?;
                    o.get(n.as_str()).or_else(|| o.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v))?
                }
                Seg::Index(i) => cur.as_array()?.get(*i)?,
            };
        }
        Some(cur)
    }

    /// The event as it is handed to a consumer, in the schema the subscription asked for.
    /// `topic_id` is the resource id of the topic the event came through.
    pub fn deliver_as(&self, want: Schema, topic_id: &str) -> Value {
        match (self.schema, want) {
            (Schema::EventGrid, Schema::EventGrid) => {
                let mut o = self.body.as_object().cloned().unwrap_or_default();
                // the service sets `topic` and `metadataVersion`, whatever the publisher sent
                o.insert("topic".into(), json!(topic_id));
                o.insert("metadataVersion".into(), json!("1"));
                Value::Object(order_eg(o))
            }
            (Schema::EventGrid, Schema::CloudEvents) => {
                let o = self.body.as_object().cloned().unwrap_or_default();
                let mut ce = Map::new();
                ce.insert("specversion".into(), json!("1.0"));
                ce.insert("id".into(), o.get("id").cloned().unwrap_or(Value::Null));
                ce.insert("source".into(), json!(topic_id));
                if let Some(s) = o.get("subject") {
                    ce.insert("subject".into(), s.clone());
                }
                ce.insert("type".into(), o.get("eventType").cloned().unwrap_or(Value::Null));
                if let Some(t) = o.get("eventTime") {
                    ce.insert("time".into(), t.clone());
                }
                if let Some(d) = o.get("data") {
                    ce.insert("data".into(), d.clone());
                }
                if let Some(v) = o.get("dataVersion").filter(|v| v.as_str() != Some("")) {
                    ce.insert("dataversion".into(), v.clone());
                }
                Value::Object(ce)
            }
            (Schema::CloudEvents, Schema::EventGrid) => {
                let o = self.body.as_object().cloned().unwrap_or_default();
                let mut eg = Map::new();
                eg.insert("id".into(), o.get("id").cloned().unwrap_or(Value::Null));
                eg.insert("topic".into(), json!(topic_id));
                eg.insert("subject".into(), o.get("subject").cloned().unwrap_or_else(|| json!("")));
                eg.insert("eventType".into(), o.get("type").cloned().unwrap_or(Value::Null));
                if let Some(d) = o.get("data") {
                    eg.insert("data".into(), d.clone());
                } else if let Some(d) = o.get("data_base64") {
                    eg.insert("data".into(), d.clone());
                }
                eg.insert("dataVersion".into(), o.get("dataversion").cloned().unwrap_or_else(|| json!("1")));
                eg.insert("metadataVersion".into(), json!("1"));
                if let Some(t) = o.get("time") {
                    eg.insert("eventTime".into(), t.clone());
                }
                Value::Object(eg)
            }
            // Custom events and same-schema delivery pass through unchanged.
            _ => self.body.clone(),
        }
    }
}

/// Event Grid sends its fields in a fixed order; keep ours readable the same way.
fn order_eg(mut o: Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for k in ["id", "topic", "subject", "data", "eventType", "dataVersion", "metadataVersion", "eventTime"] {
        if let Some(v) = o.remove(k) {
            out.insert(k.to_string(), v);
        }
    }
    for (k, v) in o {
        out.insert(k, v);
    }
    out
}

fn lookup_top<'a>(obj: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    if let Some(v) = obj.get(name) {
        return Some(v);
    }
    if let Some((_, v)) = obj.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
        return Some(v);
    }
    let alias = if name.eq_ignore_ascii_case("eventtype") {
        "type"
    } else if name.eq_ignore_ascii_case("type") {
        "eventType"
    } else {
        return None;
    };
    obj.iter().find(|(k, _)| k.eq_ignore_ascii_case(alias)).map(|(_, v)| v)
}

#[derive(Debug, PartialEq)]
enum Seg {
    Name(String),
    Index(usize),
}

/// `data.a.b[0].c` -> [data, a, b, 0, c]. None for an empty or malformed key.
fn split_key(key: &str) -> Option<Vec<Seg>> {
    let mut out = Vec::new();
    for part in key.split('.') {
        let (name, mut rest) = match part.find('[') {
            Some(i) => (&part[..i], &part[i..]),
            None => (part, ""),
        };
        if name.is_empty() && out.is_empty() {
            return None;
        }
        if !name.is_empty() {
            out.push(Seg::Name(name.to_string()));
        }
        while let Some(r) = rest.strip_prefix('[') {
            let end = r.find(']')?;
            out.push(Seg::Index(r[..end].trim().parse().ok()?));
            rest = &r[end + 1..];
        }
        if !rest.is_empty() {
            return None;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eg() -> Event {
        Event::new(
            Schema::EventGrid,
            json!({"id":"1","subject":"/a/b","eventType":"T.One","dataVersion":"2","eventTime":"2024-01-01T00:00:00Z",
                   "data":{"color":"red","n":3,"items":[{"x":1},{"x":2}],"nested":{"Deep":true}}}),
        )
    }

    #[test]
    fn lookup_paths() {
        let e = eg();
        assert_eq!(e.lookup("subject"), Some(&json!("/a/b")));
        assert_eq!(e.lookup("EventType"), Some(&json!("T.One")));
        assert_eq!(e.lookup("type"), Some(&json!("T.One")));
        assert_eq!(e.lookup("data.color"), Some(&json!("red")));
        assert_eq!(e.lookup("data.items[1].x"), Some(&json!(2)));
        assert_eq!(e.lookup("data.nested.deep"), Some(&json!(true)));
        assert_eq!(e.lookup("data.missing"), None);
        assert_eq!(e.lookup("data.items[5]"), None);
        assert_eq!(e.lookup(""), None);
        assert_eq!(e.lookup("data..x"), None);
    }

    #[test]
    fn eg_to_cloudevents_and_back() {
        let e = eg();
        let ce = e.deliver_as(Schema::CloudEvents, "/topics/t");
        assert_eq!(ce["specversion"], "1.0");
        assert_eq!(ce["type"], "T.One");
        assert_eq!(ce["source"], "/topics/t");
        assert_eq!(ce["time"], "2024-01-01T00:00:00Z");
        let back = Event::new(Schema::CloudEvents, ce).deliver_as(Schema::EventGrid, "/topics/t");
        assert_eq!(back["eventType"], "T.One");
        assert_eq!(back["dataVersion"], "2");
        assert_eq!(back["metadataVersion"], "1");
    }

    #[test]
    fn eg_delivery_adds_topic_and_metadata_version() {
        let v = eg().deliver_as(Schema::EventGrid, "/topics/t");
        assert_eq!(v["topic"], "/topics/t");
        assert_eq!(v["metadataVersion"], "1");
        let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys[0], "id");
        assert_eq!(keys[1], "topic");
    }
}
