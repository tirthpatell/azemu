//! Application Insights ingestion (the "Breeze" endpoint): `POST /v2/track` and `POST /v2.1/track`.

use azemu_core::resp::Resp;
use azemu_core::{Request, Response};
use serde_json::value::RawValue;
use serde_json::{json, Map, Value};
use time::OffsetDateTime;

use crate::body::{self, BodyError};
use crate::store::Telemetry;
use crate::MonitorService;

/// Items older than this are refused (the limit of Azure's own answer, `now - 172800000ms`).
const MAX_AGE_MS: i64 = 48 * 3600 * 1000;
/// Items dated further ahead than this are refused (`now + 7200000ms`).
const MAX_AHEAD_MS: i64 = 2 * 3600 * 1000;
/// "Telemetry item length must not exceed 1258291".
const MAX_ITEM_BYTES: usize = 1_258_291;
/// The most a decoded request body may hold.
pub(crate) const MAX_DECODED_BYTES: usize = 64 << 20;

/// The base types Azure knows (`data.baseType`); any other is `Unknown domain type "..."`.
const BASE_TYPES: &[&str] =
    &["RequestData", "RemoteDependencyData", "ExceptionData", "MessageData", "EventData", "MetricData", "PageViewData", "AvailabilityData"];

/// What went wrong with one envelope: Azure's message, and the status it gives that item.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ItemError {
    pub status: u16,
    pub message: String,
}

fn too_long() -> String {
    format!("Telemetry item length must not exceed {MAX_ITEM_BYTES}")
}

fn bad(message: impl Into<String>) -> ItemError {
    ItemError { status: 400, message: message.into() }
}

/// The most items one request may carry (the documented maximum of a batch). A request with more is refused whole, without
/// holding them: the length of a request is no bound on how many items it has.
pub(crate) const MAX_ITEMS: usize = 65_536;

fn too_many() -> ItemError {
    bad(format!("The request has more than {MAX_ITEMS} items, the most one request may carry."))
}

/// Why a piece of JSON text could not be read as items.
enum ReadError {
    /// Not JSON (or not all of it).
    Syntax,
    TooMany,
}

/// The objects of an array, one at a time, as the text they were written as. What is not an object is left out.
struct Objects<'a, 'b> {
    out: &'b mut Vec<&'a str>,
    /// Set when the reading stopped because there were too many.
    overflow: &'b mut bool,
}

impl<'de> serde::de::Visitor<'de> for Objects<'de, '_> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an array")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while let Some(raw) = seq.next_element::<&'de RawValue>()? {
            let text = raw.get();
            if text.starts_with('{') {
                if self.out.len() >= MAX_ITEMS {
                    *self.overflow = true;
                    return Err(serde::de::Error::custom("too many items"));
                }
                self.out.push(text);
            }
        }
        Ok(())
    }
}

/// Add the objects of `text` (one JSON value: an object, or an array that holds them) to `out`; the whole of `text` must be that
/// one value. A value that is neither is read and left out.
fn read_objects<'a>(text: &'a str, out: &mut Vec<&'a str>) -> Result<(), ReadError> {
    use serde::de::{Deserialize, Deserializer, IgnoredAny};
    let mut de = serde_json::Deserializer::from_str(text);
    let done = match text.as_bytes().first() {
        Some(b'[') => {
            let mut overflow = false;
            match (&mut de).deserialize_seq(Objects { out: &mut *out, overflow: &mut overflow }) {
                Ok(()) => Ok(()),
                Err(_) if overflow => return Err(ReadError::TooMany),
                Err(_) => Err(ReadError::Syntax),
            }
        }
        Some(b'{') => {
            let raw = <&RawValue>::deserialize(&mut de).map_err(|_| ReadError::Syntax)?;
            if out.len() >= MAX_ITEMS {
                return Err(ReadError::TooMany);
            }
            out.push(raw.get());
            Ok(())
        }
        _ => IgnoredAny::deserialize(&mut de).map(|_| ()).map_err(|_| ReadError::Syntax),
    };
    done?;
    de.end().map_err(|_| ReadError::Syntax)
}

/// The result of reading a body: the envelopes as the text they were written as (anything that is not a JSON object is left out,
/// as Azure does), or the refusal of the whole body. Nothing is parsed into a value here, so the memory used does not depend on
/// what is inside the items; each item is parsed when it is checked.
pub(crate) fn split_items(text: &[u8]) -> Result<Vec<&str>, ItemError> {
    let Ok(text) = std::str::from_utf8(text) else {
        return Ok(Vec::new());
    };
    let trimmed = text.trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    // the whole body is one array or one object; else one value per line. Azure skips a line that is not JSON at all
    // (plain text) and refuses the body when a line that starts like JSON does not parse.
    let mut items = Vec::new();
    match read_objects(trimmed, &mut items) {
        Ok(()) => return Ok(items),
        Err(ReadError::TooMany) => return Err(too_many()),
        Err(ReadError::Syntax) => items.clear(),
    }
    for line in trimmed.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !(line.starts_with('{') || line.starts_with('[')) {
            continue;
        }
        match read_objects(line, &mut items) {
            Ok(()) => {}
            Err(ReadError::TooMany) => return Err(too_many()),
            Err(ReadError::Syntax) => return Err(bad("Invalid JSON specified in the request")),
        }
    }
    Ok(items)
}

/// The length of an item's text without the white space between its tokens: the size Azure's limit is taken to mean, worked out
/// without parsing the item.
fn compact_len(text: &str) -> usize {
    let (mut n, mut in_string, mut escaped) = (0, false, false);
    for b in text.bytes() {
        if in_string {
            n += 1;
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else if b == b'"' {
            in_string = true;
            n += 1;
        } else if !matches!(b, b' ' | b'\n' | b'\r' | b'\t') {
            n += 1;
        }
    }
    n
}

/// `109`: a field that must be there is not.
fn missing(field: &str, ty: &str, expected: &str) -> ItemError {
    bad(format!("109: Field '{field}' on type '{ty}' is required but missing or empty. Expected: {expected}"))
}

/// `106`: a field has the wrong JSON type.
fn wrong_type(field: &str, ty: &str, expected: &str) -> ItemError {
    bad(format!("106: Field '{field}' on type '{ty}' is of incorrect type. Expected: {expected}"))
}

/// A required string (a number is taken too, as Azure does): missing, null or empty is `109`, an object or array `106`.
fn required_string<'a>(o: &'a Map<String, Value>, field: &str, ty: &str) -> Result<Option<&'a str>, ItemError> {
    match o.get(field) {
        None | Some(Value::Null) => Err(missing(field, ty, "string")),
        Some(Value::String(s)) if s.is_empty() => Err(missing(field, ty, "string")),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(Value::Number(_)) => Ok(None),
        Some(_) => Err(wrong_type(field, ty, "string")),
    }
}

fn optional_object(o: &Map<String, Value>, field: &str, ty: &str) -> Result<(), ItemError> {
    match o.get(field) {
        None | Some(Value::Null) | Some(Value::Object(_)) => Ok(()),
        Some(_) => Err(wrong_type(field, ty, "object")),
    }
}

fn optional_bool(o: &Map<String, Value>, field: &str, ty: &str) -> Result<(), ItemError> {
    match o.get(field) {
        None | Some(Value::Null) | Some(Value::Bool(_)) => Ok(()),
        Some(_) => Err(wrong_type(field, ty, "boolean")),
    }
}

/// `[d.]hh:mm:ss[.fffffff]`, at most 1000 days (what Azure calls "dd.hh:mm:ss.fffffff format not exceeding 1000 days").
pub(crate) fn valid_timespan(s: &str) -> bool {
    let (days, rest) = match s.split_once('.') {
        // a dot before the first colon separates the days
        Some((d, r)) if !d.contains(':') => (Some(d), r),
        _ => (None, s),
    };
    if days
        .is_some_and(|d| d.is_empty() || d.len() > 4 || !d.bytes().all(|b| b.is_ascii_digit()) || d.parse::<u32>().is_ok_and(|n| n > 1000))
    {
        return false;
    }
    let (hms, frac) = match rest.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (rest, None),
    };
    if frac.is_some_and(|f| f.is_empty() || f.len() > 7 || !f.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    let parts: Vec<&str> = hms.split(':').collect();
    let [h, m, sec] = parts.as_slice() else { return false };
    let num = |t: &str, max: u32| {
        t.len() <= 2 && !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) && t.parse::<u32>().is_ok_and(|n| n <= max)
    };
    num(h, 23) && num(m, 59) && num(sec, 59)
}

fn timespan(o: &Map<String, Value>, field: &str, ty: &str, required: bool) -> Result<(), ItemError> {
    match o.get(field) {
        None | Some(Value::Null) if required => Err(missing(field, ty, "timespan")),
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(s)) if s.is_empty() && required => Err(missing(field, ty, "timespan")),
        Some(Value::String(s)) if valid_timespan(s) => Ok(()),
        Some(Value::String(_)) => {
            Err(bad(format!("108: Field '{field}' on type '{ty}' is not a valid duration string. Expected: dd.hh:mm:ss.fffffff format not exceeding 1000 days")))
        }
        Some(_) => Err(wrong_type(field, ty, "timespan")),
    }
}

/// `severityLevel`: 0 to 4, or the name of a level.
fn severity(o: &Map<String, Value>, ty: &str) -> Result<(), ItemError> {
    let ok = match o.get("severityLevel") {
        None | Some(Value::Null) => true,
        Some(Value::Number(n)) => n.as_i64().is_some_and(|n| (0..=4).contains(&n)),
        Some(Value::String(s)) => ["verbose", "information", "warning", "error", "critical"].contains(&s.to_ascii_lowercase().as_str()),
        Some(_) => false,
    };
    if ok {
        Ok(())
    } else {
        Err(bad(format!("107: Field 'severityLevel' on type '{ty}' contains invalid enum value. Expected: one of 0,1,2,3,4")))
    }
}

/// The fields of `baseData` that Azure insists on, for each base type (found by sending Azure empty ones in November 2026).
fn check_base_data(ty: &str, b: &Map<String, Value>) -> Result<(), ItemError> {
    optional_object(b, "properties", ty)?;
    optional_object(b, "measurements", ty)?;
    match ty {
        "RequestData" => {
            required_string(b, "id", ty)?;
            timespan(b, "duration", ty, true)?;
            required_string(b, "responseCode", ty)?;
            optional_bool(b, "success", ty)?;
        }
        "RemoteDependencyData" => {
            required_string(b, "name", ty)?;
            timespan(b, "duration", ty, false)?;
            optional_bool(b, "success", ty)?;
        }
        "ExceptionData" => {
            let Some(Value::Array(list)) = b.get("exceptions") else {
                return Err(wrong_type("exceptions", ty, "array"));
            };
            for e in list {
                let Value::Object(e) = e else { return Err(wrong_type("exceptions", ty, "object")) };
                required_string(e, "typeName", "ExceptionDetails")?;
                required_string(e, "message", "ExceptionDetails")?;
            }
            severity(b, ty)?;
        }
        "PageViewData" => {
            required_string(b, "name", ty)?;
            timespan(b, "duration", ty, false)?;
        }
        "AvailabilityData" => {
            timespan(b, "duration", ty, true)?;
            optional_bool(b, "success", ty)?;
        }
        "MessageData" => {
            required_string(b, "message", ty)?;
            severity(b, ty)?;
        }
        "EventData" => {
            required_string(b, "name", ty)?;
        }
        "MetricData" => {
            let Some(Value::Array(list)) = b.get("metrics") else {
                return Err(wrong_type("metrics", ty, "array"));
            };
            for m in list {
                let Value::Object(m) = m else { return Err(wrong_type("metrics", ty, "object")) };
                required_string(m, "name", "DataPoint")?;
                if !m.get("value").is_some_and(Value::is_number) {
                    return Err(wrong_type("value", "DataPoint", "double"));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Check one envelope as far as Azure does without knowing the key, in Azure's order (the first problem is the one
/// reported), and read what is kept of it.
pub(crate) fn check(item: &Value, now: OffsetDateTime, received: &str) -> Result<Telemetry, ItemError> {
    let Value::Object(o) = item else {
        return Err(bad("InvalidJsonObject: Not a valid JSON object"));
    };
    let text = item.to_string();
    if text.len() > MAX_ITEM_BYTES {
        return Err(bad(too_long()));
    }
    // any GUID form is a key: with or without dashes, in braces, in either case. The emulator cannot know which keys exist.
    let ikey = match o.get("iKey") {
        Some(Value::String(s)) => uuid::Uuid::parse_str(s.trim()).ok().map(|u| u.hyphenated().to_string()),
        _ => None,
    }
    .ok_or_else(|| bad("Invalid instrumentation key"))?;
    let data = o.get("data").and_then(Value::as_object);
    let base = data.and_then(|d| d.get("baseData")).and_then(Value::as_object);
    let (Some(data), Some(base)) = (data, base) else {
        return Err(bad("InvalidJsonObject: Not a valid JSON object"));
    };
    match o.get("ver") {
        None | Some(Value::Null) | Some(Value::Number(_)) => {}
        Some(Value::String(v)) => {
            return Err(bad(format!("EnvelopeSchemaVersionUnsupported: Envelope Schema version '{v}' is not supported.")))
        }
        Some(_) => return Err(wrong_type("ver", "Envelope", "int")),
    }
    let envelope_name = required_string(o, "name", "Envelope")?.map(str::to_string).unwrap_or_else(|| o["name"].to_string());
    let when = match o.get("time") {
        None | Some(Value::Null) => return Err(missing("time", "Envelope", "date")),
        Some(Value::String(s)) if s.is_empty() => return Err(missing("time", "Envelope", "date")),
        Some(Value::String(s)) => azemu_core::time::parse_iso8601(s)
            .ok_or_else(|| bad("102: Field 'time' on type 'Envelope' is not a valid time string. Expected: date"))?,
        Some(_) => return Err(wrong_type("time", "Envelope", "date")),
    };
    let age_ms = (now - when).whole_milliseconds();
    if age_ms > MAX_AGE_MS as i128 {
        return Err(bad(format!(
            "103: Field 'time' on type 'Envelope' is older than the allowed min date. Expected: now - {MAX_AGE_MS}ms"
        )));
    }
    if -age_ms > MAX_AHEAD_MS as i128 {
        return Err(bad(format!(
            "104: Field 'time' on type 'Envelope' is newer than the allowed max date. Expected: now + {MAX_AHEAD_MS}ms"
        )));
    }
    if !matches!(o.get("sampleRate"), None | Some(Value::Null) | Some(Value::Number(_))) {
        return Err(wrong_type("sampleRate", "Envelope", "double"));
    }
    if !matches!(o.get("flags"), None | Some(Value::Null)) && !o.get("flags").is_some_and(|f| f.is_i64() || f.is_u64()) {
        return Err(wrong_type("flags", "Envelope", "long"));
    }
    optional_object(o, "tags", "Envelope")?;
    let base_type = data.get("baseType").and_then(Value::as_str).unwrap_or("");
    if !BASE_TYPES.contains(&base_type) {
        return Err(bad(format!("Unknown domain type \"{base_type}\"")));
    }
    check_base_data(base_type, base)?;

    let mut names = Vec::new();
    if let Some(n) = base.get("name").and_then(Value::as_str) {
        names.push(n.to_string());
    }
    if let Some(Value::Array(metrics)) = base.get("metrics") {
        names.extend(metrics.iter().filter_map(|m| m.get("name").and_then(Value::as_str)).map(str::to_string));
    }
    let operation_id =
        o.get("tags").and_then(Value::as_object).and_then(|t| t.get("ai.operation.id")).and_then(Value::as_str).map(str::to_string);
    Ok(Telemetry {
        received: received.to_string(),
        ikey,
        base_type: base_type.to_string(),
        envelope_name,
        names,
        operation_id,
        envelope: RawValue::from_string(text).map_err(|_| bad("InvalidJsonObject: Not a valid JSON object"))?,
    })
}

fn answer(status: u16, received: usize, accepted: usize, errors: &[(usize, ItemError)]) -> Response {
    let list: Vec<Value> = errors.iter().map(|(i, e)| json!({"index": i, "statusCode": e.status, "message": e.message})).collect();
    Resp::new(status)
        .header("x-content-type-options", "nosniff")
        .json(&json!({"itemsReceived": received, "itemsAccepted": accepted, "appId": null, "errors": list}))
        .build()
}

/// A request that cannot be read at all, with one error for the whole of it.
fn refused(status: u16, received: usize, message: &str) -> Response {
    answer(status, received, 0, &[(0, ItemError { status, message: message.to_string() })])
}

/// The answer for a body over the limit.
pub(crate) fn too_large() -> Response {
    refused(413, 0, "The request body is too large")
}

/// What a browser SDK asks before it posts from a page (`OPTIONS`): the answer Azure gives, with the same headers.
pub(crate) fn preflight() -> Response {
    Resp::new(204u16)
        .header("access-control-allow-origin", "*")
        .header(
            "access-control-allow-headers",
            "Origin,X-Requested-With,Content-Name,Content-Type,Accept,Cache-Control,Sdk-Context,X-Set-Cross-Origin-Resource-Policy,Content-Encoding",
        )
        .header("access-control-allow-methods", "POST")
        .header("access-control-max-age", "3600")
        .build()
}

impl MonitorService {
    pub(crate) fn track(&self, req: &Request) -> Response {
        let mut resp = self.track_inner(req);
        // Azure adds this to its answer when the request names an origin (a browser SDK)
        if req.header("origin").is_some() {
            resp.headers_mut().insert("access-control-allow-origin", azemu_core::http::HeaderValue::from_static("*"));
        }
        resp
    }

    fn track_inner(&self, req: &Request) -> Response {
        let bytes = match body::decode(req, MAX_DECODED_BYTES, MAX_DECODED_BYTES) {
            Ok(b) => b,
            Err(BodyError::TooLarge) => return too_large(),
            // Azure answers a body that says gzip and is not with this shape, not the one of the other answers
            Err(BodyError::BadGzip) => return Resp::new(400u16).json(&json!({"message": "Malformed content", "statusCode": 400})).build(),
            // an encoding Azure does not know is not undone: the body is read as it came
            Err(BodyError::Encoding(_)) => req.body.to_vec(),
        };
        let items = match split_items(&bytes) {
            Ok(i) => i,
            Err(e) => return refused(400, 1, &e.message),
        };
        let received = azemu_core::time::iso8601_7(req.ctx.time);
        let mut kept = Vec::new();
        let mut errors = Vec::new();
        for (index, text) in items.iter().enumerate() {
            // an item over the length is refused as it is, without making a value of it (a value takes thirty times the text)
            if compact_len(text) > MAX_ITEM_BYTES {
                errors.push((index, bad(too_long())));
                continue;
            }
            let Ok(item) = serde_json::from_str::<Value>(text) else {
                errors.push((index, bad("InvalidJsonObject: Not a valid JSON object")));
                continue;
            };
            match check(&item, req.ctx.time, &received) {
                Ok(t) => {
                    let size = t.held_bytes();
                    kept.push((t, size));
                }
                Err(e) => errors.push((index, e)),
            }
        }
        let accepted = kept.len();
        let dropped = self.telemetry.push_all(kept);
        if dropped > 0 {
            tracing::debug!(dropped, "monitor: the oldest telemetry was dropped to stay inside the limits");
        }
        let status = if errors.is_empty() {
            200
        } else if accepted == 0 {
            400
        } else {
            206
        };
        answer(status, items.len(), accepted, &errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::Duration;

    fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    const KEY: &str = "00000000-0000-0000-0000-000000000001";

    fn envelope(time: OffsetDateTime) -> Value {
        json!({"name": "Microsoft.ApplicationInsights.Request", "time": azemu_core::time::iso8601_7(time), "iKey": KEY,
               "tags": {"ai.operation.id": "op1"}, "data": {"baseType": "EventData", "baseData": {"name": "GET /x"}}})
    }

    fn with_data(base_type: &str, base_data: Value) -> Value {
        let mut e = envelope(now());
        e["data"] = json!({"baseType": base_type, "baseData": base_data});
        e
    }

    fn message(v: &Value) -> String {
        check(v, now(), "r").unwrap_err().message
    }

    #[test]
    fn splits_array_object_and_lines() {
        assert_eq!(split_items(b"[{},{}]").unwrap().len(), 2);
        assert_eq!(split_items(b"{\"a\":1}").unwrap().len(), 1);
        assert_eq!(split_items(b"{\"a\":1}\n{\"b\":2}\n\n{\"c\":3}\n").unwrap().len(), 3);
        assert_eq!(split_items(b"[{\"a\":1}]\n[{\"b\":2}]").unwrap().len(), 2);
        assert_eq!(split_items(b"  ").unwrap().len(), 0);
        assert_eq!(split_items(&[0xef, 0xbb, 0xbf, b'{', b'}']).unwrap().len(), 1, "a byte order mark is skipped");
    }

    #[test]
    fn what_is_not_an_object_is_left_out_as_azure_does() {
        assert_eq!(split_items(b"[1,2,\"x\",null,{}]").unwrap().len(), 1);
        assert_eq!(split_items(b"this is not json").unwrap().len(), 0);
        assert_eq!(split_items(b"{\"a\":1}\nnot json\n{\"b\":2}").unwrap().len(), 2);
        assert_eq!(split_items(&[0xff, 0xfe]).unwrap().len(), 0);
        // but a line that starts like JSON and is cut off refuses the body
        assert_eq!(split_items(b"[{\"name\":\"x\"").unwrap_err().message, "Invalid JSON specified in the request");
        assert!(split_items(b"{\"a\":1}\n{\"b\":").is_err());
    }

    #[test]
    fn a_good_envelope_is_read() {
        let t = check(&envelope(now()), now(), "r").unwrap();
        assert_eq!(t.base_type, "EventData");
        assert_eq!(t.names, vec!["GET /x"]);
        assert_eq!(t.operation_id.as_deref(), Some("op1"));
        assert_eq!(t.ikey, KEY);
    }

    #[test]
    fn the_key_is_any_guid_form() {
        for k in ["00000000-0000-0000-0000-00000000000A", "0000000000000000000000000000000a", "{00000000-0000-0000-0000-00000000000a}"] {
            let mut e = envelope(now());
            e["iKey"] = json!(k);
            assert_eq!(check(&e, now(), "r").unwrap().ikey, "00000000-0000-0000-0000-00000000000a", "{k}");
        }
        for bad in [json!("abc"), json!(5), json!(""), json!(null)] {
            let mut e = envelope(now());
            e["iKey"] = bad;
            assert_eq!(message(&e), "Invalid instrumentation key");
        }
        let mut e = envelope(now());
        e.as_object_mut().unwrap().remove("iKey");
        assert_eq!(message(&e), "Invalid instrumentation key");
        // the names of the fields have exactly this case
        let mut e = envelope(now());
        let k = e.as_object_mut().unwrap().remove("iKey").unwrap();
        e["IKEY"] = k;
        assert_eq!(message(&e), "Invalid instrumentation key");
    }

    #[test]
    fn metric_names_are_collected() {
        let e = with_data("MetricData", json!({"metrics": [{"name": "m1", "value": 1}, {"name": "m2", "value": 2.5}]}));
        assert_eq!(check(&e, now(), "r").unwrap().names, vec!["m1", "m2"]);
    }

    #[test]
    fn missing_fields_in_azures_words() {
        let mut e = envelope(now());
        e.as_object_mut().unwrap().remove("name");
        assert_eq!(message(&e), "109: Field 'name' on type 'Envelope' is required but missing or empty. Expected: string");
        let mut e = envelope(now());
        e.as_object_mut().unwrap().remove("time");
        assert_eq!(message(&e), "109: Field 'time' on type 'Envelope' is required but missing or empty. Expected: date");
        let mut e = envelope(now());
        e["name"] = json!("");
        assert!(message(&e).starts_with("109: Field 'name'"));
        let mut e = envelope(now());
        e.as_object_mut().unwrap().remove("data");
        assert_eq!(message(&e), "InvalidJsonObject: Not a valid JSON object");
        assert_eq!(message(&with_data("NopeData", json!({}))), "Unknown domain type \"NopeData\"");
        assert_eq!(message(&with_data("eventdata", json!({"name": "x"}))), "Unknown domain type \"eventdata\"");
        let mut e = envelope(now());
        e["data"] = json!({"baseData": {"name": "x"}});
        assert_eq!(message(&e), "Unknown domain type \"\"");
    }

    #[test]
    fn the_first_problem_in_azures_order_is_the_one_reported() {
        // key, then data, then name, then time
        let mut e = envelope(now() - Duration::hours(100));
        e.as_object_mut().unwrap().remove("name");
        e.as_object_mut().unwrap().remove("iKey");
        assert_eq!(message(&e), "Invalid instrumentation key");
        e["iKey"] = json!(KEY);
        e.as_object_mut().unwrap().remove("data");
        assert_eq!(message(&e), "InvalidJsonObject: Not a valid JSON object");
        e["data"] = json!({"baseType": "EventData", "baseData": {}});
        assert!(message(&e).starts_with("109: Field 'name' on type 'Envelope'"));
        e["name"] = json!("n");
        assert!(message(&e).starts_with("103: "));
    }

    #[test]
    fn time_limits_and_forms() {
        assert_eq!(
            message(&envelope(now() - Duration::hours(72))),
            "103: Field 'time' on type 'Envelope' is older than the allowed min date. Expected: now - 172800000ms"
        );
        assert!(check(&envelope(now() - Duration::hours(47)), now(), "r").is_ok());
        assert_eq!(
            message(&envelope(now() + Duration::hours(3))),
            "104: Field 'time' on type 'Envelope' is newer than the allowed max date. Expected: now + 7200000ms"
        );
        assert!(check(&envelope(now() + Duration::minutes(110)), now(), "r").is_ok());
        let mut e = envelope(now());
        e["time"] = json!("yesterday");
        assert_eq!(message(&e), "102: Field 'time' on type 'Envelope' is not a valid time string. Expected: date");
        e["time"] = json!(12345);
        assert_eq!(message(&e), "106: Field 'time' on type 'Envelope' is of incorrect type. Expected: date");
        // no fraction and no zone are taken
        let today = azemu_core::time::iso8601_secs(now());
        for t in [today.clone(), today.trim_end_matches('Z').to_string()] {
            let mut e = envelope(now());
            e["time"] = json!(t);
            assert!(check(&e, now(), "r").is_ok(), "{t}");
        }
    }

    #[test]
    fn envelope_field_types() {
        let mut e = envelope(now());
        e["sampleRate"] = json!("x");
        assert_eq!(message(&e), "106: Field 'sampleRate' on type 'Envelope' is of incorrect type. Expected: double");
        let mut e = envelope(now());
        e["tags"] = json!("x");
        assert_eq!(message(&e), "106: Field 'tags' on type 'Envelope' is of incorrect type. Expected: object");
        let mut e = envelope(now());
        e["flags"] = json!("x");
        assert!(message(&e).contains("Field 'flags'"));
        let mut e = envelope(now());
        e["ver"] = json!("x");
        assert_eq!(message(&e), "EnvelopeSchemaVersionUnsupported: Envelope Schema version 'x' is not supported.");
        // a number as a tag value and as a name is taken
        let mut e = envelope(now());
        e["tags"]["ai.operation.id"] = json!(5);
        e["name"] = json!(5);
        assert!(check(&e, now(), "r").is_ok());
    }

    #[test]
    fn what_each_type_needs() {
        let ok = |t: &str, d: Value| assert!(check(&with_data(t, d.clone()), now(), "r").is_ok(), "{t} {d}");
        ok("RequestData", json!({"id": "1", "duration": "0.00:00:00.012", "responseCode": "200"}));
        ok("RequestData", json!({"id": "1", "duration": "00:00:00", "responseCode": 200, "success": true}));
        ok("RemoteDependencyData", json!({"name": "x"}));
        ok("PageViewData", json!({"name": "x"}));
        ok("AvailabilityData", json!({"duration": "00:00:01"}));
        ok("MessageData", json!({"message": "m", "severityLevel": "Warning"}));
        ok("EventData", json!({"name": "e", "properties": {"a": 5}}));
        ok("ExceptionData", json!({"exceptions": []}));
        ok("ExceptionData", json!({"exceptions": [{"typeName": "T", "message": "m"}]}));
        ok("MetricData", json!({"metrics": []}));
        ok("MetricData", json!({"metrics": [{"name": "m", "value": 1}]}));

        let m = |t: &str, d: Value| message(&with_data(t, d));
        assert_eq!(m("RequestData", json!({})), "109: Field 'id' on type 'RequestData' is required but missing or empty. Expected: string");
        assert_eq!(
            m("RequestData", json!({"id": "1"})),
            "109: Field 'duration' on type 'RequestData' is required but missing or empty. Expected: timespan"
        );
        assert_eq!(
            m("RequestData", json!({"id": "1", "duration": "abc"})),
            "108: Field 'duration' on type 'RequestData' is not a valid duration string. Expected: dd.hh:mm:ss.fffffff format not exceeding 1000 days"
        );
        assert!(m("RequestData", json!({"id": "1", "duration": "00:00:00"})).contains("Field 'responseCode'"));
        assert!(m("RequestData", json!({"id": "1", "duration": "00:00:00", "responseCode": "200", "success": "yes"}))
            .contains("Expected: boolean"));
        assert!(m("RemoteDependencyData", json!({})).contains("Field 'name' on type 'RemoteDependencyData'"));
        assert!(m("PageViewData", json!({})).contains("Field 'name' on type 'PageViewData'"));
        assert!(m("AvailabilityData", json!({})).contains("Field 'duration' on type 'AvailabilityData'"));
        assert!(m("MessageData", json!({})).contains("Field 'message' on type 'MessageData'"));
        assert_eq!(
            m("MessageData", json!({"message": "m", "severityLevel": 9})),
            "107: Field 'severityLevel' on type 'MessageData' contains invalid enum value. Expected: one of 0,1,2,3,4"
        );
        assert!(m("EventData", json!({})).contains("Field 'name' on type 'EventData'"));
        assert_eq!(
            m("EventData", json!({"name": "e", "properties": "x"})),
            "106: Field 'properties' on type 'EventData' is of incorrect type. Expected: object"
        );
        assert_eq!(m("ExceptionData", json!({})), "106: Field 'exceptions' on type 'ExceptionData' is of incorrect type. Expected: array");
        assert!(m("ExceptionData", json!({"exceptions": [{}]})).contains("Field 'typeName' on type 'ExceptionDetails'"));
        assert!(m("ExceptionData", json!({"exceptions": [{"typeName": "T"}]})).contains("Field 'message' on type 'ExceptionDetails'"));
        assert_eq!(m("MetricData", json!({})), "106: Field 'metrics' on type 'MetricData' is of incorrect type. Expected: array");
        assert!(m("MetricData", json!({"metrics": [{}]})).contains("Field 'name' on type 'DataPoint'"));
        assert_eq!(
            m("MetricData", json!({"metrics": [{"name": "m"}]})),
            "106: Field 'value' on type 'DataPoint' is of incorrect type. Expected: double"
        );
    }

    #[test]
    fn timespans() {
        for ok in ["00:00:00", "0.00:00:00.012", "1.02:03:04.0123456", "1000.00:00:00", "23:59:59.9"] {
            assert!(valid_timespan(ok), "{ok}");
        }
        for bad in [
            "",
            "abc",
            "1:2",
            "24:00:00",
            "00:60:00",
            "00:00:60",
            "1001.00:00:00",
            "00:00:00.",
            "00:00:00.12345678",
            ".00:00:00",
            "-1.00:00:00",
        ] {
            assert!(!valid_timespan(bad), "{bad}");
        }
    }

    #[test]
    fn an_item_over_the_length_limit() {
        let e = with_data("EventData", json!({"name": "x", "properties": {"p": "x".repeat(1_300_000)}}));
        assert_eq!(message(&e), "Telemetry item length must not exceed 1258291");
    }

    #[test]
    fn not_an_object() {
        assert!(check(&json!(5), now(), "r").is_err());
    }
}
