//! Reading a publish request body: content type, the three schemas, validation.
//! The error texts follow the form Event Grid uses ("This resource is configured to
//! receive event in 'EventGridEvent' schema. ...").

use azemu_core::time::parse_iso8601;
use azemu_core::{Request, Response};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::{Map, Value};

use crate::errors;
use crate::events::{Event, Schema};
use crate::model::InputSchema;

/// What the content type says about the body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// `application/cloudevents+json`: one event.
    CloudSingle,
    /// `application/cloudevents-batch+json`: an array of events.
    CloudBatch,
    /// `application/json` or anything else.
    Json,
}

pub fn shape_of(content_type: Option<&str>) -> Shape {
    let main = content_type.unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match main.as_str() {
        "application/cloudevents+json" => Shape::CloudSingle,
        "application/cloudevents-batch+json" => Shape::CloudBatch,
        _ => Shape::Json,
    }
}

fn label(s: Schema) -> &'static str {
    match s {
        Schema::EventGrid => "EventGridEvent",
        Schema::CloudEvents => "CloudEventSchemaV1_0",
        Schema::Custom => "CustomEventSchema",
    }
}

fn is_binary_mode(req: &Request) -> bool {
    req.headers.contains_key("ce-specversion")
}

/// Parse and validate a classic publish body for a topic with the given input schema.
pub fn parse_classic(req: &Request, input: InputSchema) -> Result<Vec<Event>, Response> {
    let shape = shape_of(req.header("content-type"));
    if is_binary_mode(req) {
        let ev = binary_event(req)?;
        validate_ce(&ev, "").map_err(|m| errors::invalid_input(label(Schema::CloudEvents), &m))?;
        return Ok(vec![Event::new(Schema::CloudEvents, ev)]);
    }
    let value: Value = serde_json::from_slice(&req.body)
        .map_err(|e| errors::invalid_input(label(input.schema().unwrap_or(Schema::EventGrid)), &format!("Invalid JSON: {e}.")))?;
    let items: Vec<Value> = match &value {
        Value::Array(a) => a.clone(),
        Value::Object(_) => vec![value.clone()],
        _ => return Err(errors::invalid_input(label(input.schema().unwrap_or(Schema::EventGrid)), "Token Expected: StartArray.")),
    };
    let schema = match input.schema() {
        Some(s) => s,
        None => infer(shape, &items),
    };
    let wrong_container =
        |expected: &str, actual: &str| errors::invalid_input(label(schema), &format!("Token Expected: {expected}, Actual: {actual}."));
    match (schema, shape, &value) {
        (Schema::CloudEvents, Shape::CloudSingle, Value::Array(_)) => return Err(wrong_container("StartObject", "StartArray")),
        (Schema::CloudEvents, Shape::CloudBatch, Value::Object(_)) => return Err(wrong_container("StartArray", "StartObject")),
        (Schema::EventGrid, _, Value::Object(_)) => return Err(wrong_container("StartArray", "StartObject")),
        _ => {}
    }
    if items.is_empty() {
        return Err(errors::invalid_input(label(schema), "The request contains no events."));
    }
    let single = matches!(value, Value::Object(_));
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        let path = if single { String::new() } else { format!("[{i}]") };
        let checked = match schema {
            Schema::EventGrid => validate_eg(&item, &path),
            Schema::CloudEvents => validate_ce(&item, &path),
            Schema::Custom => validate_custom(&item, &path),
        };
        checked.map_err(|m| errors::invalid_input(label(schema), &m))?;
        out.push(Event::new(schema, item));
    }
    Ok(out)
}

/// Parse and validate the body of a namespace topic publish (CloudEvents only).
pub fn parse_namespace(req: &Request) -> Result<Vec<Event>, Response> {
    let bad = |m: String| errors::error(400, "BadRequest", &m, None);
    if is_binary_mode(req) {
        let ev = binary_event(req)?;
        validate_ce(&ev, "").map_err(bad)?;
        return Ok(vec![Event::new(Schema::CloudEvents, ev)]);
    }
    let shape = shape_of(req.header("content-type"));
    if shape == Shape::Json {
        let ct = req.header("content-type").unwrap_or("(none)");
        return Err(errors::error(
            415,
            "UnsupportedMediaType",
            &format!(
                "The content type '{ct}' is not supported. Use application/cloudevents+json, application/cloudevents-batch+json or the binary content mode (ce-* headers)."
            ),
            None,
        ));
    }
    let value: Value = serde_json::from_slice(&req.body).map_err(|e| bad(format!("The request body is not valid JSON: {e}.")))?;
    let items: Vec<Value> = match (shape, value) {
        (Shape::CloudSingle, v @ Value::Object(_)) => vec![v],
        (Shape::CloudBatch, Value::Array(a)) => a,
        (Shape::CloudSingle, _) => return Err(bad("The body must be a single CloudEvent (a JSON object).".into())),
        _ => return Err(bad("The body must be an array of CloudEvents.".into())),
    };
    if items.is_empty() {
        return Err(bad("The request contains no events.".into()));
    }
    let batch = shape == Shape::CloudBatch;
    let mut out = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        let path = if batch { format!("[{i}]") } else { String::new() };
        validate_ce(&item, &path).map_err(bad)?;
        out.push(Event::new(Schema::CloudEvents, item));
    }
    Ok(out)
}

/// Work out the schema of an `Auto` topic from the content type and the first event.
fn infer(shape: Shape, items: &[Value]) -> Schema {
    if shape != Shape::Json {
        return Schema::CloudEvents;
    }
    let first = items.first().and_then(Value::as_object);
    match first {
        Some(o) if o.contains_key("specversion") => Schema::CloudEvents,
        Some(o) if ["eventType", "dataVersion", "eventTime", "metadataVersion"].iter().any(|k| o.contains_key(*k)) => Schema::EventGrid,
        _ => Schema::Custom,
    }
}

fn require_str(o: &Map<String, Value>, name: &str, path: &str) -> Result<(), String> {
    match o.get(name) {
        None | Some(Value::Null) => Err(format!("Required property '{name}' not found in JSON. Path '{path}'.")),
        Some(Value::String(s)) if !s.is_empty() => Ok(()),
        Some(Value::String(_)) => Err(format!("Required property '{name}' must not be empty. Path '{path}'.")),
        Some(_) => Err(format!("Unexpected token, expected a string for '{name}'. Path '{path}.{name}'.")),
    }
}

fn object<'a>(v: &'a Value, path: &str) -> Result<&'a Map<String, Value>, String> {
    v.as_object().ok_or_else(|| format!("Token Expected: StartObject, Actual: {}. Path '{path}'.", token_name(v)))
}

fn token_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(_) => "Boolean",
        Value::Number(_) => "Number",
        Value::String(_) => "String",
        Value::Array(_) => "StartArray",
        Value::Object(_) => "StartObject",
    }
}

fn dotted(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}.{name}")
    }
}

pub fn validate_eg(v: &Value, path: &str) -> Result<(), String> {
    let o = object(v, path)?;
    for name in ["id", "subject", "eventType", "eventTime", "dataVersion"] {
        require_str(o, name, path)?;
    }
    if let Some(t) = o.get("eventTime").and_then(Value::as_str) {
        if parse_iso8601(t).is_none() {
            return Err(format!("Could not convert string to DateTime: {t}. Path '{}'.", dotted(path, "eventTime")));
        }
    }
    Ok(())
}

pub fn validate_ce(v: &Value, path: &str) -> Result<(), String> {
    let o = object(v, path)?;
    for name in ["specversion", "id", "source", "type"] {
        require_str(o, name, path)?;
    }
    if o.get("specversion").and_then(Value::as_str) != Some("1.0") {
        return Err(format!("The CloudEvents specversion must be '1.0'. Path '{}'.", dotted(path, "specversion")));
    }
    if o.contains_key("data") && o.contains_key("data_base64") {
        return Err(format!("A CloudEvent cannot have both 'data' and 'data_base64'. Path '{path}'."));
    }
    if let Some(b) = o.get("data_base64") {
        let ok = b.as_str().map(|s| STANDARD.decode(s).is_ok()).unwrap_or(false);
        if !ok {
            return Err(format!("'data_base64' is not a valid Base64 string. Path '{}'.", dotted(path, "data_base64")));
        }
    }
    if let Some(t) = o.get("time") {
        match t.as_str() {
            Some(s) if parse_iso8601(s).is_some() => {}
            Some(s) => return Err(format!("Could not convert string to DateTime: {s}. Path '{}'.", dotted(path, "time"))),
            None if t.is_null() => {}
            None => return Err(format!("Unexpected token, expected a string for 'time'. Path '{}'.", dotted(path, "time"))),
        }
    }
    Ok(())
}

pub fn validate_custom(v: &Value, path: &str) -> Result<(), String> {
    object(v, path).map(|_| ())
}

/// CloudEvents binary content mode: attributes in `ce-*` headers, the body is the data.
fn binary_event(req: &Request) -> Result<Value, Response> {
    let mut o = Map::new();
    for (name, value) in req.headers.iter() {
        let n = name.as_str();
        if let Some(attr) = n.strip_prefix("ce-") {
            if let Ok(v) = value.to_str() {
                o.insert(attr.to_string(), Value::String(v.to_string()));
            }
        }
    }
    let ct = req.header("content-type").map(str::trim).filter(|c| !c.is_empty());
    if !req.body.is_empty() {
        let main = ct.unwrap_or("application/json").split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        if let Some(c) = ct {
            o.insert("datacontenttype".into(), Value::String(c.to_string()));
        }
        if main == "application/json" || main.ends_with("+json") {
            let data: Value = serde_json::from_slice(&req.body).map_err(|e| {
                errors::error(400, "BadRequest", &format!("The body is not valid JSON for content type '{main}': {e}."), None)
            })?;
            o.insert("data".into(), data);
        } else if main.starts_with("text/") || main == "application/xml" {
            match std::str::from_utf8(&req.body) {
                Ok(s) => {
                    o.insert("data".into(), Value::String(s.to_string()));
                }
                Err(_) => {
                    o.insert("data_base64".into(), Value::String(STANDARD.encode(&req.body)));
                }
            }
        } else {
            o.insert("data_base64".into(), Value::String(STANDARD.encode(&req.body)));
        }
    }
    Ok(Value::Object(o))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shapes() {
        assert_eq!(shape_of(Some("application/cloudevents+json; charset=utf-8")), Shape::CloudSingle);
        assert_eq!(shape_of(Some("Application/CloudEvents-Batch+JSON; charset=utf-8")), Shape::CloudBatch);
        assert_eq!(shape_of(Some("application/json")), Shape::Json);
        assert_eq!(shape_of(None), Shape::Json);
    }

    #[test]
    fn infer_schema() {
        let eg = vec![json!({"id":"1","eventType":"a"})];
        let ce = vec![json!({"specversion":"1.0"})];
        let custom = vec![json!({"customId":"1"})];
        assert_eq!(infer(Shape::Json, &eg), Schema::EventGrid);
        assert_eq!(infer(Shape::Json, &ce), Schema::CloudEvents);
        assert_eq!(infer(Shape::Json, &custom), Schema::Custom);
        assert_eq!(infer(Shape::CloudBatch, &custom), Schema::CloudEvents);
    }

    #[test]
    fn eg_validation() {
        let ok = json!({"id":"1","subject":"s","eventType":"t","eventTime":"2024-05-23T22:06:35.487928Z","dataVersion":"2.0","data":{}});
        assert!(validate_eg(&ok, "[0]").is_ok());
        let mut no_subject = ok.clone();
        no_subject.as_object_mut().unwrap().remove("subject");
        let e = validate_eg(&no_subject, "[0]").unwrap_err();
        assert!(e.contains("'subject'"), "{e}");
        let mut bad_time = ok.clone();
        bad_time["eventTime"] = json!("yesterday");
        assert!(validate_eg(&bad_time, "[0]").is_err());
        assert!(validate_eg(&json!(5), "[0]").is_err());
        // data is optional
        let mut no_data = ok;
        no_data.as_object_mut().unwrap().remove("data");
        assert!(validate_eg(&no_data, "").is_ok());
    }

    #[test]
    fn ce_validation() {
        let ok = json!({"specversion":"1.0","id":"1","source":"/s","type":"t","time":"2024-05-23T22:06:22.029989+00:00","data":"x"});
        assert!(validate_ce(&ok, "").is_ok());
        assert!(validate_ce(&json!({"specversion":"0.3","id":"1","source":"/s","type":"t"}), "").is_err());
        assert!(validate_ce(&json!({"specversion":"1.0","id":"1","type":"t"}), "").is_err());
        assert!(validate_ce(&json!({"specversion":"1.0","id":"1","source":"s","type":"t","data":1,"data_base64":"aGk="}), "").is_err());
        assert!(validate_ce(&json!({"specversion":"1.0","id":"1","source":"s","type":"t","data_base64":"!!"}), "").is_err());
        assert!(validate_ce(&json!({"specversion":"1.0","id":"1","source":"s","type":"t","data_base64":"aGk="}), "").is_ok());
        assert!(validate_ce(&json!({"specversion":"1.0","id":"1","source":"s","type":"t","time":"nope"}), "").is_err());
    }
}
