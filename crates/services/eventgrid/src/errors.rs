//! Error bodies in the shape Event Grid sends:
//! `{"error":{"code":…,"message":…,"details":[{"code":…,"message":…}]}}`.

use azemu_core::resp::Resp;
use azemu_core::Response;
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

pub fn unauthorized(host: &str, reason: &str) -> Response {
    let msg =
        format!("The request authorization key is not authorized for {}. This is due to the reason: {reason}", host.to_ascii_uppercase());
    error(401, "Unauthorized", &msg, Some("Unauthorized"))
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
