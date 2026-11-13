//! Small helpers for the `dfs` handlers.

use azemu_core::resp::Resp;
use azemu_core::serde_json::Value;
use azemu_core::Request;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use crate::dfs::model::FOLDER_KEY;
use crate::errors::ApiError;
use crate::state::Time;

pub fn json(status: u16, v: &Value) -> Resp {
    Resp::new(status).json(v)
}

/// `x-ms-properties: name=base64(value),name2=...` into metadata pairs (values as text).
pub fn parse_properties(req: &Request) -> Result<Option<Vec<(String, String)>>, ApiError> {
    let Some(h) = req.header("x-ms-properties") else { return Ok(None) };
    let mut out: Vec<(String, String)> = vec![];
    for pair in h.split(',').filter(|p| !p.trim().is_empty()) {
        let bad = || ApiError::invalid_header("x-ms-properties", h);
        let (k, v) = pair.split_once('=').ok_or_else(bad)?;
        let k = k.trim();
        if k.is_empty() {
            return Err(bad());
        }
        let raw = B64.decode(v.trim()).map_err(|_| bad())?;
        out.retain(|(n, _)| !n.eq_ignore_ascii_case(k));
        out.push((k.to_string(), String::from_utf8_lossy(&raw).to_string()));
    }
    out.sort();
    Ok(Some(out))
}

/// Metadata as the `x-ms-properties` header value (the folder marker is not a property).
pub fn format_properties(meta: &[(String, String)]) -> String {
    meta.iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case(FOLDER_KEY))
        .map(|(k, v)| format!("{k}={}", B64.encode(v.as_bytes())))
        .collect::<Vec<_>>()
        .join(",")
}

/// A time as `Wed, 17 Aug 2022 22:21:17 GMT`.
pub fn http_date(t: Time) -> String {
    azemu_core::time::rfc1123(t)
}

/// `etag` unquoted, the way listings carry it.
pub fn bare_etag(e: &str) -> &str {
    e.trim_matches('"')
}
