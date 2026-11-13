//! Credentials and the headers every Event Grid answer carries.
//!
//! The emulator checks only that a credential is present: `aeg-sas-key`, `aeg-sas-token`
//! or an `Authorization` header (`SharedAccessKey …`, `SharedAccessSignature …`,
//! `Bearer …`). Its value is not verified.

use azemu_core::http::HeaderValue;
use azemu_core::{Request, Response};

use crate::errors;

pub const CLASSIC_VERSION: &str = "2018-01-01";
pub const NAMESPACE_VERSION: &str = "2024-06-01";

/// `Err(401)` when the request has no credential at all.
pub fn check(req: &Request) -> Result<(), Response> {
    let present = |name: &str| req.header(name).map(|v| !v.trim().is_empty()).unwrap_or(false);
    if present("aeg-sas-key") || present("aeg-sas-token") || present("authorization") {
        return Ok(());
    }
    let host = req.ctx.endpoint.authority.split(':').next().unwrap_or("").to_string();
    Err(errors::unauthorized(
        &host,
        "The request did not contain an authorization key (aeg-sas-key, aeg-sas-token or Authorization header).",
    ))
}

/// Add the headers Event Grid sets on every answer, unless the handler already did.
pub fn finish(mut resp: Response, req: &Request) -> Response {
    if req.path.starts_with("/_azemu/") {
        return resp;
    }
    let h = resp.headers_mut();
    let mut put = |k: &'static str, v: &str| {
        if !h.contains_key(k) {
            if let Ok(v) = HeaderValue::from_str(v) {
                h.insert(k, v);
            }
        }
    };
    let version = if req.path.starts_with("/topics/") { NAMESPACE_VERSION } else { CLASSIC_VERSION };
    put("api-supported-versions", version);
    put("Server", "Microsoft-HTTPAPI/2.0");
    put("Strict-Transport-Security", "max-age=31536000; includeSubDomains");
    put("x-ms-request-id", &req.ctx.request_id);
    if let Some(c) = req.header("x-ms-client-request-id") {
        put("x-ms-client-request-id", c);
    }
    resp
}
