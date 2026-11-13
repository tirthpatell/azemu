//! Blob Batch: a `multipart/mixed` body of Delete Blob / Set Blob Tier sub-requests,
//! each run as if it had arrived on its own.

use azemu_core::bytes::Bytes;
use azemu_core::http::{HeaderMap, HeaderName, HeaderValue, Method};
use azemu_core::resp::Resp;
use azemu_core::{Query, Request, Response};

use crate::cx::Cx;
use crate::errors::{err, ApiError, ApiResult};
use crate::pre::Pre;
use crate::router;

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() || from > hay.len() - needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn boundary(req: &Request) -> Option<String> {
    let ct = req.header("content-type")?;
    let lower = ct.to_ascii_lowercase();
    if !lower.starts_with("multipart/mixed") {
        return None;
    }
    let i = lower.find("boundary=")?;
    Some(ct[i + 9..].split(';').next()?.trim().trim_matches('"').to_string())
}

/// The sub-request parts of the body.
fn parts<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let delim = format!("--{boundary}");
    let d = delim.as_bytes();
    let mut out = vec![];
    let mut pos = match find(body, d, 0) {
        Some(p) => p,
        None => return out,
    };
    loop {
        let start = pos + d.len();
        if body[start..].starts_with(b"--") {
            break;
        }
        let Some(next) = find(body, d, start) else { break };
        let mut part = &body[start..next];
        part = part.strip_prefix(b"\r\n").unwrap_or(part);
        part = part.strip_suffix(b"\r\n").unwrap_or(part);
        out.push(part);
        pos = next;
    }
    out
}

struct Sub {
    content_id: String,
    req: Option<Request>,
}

fn parse_sub(part: &[u8], outer: &Request) -> Sub {
    let (head, inner) = match find(part, b"\r\n\r\n", 0) {
        Some(i) => (&part[..i], &part[i + 4..]),
        None => (part, &part[part.len()..]),
    };
    let head = String::from_utf8_lossy(head);
    let content_id = head
        .lines()
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-id")).map(|(_, v)| v.trim().to_string()))
        .unwrap_or_default();
    let (rhead, rbody) = match find(inner, b"\r\n\r\n", 0) {
        Some(i) => (&inner[..i], &inner[i + 4..]),
        None => (inner, &inner[inner.len()..]),
    };
    let rhead = String::from_utf8_lossy(rhead);
    let mut lines = rhead.lines();
    let first = lines.next().unwrap_or("");
    let mut it = first.split_whitespace();
    let (Some(method), Some(target)) = (it.next(), it.next()) else { return Sub { content_id, req: None } };
    let Ok(method) = Method::from_bytes(method.as_bytes()) else { return Sub { content_id, req: None } };
    // The target may be a full URL or a path.
    let target = match target.split_once("://") {
        Some((_, rest)) => rest.find('/').map(|i| &rest[i..]).unwrap_or("/"),
        None => target,
    };
    let (mut path, query) = target.split_once('?').unwrap_or((target, ""));
    if outer.ctx.endpoint.path_style {
        // `/{account}/container/blob` -> `/container/blob`
        let rest = path.trim_start_matches('/');
        path = rest.find('/').map(|i| &rest[i..]).unwrap_or("/");
    }
    let mut headers = HeaderMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            if let (Ok(n), Ok(val)) = (HeaderName::from_bytes(k.trim().as_bytes()), HeaderValue::from_str(v.trim())) {
                headers.append(n, val);
            }
        }
    }
    let mut ctx = outer.ctx.clone();
    ctx.request_id = uuid::Uuid::new_v4().to_string();
    let req = Request {
        method,
        path: path.to_string(),
        query: Query::parse(query),
        headers,
        body: Bytes::copy_from_slice(rbody),
        account: outer.account.clone(),
        ctx,
    };
    Sub { content_id, req: Some(req) }
}

fn render(resp: &Response) -> Vec<u8> {
    let status = resp.status();
    let mut out = format!("HTTP/1.1 {} {}\r\n", status.as_u16(), status.canonical_reason().unwrap_or("")).into_bytes();
    for (k, v) in resp.headers() {
        out.extend_from_slice(k.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !resp.body().is_empty() && !resp.headers().contains_key("content-length") {
        out.extend_from_slice(format!("Content-Length: {}\r\n", resp.body().len()).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(resp.body());
    out
}

/// Which sub-requests a batch may carry.
fn allowed(req: &Request, container: Option<&str>) -> bool {
    let comp = req.query.get("comp").map(|s| s.to_ascii_lowercase());
    let segs = req.segments();
    let depth_ok = segs.len() >= 2 || (segs.len() == 1 && req.query.get("restype").is_none());
    if !depth_ok {
        return false;
    }
    if let Some(c) = container {
        if segs.first().map(|s| s.as_str()) != Some(c) {
            return false;
        }
    }
    matches!((req.method.as_str(), comp.as_deref()), ("DELETE", None) | ("PUT", Some("tier")))
}

pub fn run_batch(cx: &mut Cx, container: Option<&str>) -> ApiResult {
    let req = cx.req;
    let boundary = boundary(req).ok_or_else(|| ApiError::invalid_header("Content-Type", req.header("content-type").unwrap_or("")))?;
    if let Some(c) = container {
        cx.container(c)?;
    }
    let raw = parts(&req.body, &boundary);
    if raw.is_empty() {
        return Err(err("InvalidInput").msg("The batch request body contains no sub-requests."));
    }
    if raw.len() > 256 {
        return Err(err("InvalidInput").msg("The batch request contains more than 256 sub-requests."));
    }
    let rboundary = format!("batchresponse_{}", uuid::Uuid::new_v4());
    let mut body: Vec<u8> = vec![];
    for part in raw {
        let sub = parse_sub(part, req);
        let resp = match &sub.req {
            Some(r) if allowed(r, container) => router::run(cx.st, r, None, &Pre::default()),
            Some(r) => {
                let e = err("InvalidInput").msg("This operation is not supported in a batch request.");
                azemu_core::storage::finish(e.into_response(&r.ctx), r, router::SERVER)
            }
            None => {
                let e = err("InvalidInput");
                azemu_core::storage::finish(e.into_response(&req.ctx), req, router::SERVER)
            }
        };
        body.extend_from_slice(
            format!("--{rboundary}\r\nContent-Type: application/http\r\nContent-ID: {}\r\n\r\n", sub.content_id).as_bytes(),
        );
        body.extend_from_slice(&render(&resp));
        // Azure writes the line break that ends a part only when the part has a body. Without a body
        // the blank line after the headers is that line break, so the delimiter follows it directly
        // (the Java batch client reads any text after the blank line as an error body).
        if !resp.body().is_empty() {
            body.extend_from_slice(b"\r\n");
        }
    }
    // no line break after the closing delimiter, as in the recorded answers
    body.extend_from_slice(format!("--{rboundary}--").as_bytes());
    Ok(Resp::new(202u16).header("Content-Type", format!("multipart/mixed; boundary={rboundary}")).body(Bytes::from(body)).build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_parts() {
        let body = b"--b\r\nContent-ID: 0\r\n\r\nDELETE /c/x HTTP/1.1\r\n\r\n\r\n--b\r\nContent-ID: 1\r\n\r\nDELETE /c/y HTTP/1.1\r\n\r\n\r\n--b--\r\n";
        let p = parts(body, "b");
        assert_eq!(p.len(), 2);
        assert!(String::from_utf8_lossy(p[1]).contains("/c/y"));
    }
}
