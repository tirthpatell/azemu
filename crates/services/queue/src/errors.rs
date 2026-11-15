//! Error responses, with the codes and messages Azure Queue Storage sends.

use azemu_core::storage;
use azemu_core::{RequestCtx, Response};

use crate::xml::Pos;

pub fn err(ctx: &RequestCtx, status: u16, code: &str, message: &str) -> Response {
    storage::error(ctx, status, code, message)
}

fn err_with(ctx: &RequestCtx, status: u16, code: &str, message: &str, d: &[(&str, &str)]) -> Response {
    storage::error_with_details(ctx, status, code, message, d)
}

pub fn queue_not_found(ctx: &RequestCtx) -> Response {
    err(ctx, 404, "QueueNotFound", "The specified queue does not exist.")
}

pub fn queue_already_exists(ctx: &RequestCtx) -> Response {
    err(ctx, 409, "QueueAlreadyExists", "The specified queue already exists.")
}

pub fn message_not_found(ctx: &RequestCtx) -> Response {
    err(ctx, 404, "MessageNotFound", "The specified message does not exist.")
}

pub fn pop_receipt_mismatch(ctx: &RequestCtx) -> Response {
    err(ctx, 400, "PopReceiptMismatch", "The specified pop receipt did not match the pop receipt for a dequeued message.")
}

// Azure spells it "specifed".
pub fn invalid_resource_name(ctx: &RequestCtx) -> Response {
    err(ctx, 400, "InvalidResourceName", "The specifed resource name contains invalid characters.")
}

pub fn name_out_of_range(ctx: &RequestCtx) -> Response {
    err(ctx, 400, "OutOfRangeInput", "The specified resource name length is not within the permissible limits.")
}

pub fn invalid_metadata(ctx: &RequestCtx) -> Response {
    err(ctx, 400, "InvalidMetadata", "The metadata specified is invalid. It has characters that are not permitted.")
}

pub fn metadata_too_large(ctx: &RequestCtx) -> Response {
    err(ctx, 400, "MetadataTooLarge", "The metadata specified exceeds the maximum permissible limit.")
}

/// `InvalidXmlDocument` with the position and reason Azure adds to the body.
pub fn invalid_xml(ctx: &RequestCtx, pos: Pos, reason: &str) -> Response {
    err_with(
        ctx,
        400,
        "InvalidXmlDocument",
        "XML specified is not syntactically valid.",
        &[("LineNumber", &pos.line.to_string()), ("LinePosition", &pos.col.to_string()), ("Reason", reason)],
    )
}

/// Same, for failures Azure reports without a position (line 0, position 0).
pub fn invalid_xml_no_pos(ctx: &RequestCtx, reason: &str) -> Response {
    invalid_xml(ctx, Pos { line: 0, col: 0 }, reason)
}

pub fn body_too_large(ctx: &RequestCtx, max: usize) -> Response {
    err_with(
        ctx,
        413,
        "RequestBodyTooLarge",
        "The request body is too large and exceeds the maximum permissible limit.",
        &[("MaxLimit", &max.to_string())],
    )
}

pub fn out_of_range_query(ctx: &RequestCtx, name: &str, value: &str, min: i64, max: i64) -> Response {
    err_with(
        ctx,
        400,
        "OutOfRangeQueryParameterValue",
        "One of the query parameters specified in the request URI is outside the permissible range.",
        &[
            ("QueryParameterName", name),
            ("QueryParameterValue", value),
            ("MinimumAllowed", &min.to_string()),
            ("MaximumAllowed", &max.to_string()),
        ],
    )
}

pub fn invalid_query(ctx: &RequestCtx, name: &str, value: &str, reason: &str) -> Response {
    let mut d = vec![("QueryParameterName", name), ("QueryParameterValue", value)];
    if !reason.is_empty() {
        d.push(("Reason", reason));
    }
    err_with(ctx, 400, "InvalidQueryParameterValue", "Value for one of the query parameters specified in the request URI is invalid.", &d)
}

pub fn missing_query(ctx: &RequestCtx, name: &str) -> Response {
    err_with(
        ctx,
        400,
        "MissingRequiredQueryParameter",
        "A query parameter that's mandatory for this request is not specified.",
        &[("QueryParameterName", name)],
    )
}

pub fn invalid_uri(ctx: &RequestCtx) -> Response {
    err(ctx, 400, "InvalidUri", "The requested URI does not represent any resource on the server.")
}

pub fn unsupported_verb(ctx: &RequestCtx) -> Response {
    err(ctx, 405, "UnsupportedHttpVerb", "The resource doesn't support specified Http Verb.")
}

pub fn authentication_failed(ctx: &RequestCtx, detail: Option<&str>) -> Response {
    let msg = "Server failed to authenticate the request. Make sure the value of Authorization header is formed correctly including the signature.";
    match detail {
        Some(d) => err_with(ctx, 403, "AuthenticationFailed", msg, &[("AuthenticationErrorDetail", d)]),
        None => err(ctx, 403, "AuthenticationFailed", msg),
    }
}

/// 400: the `Authorization` header cannot be read.
pub fn invalid_authentication_info(ctx: &RequestCtx) -> Response {
    err(
        ctx,
        400,
        "InvalidAuthenticationInfo",
        "Authentication information is not given in the correct format. Check the value of Authorization header.",
    )
}

/// What a request with no credentials is told (see `azemu_core::storage::credentials::without_credentials`).
pub fn no_credentials(req: &azemu_core::Request) -> Response {
    use storage::credentials::{without_credentials, Flavor, NoCredentials, CHALLENGE};
    match without_credentials(req, Flavor::Queue) {
        NoCredentials::NotFound => err(&req.ctx, 404, "ResourceNotFound", "The specified resource does not exist."),
        NoCredentials::Challenge => {
            let mut resp = err(
                &req.ctx,
                401,
                "NoAuthenticationInformation",
                "Server failed to authenticate the request. Please refer to the information in the www-authenticate header.",
            );
            if let Ok(v) = azemu_core::http::HeaderValue::from_str(CHALLENGE) {
                resp.headers_mut().insert("www-authenticate", v);
            }
            resp
        }
    }
}

/// 400 for an `x-ms-version` Azure does not know.
pub fn invalid_version(ctx: &RequestCtx, value: &str) -> Response {
    err_with(
        ctx,
        400,
        "InvalidHeaderValue",
        "The value for one of the HTTP headers is not in the correct format.",
        &[("HeaderName", "x-ms-version"), ("HeaderValue", value)],
    )
}

pub fn authorization_failure(ctx: &RequestCtx) -> Response {
    err(ctx, 403, "AuthorizationFailure", "This request is not authorized to perform this operation.")
}

pub fn permission_mismatch(ctx: &RequestCtx) -> Response {
    err(ctx, 403, "AuthorizationPermissionMismatch", "This request is not authorized to perform this operation using this permission.")
}

pub fn service_mismatch(ctx: &RequestCtx) -> Response {
    err(ctx, 403, "AuthorizationServiceMismatch", "This request is not authorized to perform this operation using this service.")
}

pub fn resource_type_mismatch(ctx: &RequestCtx) -> Response {
    err(ctx, 403, "AuthorizationResourceTypeMismatch", "This request is not authorized to perform this operation using this resource type.")
}

pub fn protocol_mismatch(ctx: &RequestCtx) -> Response {
    err(ctx, 403, "AuthorizationProtocolMismatch", "This request is not authorized to perform this operation using this protocol.")
}
