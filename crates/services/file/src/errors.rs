//! Error responses, with the codes and messages Azure Files sends (checked against Azure in November 2026).

use azemu_core::storage;
use azemu_core::{Request, RequestCtx, Response};

use crate::xml::Pos;

pub fn err(ctx: &RequestCtx, status: u16, code: &str, message: &str) -> Response {
    storage::error(ctx, status, code, message)
}

fn err_with(ctx: &RequestCtx, status: u16, code: &str, message: &str, d: &[(&str, &str)]) -> Response {
    storage::error_with_details(ctx, status, code, message, d)
}

pub fn share_not_found(ctx: &RequestCtx) -> Response {
    err(ctx, 404, "ShareNotFound", "The specified share does not exist.")
}

pub fn share_already_exists(ctx: &RequestCtx) -> Response {
    err(ctx, 409, "ShareAlreadyExists", "The specified share already exists.")
}

pub fn share_has_snapshots(ctx: &RequestCtx) -> Response {
    err(ctx, 409, "ShareHasSnapshots", "The share has snapshots and the operation requires no snapshots.")
}

pub fn resource_not_found(ctx: &RequestCtx) -> Response {
    err(ctx, 404, "ResourceNotFound", "The specified resource does not exist.")
}

pub fn parent_not_found(ctx: &RequestCtx) -> Response {
    err(ctx, 404, "ParentNotFound", "The specified parent path does not exist.")
}

pub fn resource_already_exists(ctx: &RequestCtx) -> Response {
    err(ctx, 409, "ResourceAlreadyExists", "The specified resource already exists.")
}

pub fn resource_type_mismatch(ctx: &RequestCtx) -> Response {
    err(ctx, 409, "ResourceTypeMismatch", "The specified resource type does not match the type of the existing resource.")
}

pub fn directory_not_empty(ctx: &RequestCtx) -> Response {
    err(ctx, 409, "DirectoryNotEmpty", "The specified directory is not empty.")
}

pub fn invalid_resource_name(ctx: &RequestCtx) -> Response {
    // Azure spells it "specifed".
    err(ctx, 400, "InvalidResourceName", "The specifed resource name contains invalid characters.")
}

pub fn name_out_of_range(ctx: &RequestCtx) -> Response {
    err(ctx, 400, "OutOfRangeInput", "The specified resource name length is not within the permissible limits.")
}

pub fn out_of_range_input(ctx: &RequestCtx, message: &str) -> Response {
    err(ctx, 400, "OutOfRangeInput", message)
}

pub fn invalid_header(ctx: &RequestCtx, name: &str, value: &str) -> Response {
    err_with(
        ctx,
        400,
        "InvalidHeaderValue",
        "The value for one of the HTTP headers is not in the correct format.",
        &[("HeaderName", name), ("HeaderValue", value)],
    )
}

pub fn missing_header(ctx: &RequestCtx, name: &str) -> Response {
    // `name` is empty when Azure cannot say which header is missing (a lease request with no action).
    err_with(
        ctx,
        400,
        "MissingRequiredHeader",
        "An HTTP header that's mandatory for this request is not specified.",
        &[("HeaderName", name)],
    )
}

pub fn invalid_query(ctx: &RequestCtx, name: &str, value: &str, reason: &str) -> Response {
    let mut d = vec![("QueryParameterName", name), ("QueryParameterValue", value)];
    if !reason.is_empty() {
        d.push(("Reason", reason));
    }
    err_with(ctx, 400, "InvalidQueryParameterValue", "Value for one of the query parameters specified in the request URI is invalid.", &d)
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

/// A marker that is not one this service handed out.
pub fn bad_marker(ctx: &RequestCtx) -> Response {
    err(ctx, 400, "OutOfRangeInput", "One of the request inputs is out of range.")
}

pub fn invalid_range(ctx: &RequestCtx) -> Response {
    err(ctx, 416, "InvalidRange", "The range specified is invalid for the current size of the resource.")
}

/// 416 for a read past the end: Azure also sends `Content-Range: bytes */<size>`.
pub fn invalid_range_read(ctx: &RequestCtx, size: u64) -> Response {
    let mut r = invalid_range(ctx);
    if let Ok(v) = azemu_core::http::HeaderValue::from_str(&format!("bytes */{size}")) {
        r.headers_mut().insert("content-range", v);
    }
    r
}

pub fn md5_mismatch(ctx: &RequestCtx, user: &str, server: &str) -> Response {
    err_with(
        ctx,
        400,
        "Md5Mismatch",
        "The MD5 value specified in the request did not match with the MD5 value calculated by the server.",
        &[("UserSpecifiedMd5", user), ("ServerCalculatedMd5", server)],
    )
}

pub fn unsupported_verb(ctx: &RequestCtx) -> Response {
    err(ctx, 405, "UnsupportedHttpVerb", "The resource doesn't support specified Http Verb.")
}

pub fn conflict(ctx: &RequestCtx, code: &str, message: &str) -> Response {
    err(ctx, 409, code, message)
}

pub fn precondition(ctx: &RequestCtx, code: &str, message: &str) -> Response {
    err(ctx, 412, code, message)
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

pub fn invalid_xml_no_pos(ctx: &RequestCtx, reason: &str) -> Response {
    invalid_xml(ctx, Pos { line: 0, col: 0 }, reason)
}

/// An operation that is not built: 501 that names it (and `GET /_azemu/unsupported` counts it as `File_<name>`).
pub fn not_implemented(ctx: &RequestCtx, op: &str) -> Response {
    let resp = err(ctx, 501, "NotImplemented", &format!("file: {op} is not implemented by this emulator."));
    azemu_core::unsupported::mark(resp, format!("File_{}", op.replace(' ', "")))
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

/// What a request with no credentials is told (see `azemu_core::storage::credentials::without_credentials`). Azure Files answers
/// as Blob does: the challenge from `x-ms-version` 2019-12-12 on, 404 before it (checked in November 2026).
pub fn no_credentials(req: &Request) -> Response {
    use storage::credentials::{without_credentials, Flavor, NoCredentials, CHALLENGE};
    match without_credentials(req, Flavor::Blob) {
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
    invalid_header(ctx, "x-ms-version", value)
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

pub fn resource_type_mismatch_auth(ctx: &RequestCtx) -> Response {
    err(ctx, 403, "AuthorizationResourceTypeMismatch", "This request is not authorized to perform this operation using this resource type.")
}

pub fn protocol_mismatch(ctx: &RequestCtx) -> Response {
    err(ctx, 403, "AuthorizationProtocolMismatch", "This request is not authorized to perform this operation using this protocol.")
}

/// 400 for a JSON body that is not what the operation takes.
pub fn err_json_document(ctx: &RequestCtx) -> Response {
    err_with(
        ctx,
        400,
        "InvalidJsonDocument",
        "JSON specified is not syntactically valid.",
        &[("LineNumber", "1"), ("LinePosition", "2"), ("Reason", "Invalid file permission in the request body. Failed to deserialize.")],
    )
}
