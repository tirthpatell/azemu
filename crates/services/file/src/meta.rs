//! User metadata (`x-ms-meta-*` headers) of shares, directories and files.

use azemu_core::resp::Resp;
use azemu_core::{Request, Response};

use crate::errors;
use crate::xml;

/// The most metadata one resource holds, names and values together.
const MAX_BYTES: usize = 8 * 1024;

/// The metadata of a request, sorted by name (header names reach the service in lower case, so the case a client used is lost). Names are C# identifiers; the same name twice (in any case) or a
/// total over 8 KiB is refused.
pub fn read(req: &Request) -> Result<Vec<(String, String)>, Response> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut size = 0usize;
    for (name, value) in req.headers.iter() {
        let Some(key) = name.as_str().strip_prefix("x-ms-meta-") else { continue };
        let Ok(value) = value.to_str() else {
            return Err(invalid(req));
        };
        let valid = key.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(invalid(req));
        }
        if out.iter().any(|(k, _)| k.eq_ignore_ascii_case(key)) {
            return Err(invalid(req));
        }
        size += key.len() + value.len();
        out.push((key.to_string(), value.trim().to_string()));
    }
    if size > MAX_BYTES {
        return Err(errors::err(&req.ctx, 400, "MetadataTooLarge", "The metadata specified exceeds the maximum permissible limit."));
    }
    out.sort();
    Ok(out)
}

fn invalid(req: &Request) -> Response {
    errors::err(&req.ctx, 400, "InvalidMetadata", "The metadata specified is invalid. It has characters that are not permitted.")
}

/// `x-ms-meta-name: value` headers for a response.
pub fn headers(mut r: Resp, meta: &[(String, String)]) -> Resp {
    for (k, v) in meta {
        r = r.header(format!("x-ms-meta-{k}"), v);
    }
    r
}

/// `<Metadata>...</Metadata>` of a list entry.
pub fn element(meta: &[(String, String)]) -> String {
    if meta.is_empty() {
        return "<Metadata />".to_string();
    }
    let mut s = String::from("<Metadata>");
    for (k, v) in meta {
        s.push_str(&xml::element(k, v));
    }
    s.push_str("</Metadata>");
    s
}
