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
    /// How the body is written (see [`Form`]).
    pub form: Form,
}

/// How an error is written, where Azure's answers differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Form {
    /// OData JSON (or `m:error` XML for the operations with XML bodies), the Content-Type of the table operations.
    #[default]
    Table,
    /// OData JSON with the plain Content-Type `application/json`: what the front end sends for a credential it refuses.
    Credential,
    /// The XML document `<error xmlns="...">` with indented elements, Content-Type `application/xml`: an `x-ms-version`
    /// it does not know.
    Version,
}

pub type Res<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: u16, code: &str, message: &str) -> ApiError {
        ApiError { status, code: code.to_string(), message: message.to_string(), challenge: None, unsupported: None, form: Form::Table }
    }

    /// Written as an answer to a credential (see [`Form::Credential`]).
    pub fn credential(mut self) -> ApiError {
        self.form = Form::Credential;
        self
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
    .credential()
}

pub fn authorization_failure() -> ApiError {
    ApiError::new(403, "AuthorizationFailure", "This request is not authorized to perform this operation.").credential()
}

pub fn permission_mismatch() -> ApiError {
    ApiError::new(403, "AuthorizationPermissionMismatch", "This request is not authorized to perform this operation using this permission.")
        .credential()
}

pub fn resource_type_mismatch() -> ApiError {
    ApiError::new(
        403,
        "AuthorizationResourceTypeMismatch",
        "This request is not authorized to perform this operation using this resource type.",
    )
    .credential()
}

pub fn service_mismatch() -> ApiError {
    ApiError::new(403, "AuthorizationServiceMismatch", "This request is not authorized to perform this operation using this service.")
        .credential()
}

pub fn protocol_mismatch() -> ApiError {
    ApiError::new(403, "AuthorizationProtocolMismatch", "This request is not authorized to perform this operation using this protocol.")
        .credential()
}

/// 400: the `Authorization` header cannot be read.
pub fn invalid_authentication_info() -> ApiError {
    ApiError::new(
        400,
        "InvalidAuthenticationInfo",
        "Authentication information is not given in the correct format. Check the value of Authorization header.",
    )
    .credential()
}

/// 400: an `x-ms-version` Azure does not know. The only error of the Table service that has no code header.
pub fn invalid_version() -> ApiError {
    let mut e = ApiError::new(400, "InvalidHeaderValue", "The value for one of the HTTP headers is not in the correct format.");
    e.form = Form::Version;
    e
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

/// The indented XML document of an unknown `x-ms-version`.
fn version_body(e: &ApiError, ctx: &RequestCtx) -> String {
    format!(
        "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\r\n<error xmlns=\"http://schemas.microsoft.com/ado/2007/08/dataservices/metadata\">\r\n  <code>{}</code>\r\n  <message xml:lang=\"en-US\">{}</message>\r\n</error>",
        crate::xml::escape(&e.code),
        crate::xml::escape(&detail(e, ctx)),
    )
}

/// Render an error as a full HTTP response.
pub fn render(e: &ApiError, ctx: &RequestCtx, xml: bool) -> Response {
    let mut r = Resp::new(e.status);
    if e.form != Form::Version {
        r = r.header("x-ms-error-code", &e.code);
    }
    r = match (e.form, xml) {
        (Form::Version, _) => r.xml(version_body(e, ctx)),
        (_, true) => r.xml(xml_body(e, ctx)),
        (Form::Credential, false) => r.header("Content-Type", "application/json").body(json_body(e, ctx)),
        (Form::Table, false) => {
            r.header("Content-Type", crate::odata::CT_MINIMAL).header("X-Content-Type-Options", "nosniff").body(json_body(e, ctx))
        }
    };
    if let Some(c) = &e.challenge {
        r = r.header("WWW-Authenticate", c);
    }
    let resp = r.build();
    match (e.status, &e.unsupported) {
        (501, Some(op)) => azemu_core::unsupported::mark(resp, op.clone()),
        _ => resp,
    }
}
