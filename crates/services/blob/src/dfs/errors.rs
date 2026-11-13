//! Data Lake errors: the same `ApiError` the blob handlers use, written as the JSON the
//! `dfs` endpoint sends, with the Data Lake names for the codes that differ.

use azemu_core::resp::Resp;
use azemu_core::serde_json::{json, Value};
use azemu_core::time::iso8601_7;
use azemu_core::{RequestCtx, Response};

use crate::errors::ApiError;

/// Blob code -> (Data Lake code, status, message).
const RENAMED: &[(&str, &str, u16, &str)] = &[
    ("BlobNotFound", "PathNotFound", 404, "The specified path does not exist."),
    ("ContainerNotFound", "FilesystemNotFound", 404, "The specified filesystem does not exist."),
    ("BlobAlreadyExists", "PathAlreadyExists", 409, "The specified path already exists."),
    ("ContainerAlreadyExists", "FilesystemAlreadyExists", 409, "The specified filesystem already exists."),
];

/// Codes that only exist on the `dfs` endpoint.
pub fn dfs_error(code: &str) -> ApiError {
    let (status, msg) = match code {
        "PathNotFound" => (404, "The specified path does not exist."),
        "FilesystemNotFound" => (404, "The specified filesystem does not exist."),
        "PathAlreadyExists" => (409, "The specified path already exists."),
        "FilesystemAlreadyExists" => (409, "The specified filesystem already exists."),
        "PathConflict" => (409, "The specified path, or an element of the path, exists and its resource type is invalid for this operation."),
        "DirectoryNotEmpty" => (409, "The recursive query parameter value must be true to delete a non-empty directory."),
        "InvalidFlushPosition" => (400, "The uploaded data is not contiguous or the position query parameter value is not equal to the length of the file after appending the uploaded data."),
        "InvalidSourceOrDestinationResourceType" => (409, "The source and destination resource types of a rename must be the same."),
        "InvalidRenameSourcePath" => (409, "The source path of a rename cannot be the destination or a parent of it."),
        "InvalidSourceUri" => (400, "The source URI is invalid."),
        "SourcePathNotFound" => (404, "The source path for a rename operation does not exist."),
        "RenameDestinationParentPathNotFound" => (404, "The parent path of the rename destination does not exist."),
        _ => (400, "The request is invalid."),
    };
    ApiError::new(code).status(status).msg(msg)
}

/// The error as the `dfs` endpoint sends it.
pub fn into_response(mut e: ApiError, ctx: &RequestCtx) -> Response {
    for (blob, dfs, status, msg) in RENAMED {
        if e.code == *blob {
            e.code = (*dfs).to_string();
            e.status = *status;
            e.message = (*msg).to_string();
        }
    }
    if e.status == 304 {
        return e.into_response(ctx);
    }
    let message = format!("{}\nRequestId:{}\nTime:{}", e.message, ctx.request_id, iso8601_7(ctx.time));
    let mut err = json!({ "code": e.code, "message": message });
    if let Some((_, v)) = e.details.iter().find(|(k, _)| k == "AuthenticationErrorDetail") {
        err["detail"] = json!({ "AuthenticationErrorDetail": v });
    }
    let body: Value = json!({ "error": err });
    let mut r = Resp::new(e.status).json(&body).header("x-ms-error-code", &e.code);
    for (k, v) in &e.headers {
        r = r.header(k, v);
    }
    let resp = r.build();
    match (e.status, e.unsupported) {
        (501, Some(op)) => azemu_core::unsupported::mark(resp, op.to_string()),
        _ => resp,
    }
}

/// `op` is a short description of the request (`GET ?resource=...`); the name shown by `/_azemu/unsupported` is
/// `DataLake_` and the operation of the request (see `router::unknown`).
pub fn not_implemented(op: &str) -> ApiError {
    ApiError::new("NotImplemented").msg(format!("dfs: {op} is not implemented by this emulator.")).unsupported(format!("DataLake_{op}"))
}

/// A bad `x-ms-acl` (not parseable, or more than 32 access or 32 default entries): `400 InvalidHeaderValue`, naming the header.
/// The value is cut to 200 characters, so a huge header is not sent back.
pub fn acl_error(value: &str, why: String) -> ApiError {
    let shown: String = value.chars().take(200).collect();
    ApiError::invalid_header("x-ms-acl", &shown).msg(why)
}
