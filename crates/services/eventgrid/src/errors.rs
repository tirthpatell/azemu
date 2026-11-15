//! Error bodies in the shape Event Grid sends:
//! `{"error":{"code":…,"message":…,"details":[{"code":…,"message":…}]}}`.

use azemu_core::resp::Resp;
use azemu_core::{Request, Response};
use serde_json::{json, Value};

pub fn body(code: &str, message: &str, detail_code: Option<&str>) -> Value {
    let mut err = json!({ "code": code, "message": message });
    if let Some(d) = detail_code {
        err["details"] = json!([{ "code": d, "message": message }]);
    }
    json!({ "error": err })
}

/// An error answer. The details entry repeats code and message, as Event Grid does for
/// authorization and size errors.
pub fn error(status: u16, code: &str, message: &str, detail_code: Option<&str>) -> Response {
    Resp::new(status).header("x-ms-error-code", code).json(&body(code, message, detail_code)).build()
}

pub fn bad_request(message: &str) -> Response {
    error(400, "BadRequest", message, None)
}

/// 400 for events that do not match the expected schema.
pub fn invalid_input(schema_label: &str, message: &str) -> Response {
    let full = format!(
        "This resource is configured to receive event in '{schema_label}' schema. The JSON received does not conform to the expected schema. {message}"
    );
    error(400, "BadRequest", &full, Some("InputJsonInvalid"))
}

pub fn not_found(message: &str) -> Response {
    error(404, "NotFound", message, None)
}

/// What a failed check says, before it is turned into an answer (the tail of the message needs the request).
#[derive(Debug)]
pub struct Fail {
    pub status: u16,
    pub code: &'static str,
    /// The `details[0].code`.
    pub detail: &'static str,
    pub message: String,
}

impl Fail {
    pub fn new(status: u16, code: &'static str, detail: &'static str, message: impl Into<String>) -> Fail {
        Fail { status, code, detail, message: message.into() }
    }

    /// 400 `BadRequest` with the inner code `InputJsonInvalid`.
    pub fn input_json_invalid(message: impl Into<String>) -> Fail {
        Fail::new(400, "BadRequest", "InputJsonInvalid", message)
    }

    pub fn bad_request(message: impl Into<String>) -> Fail {
        Fail::input_json_invalid(message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Fail {
        Fail::new(401, "Unauthorized", "Unauthorized", message)
    }

    /// The answer, with the sentence Event Grid puts at the end of nearly every message.
    pub fn into_response(self, req: &Request) -> Response {
        let message = format!("{} {}", self.message, report(req));
        error(self.status, self.code, &message, Some(self.detail))
    }
}

/// `11/14/2026 5:34:02 AM`, the way the service writes a time in its messages.
pub fn us_time(t: azemu_core::time::OffsetDateTime) -> String {
    let (h12, half) = match t.hour() {
        0 => (12, "AM"),
        h @ 1..=11 => (h, "AM"),
        12 => (12, "PM"),
        h => (h - 12, "PM"),
    };
    format!("{}/{}/{} {h12}:{:02}:{:02} {half}", u8::from(t.month()), t.day(), t.year(), t.minute(), t.second())
}

/// `Report '<id>:<n>:<time> (UTC)' to our forums for assistance or raise a support ticket.`
pub fn report(req: &Request) -> String {
    // The middle number is some counter of the service; it only has to look like one.
    let n = req.ctx.request_id.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32)) % 15;
    format!("Report '{}:{n}:{} (UTC)' to our forums for assistance or raise a support ticket.", req.ctx.request_id, us_time(req.ctx.time))
}

/// 401 for a key or token Event Grid does not accept. `host` is the host the request went to.
pub fn unauthorized_key(req: &Request, host: &str, reason: Option<&str>) -> Response {
    let mut m = format!("The request authorization key is not authorized for {}.", host.to_ascii_uppercase());
    if let Some(r) = reason {
        m.push_str(&format!(" This is due to the reason: {r}"));
    }
    Fail::unauthorized(m).into_response(req)
}

/// 400 `UnsupportedApiVersion`; this one has no details list and no report sentence.
pub fn unsupported_api_version(uri: &str, version: &str) -> Response {
    let msg = format!("The HTTP resource that matches the request URI '{uri}' does not support the API version '{version}'.");
    Resp::new(400u16)
        .header("x-ms-error-code", "UnsupportedApiVersion")
        .json(&json!({ "error": { "code": "UnsupportedApiVersion", "message": msg, "innerError": Value::Null } }))
        .build()
}

/// 413 for one event over the size Event Grid takes: the message names the limit of the JSON content.
pub fn event_too_large(req: &Request, limit: usize) -> Response {
    Fail::new(
        413,
        "RequestEntityTooLarge",
        "PayloadTooLarge",
        format!("The maximum size for the JSON content ({limit}) has been exceeded."),
    )
    .into_response(req)
}

/// 404 for a path the topic host does not have.
pub fn no_resource(req: &Request) -> Response {
    let uri = format!("{}{}", req.ctx.endpoint.base_url(), req.path);
    let encoded: String = uri
        .bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect();
    Fail::new(404, "NotFound", "ResourceNotFound", format!("No HTTP resource was found that matches the request URI '{encoded}'."))
        .into_response(req)
}

/// 405 for a method the publish path does not take: an empty reason, then the report sentence, and the `Allow` header.
pub fn publish_method_not_allowed(req: &Request) -> Response {
    let mut r = Fail::new(405, "MethodNotAllowed", "MethodNotAllowed", ".").into_response(req);
    r.headers_mut().insert("allow", azemu_core::http::HeaderValue::from_static("OPTIONS, POST"));
    r
}

pub fn payload_too_large(max: usize) -> Response {
    let msg = format!("The maximum size ({max}) has been exceeded.");
    error(413, "RequestEntityTooLarge", &msg, Some("PayloadTooLarge"))
}

/// 501 naming the operation that is not built.
pub fn not_implemented(what: &str) -> Response {
    error(501, "NotImplemented", &format!("eventgrid: {what}"), None)
}

pub fn method_not_allowed(what: &str) -> Response {
    error(405, "MethodNotAllowed", &format!("eventgrid: {what}"), None)
}
