//! Credential check, `api-version` check and the headers every response carries.
//!
//! Auth is loose: any `HMAC-SHA256 ...` or `Bearer ...` header is accepted unchecked.

use azemu_core::http::HeaderValue;
use azemu_core::time;
use azemu_core::{Request, Response};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;

use crate::errors;

/// First api-version with the `description` field and `x-ms-item-count` header
/// (the recordings show them from 2026-04-01 on, not in 2023-11-01).
const LAST_OLD_VERSION: &str = "2024-09-01";

/// Does this api-version add `description` to items and `x-ms-item-count` to lists?
pub fn is_newer(version: &str) -> bool {
    version.starts_with("20") && version > LAST_OLD_VERSION
}

/// The versions the service takes (tried in November 2026). Any other value that looks like a version is "Unsupported API
/// version" (`2022-11-01`, `2025-01-01`, `2.0`, `7.6`); text that is none is "Invalid API version". No version at all is taken.
const API_VERSIONS: [&str; 6] = ["1.0", "2022-11-01-preview", "2023-10-01", "2023-11-01", "2024-09-01", "2026-04-01"];

/// The versions whose list answers have no `etag` in the body (and what a request without a version gets).
pub fn lists_have_body_etag(version: &str) -> bool {
    !matches!(version, "" | "1.0" | "2022-11-01-preview")
}

fn has_credentials(req: &Request) -> bool {
    let Some(h) = req.header("authorization") else {
        return false;
    };
    let (scheme, rest) = h.trim().split_once(' ').unwrap_or((h.trim(), ""));
    let scheme = scheme.to_ascii_lowercase();
    (scheme == "hmac-sha256" || scheme == "bearer") && !rest.trim().is_empty()
}

/// Checks before routing. `Some(response)` ends the request.
pub fn precheck(req: &Request) -> Option<Response> {
    if req.method == azemu_core::http::Method::OPTIONS {
        return None;
    }
    if !has_credentials(req) {
        return Some(errors::unauthorized());
    }
    match req.query.get("api-version") {
        None | Some("") => None,
        Some(v) if API_VERSIONS.contains(&v) => None,
        Some(v) => {
            let uri = format!("{}{}", req.ctx.endpoint.base_url(), req.path);
            let looks_like_one = v.bytes().next().map(|b| b.is_ascii_digit()).unwrap_or(false);
            Some(errors::unsupported_api_version(&uri, v, looks_like_one))
        }
    }
}

/// The api-version of a request that passed `precheck`.
pub fn version(req: &Request) -> &str {
    req.query.get("api-version").unwrap_or("")
}

fn hv(v: &str) -> HeaderValue {
    HeaderValue::from_str(v).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// Add the headers real App Configuration sends on every response (never overwriting).
pub fn finish(mut resp: Response, req: &Request, sn: u64) -> Response {
    let token = STANDARD.encode(format!("14:19#{sn}"));
    let date = time::rfc1123(req.ctx.time);
    let h = resp.headers_mut();
    let mut set = |k: &'static str, v: &str| {
        if !h.contains_key(k) {
            h.insert(k, hv(v));
        }
    };
    set("access-control-allow-origin", "*");
    set("strict-transport-security", "max-age=31536000; includeSubDomains");
    set("sync-token", &format!("zAJw6V16={token};sn={sn}"));
    set("x-ms-request-id", &req.ctx.request_id);
    set("x-ms-correlation-request-id", &uuid::Uuid::new_v4().to_string());
    set("x-ms-date", &date);
    if let Some(id) = req.header("x-ms-client-request-id") {
        set("x-ms-client-request-id", id);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_versions() {
        for v in ["1.0", "2022-11-01-preview", "2023-10-01", "2023-11-01", "2024-09-01", "2026-04-01"] {
            assert!(API_VERSIONS.contains(&v), "{v}");
        }
        for v in ["", "2.0", "latest", "2023-1-01", "2020-01-01", "2022-11-01", "2025-01-01", "2026-04-01-preview", "7.6"] {
            assert!(!API_VERSIONS.contains(&v), "{v}");
        }
    }

    #[test]
    fn newer_versions_have_description() {
        assert!(!is_newer("1.0"));
        assert!(!is_newer("2023-11-01"));
        assert!(!is_newer("2024-09-01"));
        assert!(is_newer("2026-04-01"));
    }
}
