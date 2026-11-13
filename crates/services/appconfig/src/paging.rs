//! Page cursors, `Link` headers and the list response.

use azemu_core::conditions::{Access, Conditions, Outcome, Resource};
use azemu_core::resp::Resp;
use azemu_core::time::OffsetDateTime;
use azemu_core::{Request, Response};
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use base64::Engine;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::{json, Map, Value};

use crate::errors;
use crate::state::token;

/// Items per page, as real App Configuration.
pub const PAGE_SIZE: usize = 100;

/// What stays raw in a `Link` URL: the recordings show `*`, `$` and `,` unencoded, `=` encoded.
const LINK_SET: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~').remove(b'*').remove(b'$').remove(b',');

pub fn encode_component(s: &str) -> String {
    utf8_percent_encode(s, LINK_SET).to_string()
}

pub fn encode_cursor(s: &str) -> String {
    STANDARD.encode(s.as_bytes())
}

/// A cursor we issued. Anything else gives `None` and the listing starts from the top.
pub fn decode_cursor(s: &str) -> Option<String> {
    let t = s.trim_end_matches('=');
    let bytes = STANDARD_NO_PAD.decode(t).ok().or_else(|| URL_SAFE_NO_PAD.decode(t).ok())?;
    String::from_utf8(bytes).ok()
}

/// `/kv?<the request's query without after>&after=<cursor>`
pub fn link_target(req: &Request, path: &str, after: &str) -> String {
    let mut parts: Vec<String> = req
        .query
        .pairs()
        .iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("after"))
        .map(|(k, v)| format!("{}={}", encode_component(k), encode_component(v)))
        .collect();
    parts.push(format!("after={}", encode_component(after)));
    format!("{path}?{}", parts.join("&"))
}

/// The etag of a page: a hash of the etags of its items.
pub fn page_etag<'a>(item_etags: impl Iterator<Item = &'a str>) -> String {
    let joined: Vec<&str> = item_etags.collect();
    token(&joined.join("\n"))
}

/// One page of a list, ready to be sent.
pub struct Page {
    pub items: Vec<Value>,
    /// Page etag; lists without one (`/labels`, `/keys`, `/snapshots`) leave it out.
    pub etag: Option<String>,
    /// Link to the next page (path and query).
    pub next: Option<String>,
    pub content_type: &'static str,
    pub status: u16,
    pub extra_headers: Vec<(&'static str, String)>,
}

impl Page {
    pub fn new(content_type: &'static str, items: Vec<Value>) -> Page {
        Page { items, etag: None, next: None, content_type, status: 200, extra_headers: Vec::new() }
    }
}

/// Answer a list request. With an etag, `If-None-Match` of the page's etag gives 304.
pub fn respond(req: &Request, page: Page, newer: bool) -> Response {
    let link = page.next.as_ref().map(|n| format!("<{n}>; rel=\"next\""));
    if let Some(etag) = &page.etag {
        let c = Conditions {
            // `If-Match` on a page is ignored: real Azure answers 200 to the setting's etag
            // (recordings `*_correct_etag`) and the recordings never show a 412 on a list.
            if_match: None,
            if_none_match: req.header("if-none-match"),
            if_modified_since: None,
            if_unmodified_since: None,
        };
        let res = Resource::Exists { etag, last_modified: req.ctx.time };
        match c.evaluate(res, Access::Read) {
            Outcome::Proceed => {}
            Outcome::NotModified => {
                let mut r = Resp::new(304u16);
                if let Some(l) = &link {
                    r = r.header("Link", l);
                }
                return r.build();
            }
            Outcome::PreconditionFailed => return errors::precondition_failed(),
        }
    }
    let count = page.items.len();
    let mut body = Map::new();
    if let Some(etag) = &page.etag {
        body.insert("etag".into(), json!(etag));
    }
    body.insert("items".into(), Value::Array(page.items));
    if let Some(n) = &page.next {
        body.insert("@nextLink".into(), json!(n));
    }
    let bytes = serde_json::to_vec(&Value::Object(body)).unwrap_or_default();
    let mut r = Resp::new(page.status).header("Content-Type", page.content_type).body(bytes);
    if let Some(etag) = &page.etag {
        r = r.header("ETag", format!("\"{etag}\""));
    }
    if let Some(l) = &link {
        r = r.header("Link", l);
    }
    if newer {
        r = r.header("x-ms-item-count", count);
    }
    for (k, v) in page.extra_headers {
        r = r.header(k, v);
    }
    r.build()
}

/// `Memento-Datetime` value for an `Accept-Datetime` read.
pub fn memento(t: OffsetDateTime) -> String {
    azemu_core::time::rfc1123(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip() {
        let c = encode_cursor("sample_key_188\nsample_label_188");
        assert_eq!(c, "c2FtcGxlX2tleV8xODgKc2FtcGxlX2xhYmVsXzE4OA==");
        assert_eq!(decode_cursor(&c).unwrap(), "sample_key_188\nsample_label_188");
        assert_eq!(decode_cursor("c2FtcGxlX2tleV8xODgKc2FtcGxlX2xhYmVsXzE4OA").unwrap(), "sample_key_188\nsample_label_188");
        assert!(decode_cursor("!!!").is_none());
    }

    #[test]
    fn link_encoding_matches_the_recordings() {
        assert_eq!(encode_component("sample_key_*"), "sample_key_*");
        assert_eq!(encode_component("$select"), "$select");
        assert_eq!(encode_component("key,content_type"), "key,content_type");
        assert_eq!(encode_component("tag1=value1"), "tag1%3Dvalue1");
        assert_eq!(encode_component("abc=="), "abc%3D%3D");
    }
}
