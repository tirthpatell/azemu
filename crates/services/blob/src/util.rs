//! Small helpers: name validation, ranges, hashes, metadata and tag parsing.

use std::collections::BTreeMap;

use azemu_core::bytes::Bytes;
use azemu_core::time;
use azemu_core::Request;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use md5::{Digest, Md5};
use percent_encoding::percent_decode_str;

use crate::errors::{err, ApiError};
use crate::pre::Pre;
use crate::state::Time;

pub fn md5(data: &[u8]) -> Vec<u8> {
    let mut h = Md5::new();
    h.update(data);
    h.finalize().to_vec()
}

pub fn b64(data: &[u8]) -> String {
    B64.encode(data)
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    B64.decode(s.trim()).ok()
}

/// CRC-64/NVME, the checksum behind `x-ms-content-crc64`.
pub fn crc64(data: &[u8]) -> u64 {
    static TABLE: std::sync::OnceLock<[u64; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let poly: u64 = 0x9A6C_9329_AC4B_C9B5;
        let mut t = [0u64; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u64;
            for _ in 0..8 {
                c = if c & 1 == 1 { (c >> 1) ^ poly } else { c >> 1 };
            }
            *slot = c;
        }
        t
    });
    let mut c = u64::MAX;
    for &b in data {
        c = t[((c ^ b as u64) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ u64::MAX
}

pub fn crc64_b64(v: u64) -> String {
    B64.encode(v.to_le_bytes())
}

/// Header value as `&str`.
pub fn hdr<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.header(name)
}

/// Header present and true (case-insensitive).
pub fn hdr_true(req: &Request, name: &str) -> bool {
    req.header(name).map(|v| v.eq_ignore_ascii_case("true")).unwrap_or(false)
}

pub fn hdr_u64(req: &Request, name: &str) -> Result<Option<u64>, ApiError> {
    match req.header(name) {
        None => Ok(None),
        Some(v) => v.trim().parse::<u64>().map(Some).map_err(|_| ApiError::invalid_header(name, v)),
    }
}

pub fn hdr_i64(req: &Request, name: &str) -> Result<Option<i64>, ApiError> {
    match req.header(name) {
        None => Ok(None),
        Some(v) => v.trim().parse::<i64>().map(Some).map_err(|_| ApiError::invalid_header(name, v)),
    }
}

/// Parse an RFC 1123 date header; Err when present but not a date.
pub fn hdr_date(req: &Request, name: &str) -> Result<Option<Time>, ApiError> {
    match req.header(name) {
        None => Ok(None),
        Some(v) => time::parse_rfc1123(v).map(Some).ok_or_else(|| ApiError::invalid_header(name, v)),
    }
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

pub fn validate_container_name(name: &str) -> Result<(), ApiError> {
    if name == "$root" || name == "$web" || name == "$logs" {
        return Ok(());
    }
    let ok_chars = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    if !ok_chars {
        return Err(err("InvalidResourceName"));
    }
    if name.len() < 3 || name.len() > 63 {
        return Err(err("OutOfRangeInput"));
    }
    Ok(())
}

/// The rules for a blob name in a container that is not hierarchical; `names.rs` has them for both endpoints.
pub fn validate_blob_name(name: &str) -> Result<(), ApiError> {
    crate::names::check(name, false)
}

// ---------------------------------------------------------------------------
// Ranges
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    /// `bytes=a-b` or `bytes=a-`
    From { start: u64, end: Option<u64> },
    /// `bytes=-n`
    Suffix(u64),
}

pub fn parse_range(v: &str) -> Option<RangeSpec> {
    let v = v.trim().strip_prefix("bytes=")?;
    let (a, b) = v.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        return b.parse().ok().map(RangeSpec::Suffix);
    }
    let start = a.parse().ok()?;
    let end = if b.is_empty() { None } else { Some(b.parse().ok()?) };
    if let Some(e) = end {
        if e < start {
            return None;
        }
    }
    Some(RangeSpec::From { start, end })
}

/// Resolve a range against a content length. `None` means not satisfiable.
pub fn resolve_range(spec: RangeSpec, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    match spec {
        RangeSpec::From { start, end } => {
            if start >= size {
                return None;
            }
            Some((start, end.unwrap_or(size - 1).min(size - 1)))
        }
        RangeSpec::Suffix(n) => {
            if n == 0 {
                return None;
            }
            Some((size.saturating_sub(n), size - 1))
        }
    }
}

/// The request's range: `x-ms-range` wins over `Range`.
pub fn request_range(req: &Request, header: &str) -> Result<Option<RangeSpec>, ApiError> {
    let v = req.header(header).or_else(|| if header == "x-ms-range" { req.header("range") } else { None });
    match v {
        None => Ok(None),
        Some(v) => parse_range(v).map(Some).ok_or_else(|| ApiError::invalid_header(header, v)),
    }
}

// ---------------------------------------------------------------------------
// Metadata and tags
// ---------------------------------------------------------------------------

pub fn is_valid_meta_name(n: &str) -> bool {
    let mut chars = n.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `x-ms-meta-*` headers, in a stable order.
pub fn parse_metadata(req: &Request) -> Result<Vec<(String, String)>, ApiError> {
    parse_metadata_prefix(req, "x-ms-meta-")
}

pub fn parse_metadata_prefix(req: &Request, prefix: &str) -> Result<Vec<(String, String)>, ApiError> {
    let mut out: Vec<(String, String)> = vec![];
    let mut total = 0usize;
    for (k, v) in req.headers.iter() {
        let name = k.as_str();
        if let Some(rest) = name.strip_prefix(prefix) {
            if rest.is_empty() {
                return Err(err("EmptyMetadataKey"));
            }
            if !is_valid_meta_name(rest) {
                return Err(err("InvalidMetadata"));
            }
            if out.iter().any(|(n, _)| n.eq_ignore_ascii_case(rest)) {
                return Err(err("InvalidMetadata"));
            }
            let val = v.to_str().map_err(|_| err("InvalidMetadata"))?.to_string();
            total += rest.len() + val.len();
            out.push((rest.to_string(), val));
        }
    }
    if total > 8 * 1024 {
        return Err(err("MetadataTooLarge"));
    }
    out.sort();
    Ok(out)
}

pub fn valid_tag_text(s: &str) -> bool {
    s.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '+' | '-' | '.' | '/' | ':' | '=' | '_'))
}

pub fn validate_tags(tags: &BTreeMap<String, String>) -> Result<(), ApiError> {
    if tags.len() > 10 {
        return Err(err("TagsTooLarge").msg("The tags specified exceed the maximum permissible limit of 10."));
    }
    for (k, v) in tags {
        if k.is_empty() || k.chars().count() > 128 || v.chars().count() > 256 || !valid_tag_text(k) || !valid_tag_text(v) {
            return Err(err("TagsTooLarge"));
        }
    }
    Ok(())
}

/// `x-ms-tags: k1=v1&k2=v2` (percent-encoded).
pub fn parse_tags_header(req: &Request) -> Result<BTreeMap<String, String>, ApiError> {
    let mut out = BTreeMap::new();
    if let Some(h) = req.header("x-ms-tags") {
        for pair in h.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let dec = |s: &str| percent_decode_str(&s.replace('+', "%2B")).decode_utf8_lossy().to_string();
            out.insert(dec(k), dec(v));
        }
        validate_tags(&out)?;
    }
    Ok(out)
}

/// Content-MD5 request header, decoded. Err when it is not a valid MD5.
pub fn request_md5(req: &Request, header: &str) -> Result<Option<Vec<u8>>, ApiError> {
    match req.header(header) {
        None => Ok(None),
        Some(v) => match b64_decode(v) {
            Some(d) if d.len() == 16 => Ok(Some(d)),
            _ => Err(err("InvalidMd5").detail("HeaderName", header).detail("HeaderValue", v)),
        },
    }
}

/// Check `Content-MD5` and `x-ms-content-crc64` against a body. Returns the computed
/// (md5, crc64).
pub fn check_body_hashes(req: &Request, body: &Bytes, pre: &Pre) -> Result<(Vec<u8>, u64), ApiError> {
    let want_md5 = request_md5(req, "content-md5")?;
    let want_crc = match req.header("x-ms-content-crc64") {
        None => None,
        Some(v) => match b64_decode(v) {
            Some(d) if d.len() == 8 => Some(u64::from_le_bytes(d.try_into().unwrap_or([0; 8]))),
            _ => return Err(err("InvalidHeaderValue").detail("HeaderName", "x-ms-content-crc64").detail("HeaderValue", v)),
        },
    };
    let got_md5 = pre.md5(body);
    let got_crc = pre.crc64(body);
    if let Some(w) = want_md5 {
        if w != got_md5 {
            return Err(err("Md5Mismatch").detail("UserSpecifiedMd5", b64(&w)).detail("ServerCalculatedMd5", b64(&got_md5)));
        }
    }
    if let Some(w) = want_crc {
        if w != got_crc {
            return Err(err("Crc64Mismatch")
                .status(400)
                .msg("The CRC64 value specified in the request did not match with the CRC64 value calculated by the server.")
                .detail("UserSpecifiedCrc", crc64_b64(w))
                .detail("ServerCalculatedCrc", crc64_b64(got_crc)));
        }
    }
    Ok((got_md5, got_crc))
}

/// Percent-encode a blob name for a URL path (keeps `/`).
pub fn encode_path(name: &str) -> String {
    const SET: &percent_encoding::AsciiSet =
        &percent_encoding::NON_ALPHANUMERIC.remove(b'/').remove(b'-').remove(b'_').remove(b'.').remove(b'~');
    percent_encoding::utf8_percent_encode(name, SET).to_string()
}

/// Strip the quotes of an ETag (list XML carries it unquoted).
pub fn unquote_etag(e: &str) -> &str {
    e.trim_matches('"')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc64_matches_azure() {
        assert_eq!(crc64_b64(crc64(b"abc")), "6/rBP7vK5QU=");
    }

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("bytes=2-5"), Some(RangeSpec::From { start: 2, end: Some(5) }));
        assert_eq!(parse_range("bytes=7-"), Some(RangeSpec::From { start: 7, end: None }));
        assert_eq!(resolve_range(RangeSpec::From { start: 10, end: None }, 10), None);
        assert_eq!(resolve_range(RangeSpec::From { start: 0, end: Some(99) }, 10), Some((0, 9)));
        assert_eq!(resolve_range(RangeSpec::From { start: 0, end: None }, 0), None);
    }

    #[test]
    fn container_names() {
        assert!(validate_container_name("abc-1").is_ok());
        assert_eq!(validate_container_name("ab").unwrap_err().code, "OutOfRangeInput");
        assert_eq!(validate_container_name("Abc").unwrap_err().code, "InvalidResourceName");
        assert_eq!(validate_container_name("a--b").unwrap_err().code, "InvalidResourceName");
        assert!(validate_container_name("$root").is_ok());
    }
}
