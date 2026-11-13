//! Error values and their wire forms.
//!
//! Table operations answer errors as OData JSON
//! (`{"odata.error":{"code":..,"message":{"lang":"en-US","value":..}}}`). The
//! operations with XML bodies (stored access policies, service properties and
//! statistics) answer an `m:error` XML document instead.

use azemu_core::resp::Resp;
use azemu_core::{time, RequestCtx, Response};

/// What went wrong, before it is rendered.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
    /// Sent as `WWW-Authenticate` (the bearer token challenge).
    pub challenge: Option<String>,
    /// For a 501: the operation that is not built, as `GET /_azemu/unsupported` shows it.
    pub unsupported: Option<String>,
}

pub type Res<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: u16, code: &str, message: &str) -> ApiError {
        ApiError { status, code: code.to_string(), message: message.to_string(), challenge: None, unsupported: None }
    }

    /// Put the index of the failing batch operation in front of the message
    /// (`"2:The specified entity already exists."`), as batch responses do.
    pub fn at_index(mut self, index: usize) -> ApiError {
        self.message = format!("{index}:{}", self.message);
        self
    }
}

pub fn invalid_input() -> ApiError {
    ApiError::new(400, "InvalidInput", "One of the request inputs is not valid.")
}

pub fn invalid_value() -> ApiError {
    ApiError::new(400, "InvalidInput", "An error occurred while processing this request.")
}

pub fn invalid_input_msg(msg: &str) -> ApiError {
    ApiError::new(400, "InvalidInput", msg)
}

pub fn table_not_found() -> ApiError {
    ApiError::new(404, "TableNotFound", "The table specified does not exist.")
}

pub fn resource_not_found() -> ApiError {
    ApiError::new(404, "ResourceNotFound", "The specified resource does not exist.")
}

pub fn entity_exists() -> ApiError {
    ApiError::new(409, "EntityAlreadyExists", "The specified entity already exists.")
}

pub fn table_exists() -> ApiError {
    ApiError::new(409, "TableAlreadyExists", "The table specified already exists.")
}

pub fn condition_failed() -> ApiError {
    ApiError::new(412, "UpdateConditionNotSatisfied", "The update condition specified in the request was not satisfied.")
}

pub fn needs_value() -> ApiError {
    ApiError::new(400, "PropertiesNeedValue", "The values are not specified for all properties in the entity.")
}

pub fn invalid_xml() -> ApiError {
    ApiError::new(400, "InvalidXmlDocument", "XML specified is not syntactically valid.")
}

/// `operation` is what `GET /_azemu/unsupported` shows (`Table_Unknown (GET Entity)`): a shape, no names.
pub fn not_implemented(what: &str, operation: &str) -> ApiError {
    let mut e = ApiError::new(501, "NotImplemented", &format!("table: {what}"));
    e.unsupported = Some(operation.to_string());
    e
}

pub fn auth_failed() -> ApiError {
    ApiError::new(
        403,
        "AuthenticationFailed",
        "Server failed to authenticate the request. Make sure the value of Authorization header is formed correctly including the signature.",
    )
}

pub fn authorization_failure() -> ApiError {
    ApiError::new(403, "AuthorizationFailure", "This request is not authorized to perform this operation.")
}

pub fn permission_mismatch() -> ApiError {
    ApiError::new(403, "AuthorizationPermissionMismatch", "This request is not authorized to perform this operation using this permission.")
}

pub fn resource_type_mismatch() -> ApiError {
    ApiError::new(
        403,
        "AuthorizationResourceTypeMismatch",
        "This request is not authorized to perform this operation using this resource type.",
    )
}

pub fn service_mismatch() -> ApiError {
    ApiError::new(403, "AuthorizationServiceMismatch", "This request is not authorized to perform this operation using this service.")
}

pub fn protocol_mismatch() -> ApiError {
    ApiError::new(403, "AuthorizationProtocolMismatch", "This request is not authorized to perform this operation using this protocol.")
}

fn detail(e: &ApiError, ctx: &RequestCtx) -> String {
    format!("{}\nRequestId:{}\nTime:{}", e.message, ctx.request_id, time::iso8601_7(ctx.time))
}

/// The JSON error body (also used for the failing part of a batch response).
pub fn json_body(e: &ApiError, ctx: &RequestCtx) -> String {
    let mut out = String::from("{\"odata.error\":{\"code\":");
    crate::json::write_str(&mut out, &e.code);
    out.push_str(",\"message\":{\"lang\":\"en-US\",\"value\":");
    crate::json::write_str(&mut out, &detail(e, ctx));
    out.push_str("}}}");
    out
}

pub fn xml_body(e: &ApiError, ctx: &RequestCtx) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><m:error xmlns:m=\"http://schemas.microsoft.com/ado/2007/08/dataservices/metadata\"><m:code>{}</m:code><m:message xml:lang=\"en-US\">{}</m:message></m:error>",
        crate::xml::escape(&e.code),
        crate::xml::escape(&detail(e, ctx)),
    )
}

/// Render an error as a full HTTP response.
pub fn render(e: &ApiError, ctx: &RequestCtx, xml: bool) -> Response {
    let mut r = Resp::new(e.status).header("x-ms-error-code", &e.code);
    if xml {
        r = r.xml(xml_body(e, ctx));
    } else {
        r = r.header("Content-Type", crate::odata::CT_MINIMAL).header("X-Content-Type-Options", "nosniff").body(json_body(e, ctx));
    }
    if let Some(c) = &e.challenge {
        r = r.header("WWW-Authenticate", c);
    }
    let resp = r.build();
    match (e.status, &e.unsupported) {
        (501, Some(op)) => azemu_core::unsupported::mark(resp, op.clone()),
        _ => resp,
    }
}
