//! The Event Grid schema as a custom topic reads it, in the order and with the texts a real topic uses
//! (checked against Azure in November 2026, see `docs/services/eventgrid.md`).
//!
//! A topic whose input schema is `EventGridSchema` reads the body like this:
//!
//! 1. the body must be a JSON array (the first token is looked at before anything else; text that is not
//!    JSON at all is another error);
//! 2. the events are checked one after the other, each in two steps. First every property is read in the order it
//!    was written: a property the schema does not have, a string property that holds an object or an array, and an
//!    `eventTime` that is not a date stop the request. Then the required properties are looked at in the order
//!    `subject`, `eventType`, `id`, `eventTime` (empty or blank counts as missing). `dataVersion` and `data` are optional;
//! 3. names are not case sensitive; numbers and booleans in a string property are taken as their text.

use azemu_core::time::parse_iso8601;
use azemu_core::{Request, Response};
use serde_json::{Map, Value};

use crate::errors::Fail;

const SCHEMA: &str = "EventGridEvent";

/// The property names of the schema, in the spelling the events carry.
const KNOWN: [&str; 8] = ["id", "topic", "subject", "data", "eventType", "eventTime", "metadataVersion", "dataVersion"];

/// What a JSON reader calls the token a value starts with.
fn token_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(_) => "Boolean",
        Value::Number(n) if n.is_f64() => "Float",
        Value::Number(_) => "Integer",
        Value::String(_) => "String",
        Value::Array(_) => "StartArray",
        Value::Object(_) => "StartObject",
    }
}

fn container_error(expected: &str, actual: &str) -> Fail {
    Fail::input_json_invalid(
        format!(
            "This resource is configured to receive event in '{SCHEMA}' schema. The JSON received does not conform to the expected schema. Token Expected: {expected}, Actual Token Received: {actual}."
        ),
    )
}

/// The token the text starts with, for the error of a body that is not an array. `None` when it is not JSON.
fn first_token(text: &[u8]) -> Option<&'static str> {
    let word = |w: &[u8]| text.starts_with(w);
    match text.first()? {
        b'{' => Some("StartObject"),
        b'[' => Some("StartArray"),
        b'"' => Some("String"),
        b't' if word(b"true") => Some("Boolean"),
        b'f' if word(b"false") => Some("Boolean"),
        b'n' if word(b"null") => Some("Null"),
        b'-' | b'0'..=b'9' => {
            let end = text.iter().position(|c| !matches!(c, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')).unwrap_or(text.len());
            Some(if text[..end].iter().any(|c| matches!(c, b'.' | b'e' | b'E')) { "Float" } else { "Integer" })
        }
        _ => None,
    }
}

fn unexpected_character(text: &[u8]) -> Fail {
    let c = text.first().map(|b| *b as char).unwrap_or(' ');
    Fail::bad_request(format!("Unexpected character encountered while parsing value: {c}. Path '', line 0, position 0."))
}

/// A body that is not valid JSON inside an array (text that is cut short, a stray character).
fn json_error(e: &serde_json::Error) -> Fail {
    if e.is_eof() {
        return Fail::bad_request("Unexpected end when reading JSON.");
    }
    Fail::bad_request(format!(
        "Unexpected character encountered while parsing value. Path '', line {}, position {}.",
        e.line().saturating_sub(1),
        e.column()
    ))
}

/// The events of a request body for a topic fixed to the Event Grid schema.
pub fn parse_body(req: &Request) -> Result<Vec<Value>, Response> {
    read_body(&req.body).map_err(|f| f.into_response(req))
}

fn read_body(body: &[u8]) -> Result<Vec<Value>, Fail> {
    let body = body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(body);
    let Some(start) = body.iter().position(|b| !b.is_ascii_whitespace()) else {
        return Err(Fail::bad_request("Unexpected end when reading JSON."));
    };
    let text = &body[start..];
    match first_token(text) {
        None => return Err(unexpected_character(text)),
        Some("StartArray") => {}
        Some(other) => return Err(container_error("StartArray", other)),
    }
    // Only the first value is read: what follows the closing bracket is not looked at.
    let items = match serde_json::Deserializer::from_slice(text).into_iter::<Value>().next() {
        Some(Ok(Value::Array(a))) => a,
        Some(Ok(_)) | None => return Err(unexpected_character(text)),
        Some(Err(e)) => return Err(json_error(&e)),
    };
    if items.is_empty() {
        return Err(container_error("StartObject", "EndArray"));
    }
    items.into_iter().map(|item| check_event(item, true)).collect()
}

/// Check one event and return it with canonical property names and text where the schema wants text.
/// `strict_properties` refuses properties the schema does not have (a topic fixed to the schema does; a topic that
/// works the schema out from the events, which may carry anything, does not).
pub fn check_event(item: Value, strict_properties: bool) -> Result<Value, Fail> {
    let Value::Object(map) = item else {
        return Err(container_error("StartObject", token_name(&item)));
    };
    let mut out = Map::new();
    for (name, value) in map {
        let Some(canonical) = KNOWN.iter().find(|k| k.eq_ignore_ascii_case(&name)) else {
            if strict_properties {
                return Err(Fail::input_json_invalid(format!(
                    "This resource is configured for '{SCHEMA}' schema. Received unexpected property '{name}' in JSON which does not conform to the expected schema."
                )));
            }
            out.insert(name, value);
            continue;
        };
        let value = match *canonical {
            "data" => value,
            "eventTime" => match value {
                Value::Null => Value::Null,
                Value::String(s) if parse_iso8601(&s).is_some() => Value::String(s),
                _ => return Err(Fail::input_json_invalid("The event time property 'eventTime' was not a valid date/time.")),
            },
            _ => match value {
                Value::Null | Value::String(_) => value,
                Value::Bool(b) => Value::String(if b { "True" } else { "False" }.into()),
                Value::Number(n) => Value::String(n.to_string()),
                other => {
                    return Err(Fail::bad_request(format!(
                        "Error reading string. Unexpected token: {}. Path '{canonical}'.",
                        token_name(&other)
                    )))
                }
            },
        };
        out.insert((*canonical).to_string(), value);
    }
    for required in ["subject", "eventType", "id", "eventTime"] {
        let ok = matches!(out.get(required), Some(Value::String(s)) if !s.trim().is_empty());
        if !ok {
            return Err(Fail::input_json_invalid(format!(
                "This resource is configured for '{SCHEMA}' schema and requires '{required}' property to be set."
            )));
        }
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(f: Fail) -> String {
        f.message
    }

    fn ok_event() -> Value {
        json!({"id":"1","subject":"/s","eventType":"t","eventTime":"2026-11-14T00:00:00Z","dataVersion":"1","data":{"a":1}})
    }

    #[test]
    fn a_valid_event_is_kept() {
        assert_eq!(read_body(json!([ok_event()]).to_string().as_bytes()).unwrap(), vec![ok_event()]);
    }

    #[test]
    fn the_required_properties_are_looked_at_in_the_order_azure_uses() {
        let e = |names: &[&str]| {
            let mut v = ok_event();
            for n in names {
                v.as_object_mut().unwrap().remove(*n);
            }
            msg(check_event(v, true).unwrap_err())
        };
        let want = |n: &str| format!("This resource is configured for 'EventGridEvent' schema and requires '{n}' property to be set.");
        assert_eq!(e(&["id", "subject"]), want("subject"));
        assert_eq!(e(&["id", "eventType"]), want("eventType"));
        assert_eq!(e(&["id", "eventTime"]), want("id"));
        assert_eq!(e(&["eventType", "eventTime"]), want("eventType"));
        assert_eq!(e(&["subject", "eventTime"]), want("subject"));
        assert_eq!(e(&["subject", "eventType", "id", "eventTime"]), want("subject"));
    }

    #[test]
    fn dataversion_and_data_are_optional_and_text_is_taken_from_numbers() {
        let mut v = ok_event();
        v.as_object_mut().unwrap().remove("dataVersion");
        v.as_object_mut().unwrap().remove("id");
        v["Id"] = json!(5);
        let got = check_event(v, true).unwrap();
        assert_eq!(got["id"], json!("5"));
        assert!(got.get("dataVersion").is_none());
        let mut v = ok_event();
        v["dataVersion"] = json!(true);
        assert_eq!(check_event(v, true).unwrap()["dataVersion"], json!("True"));
    }

    #[test]
    fn unknown_properties_are_refused_only_when_strict() {
        let mut v = ok_event();
        v["color"] = json!("red");
        assert_eq!(
            msg(check_event(v.clone(), true).unwrap_err()),
            "This resource is configured for 'EventGridEvent' schema. Received unexpected property 'color' in JSON which does not conform to the expected schema."
        );
        assert_eq!(check_event(v, false).unwrap()["color"], json!("red"));
    }

    #[test]
    fn wrong_types_and_dates() {
        let mut v = ok_event();
        v["subject"] = json!({"a": 1});
        assert_eq!(msg(check_event(v, true).unwrap_err()), "Error reading string. Unexpected token: StartObject. Path 'subject'.");
        let mut v = ok_event();
        v["eventTime"] = json!(5);
        assert_eq!(msg(check_event(v, true).unwrap_err()), "The event time property 'eventTime' was not a valid date/time.");
        for good in ["2026-11-14", "2026-11-14T00:00:00", "2026-11-14 00:00:00", "2026-11-14T00:00:00+02:00", "2026-11-14t00:00:00z"] {
            let mut v = ok_event();
            v["eventTime"] = json!(good);
            assert!(check_event(v, true).is_ok(), "{good}");
        }
        let mut v = ok_event();
        v["eventTime"] = json!("2026-11-14T25:00:00Z");
        assert!(check_event(v, true).is_err());
    }

    #[test]
    fn body_errors() {
        let m = |b: &str| msg(read_body(b.as_bytes()).unwrap_err());
        let top = "This resource is configured to receive event in 'EventGridEvent' schema. The JSON received does not conform to the expected schema. Token Expected:";
        assert_eq!(m(""), "Unexpected end when reading JSON.");
        assert_eq!(m("   "), "Unexpected end when reading JSON.");
        assert_eq!(m("{not json"), format!("{top} StartArray, Actual Token Received: StartObject."));
        assert_eq!(m("null"), format!("{top} StartArray, Actual Token Received: Null."));
        assert_eq!(m("5"), format!("{top} StartArray, Actual Token Received: Integer."));
        assert_eq!(m("[]"), format!("{top} StartObject, Actual Token Received: EndArray."));
        assert_eq!(m("[1]"), format!("{top} StartObject, Actual Token Received: Integer."));
        assert_eq!(m("[null]"), format!("{top} StartObject, Actual Token Received: Null."));
        assert!(m("garbage").starts_with("Unexpected character encountered while parsing value: g."));
        assert_eq!(m("["), "Unexpected end when reading JSON.");
        // what follows the array is not read
        assert!(read_body(format!("{} trailing", json!([ok_event()])).as_bytes()).is_ok());
    }

    #[test]
    fn the_second_event_is_looked_at_after_the_first() {
        let mut no_subject = ok_event();
        no_subject.as_object_mut().unwrap().remove("subject");
        let mut unknown = ok_event();
        unknown["color"] = json!(1);
        let body = json!([no_subject, unknown]).to_string();
        assert!(msg(read_body(body.as_bytes()).unwrap_err()).contains("requires 'subject'"));
        let body = json!([ok_event(), unknown]).to_string();
        assert!(msg(read_body(body.as_bytes()).unwrap_err()).contains("unexpected property 'color'"));
    }
}
