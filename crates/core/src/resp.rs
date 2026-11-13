//! Response builder.

use bytes::Bytes;
use http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use serde::Serialize;

use crate::Response;

/// Anything that can be used as a status code: `StatusCode` or a `u16`.
/// An invalid `u16` becomes 500.
pub trait IntoStatus {
    fn into_status(self) -> StatusCode;
}

impl IntoStatus for StatusCode {
    fn into_status(self) -> StatusCode {
        self
    }
}

impl IntoStatus for u16 {
    fn into_status(self) -> StatusCode {
        StatusCode::from_u16(self).unwrap_or_else(|_| {
            tracing::warn!(status = self, "invalid status code, using 500");
            StatusCode::INTERNAL_SERVER_ERROR
        })
    }
}

/// `Resp::new(status).header(k, v).xml(s).json(&v).body(bytes).build()`
///
/// `header` replaces an existing header of the same name, so a later
/// `.header("Content-Type", ...)` overrides the type set by `.xml()` / `.json()`.
/// Use `append_header` for headers that may repeat. A header name or value that
/// is not valid is dropped with a warning; it never panics.
pub struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
    stream: Option<Box<dyn crate::stream::BodyStream>>,
}

impl Resp {
    pub fn new(status: impl IntoStatus) -> Resp {
        Resp { status: status.into_status(), headers: HeaderMap::new(), body: Bytes::new(), stream: None }
    }

    fn parse(k: &str, v: &str) -> Option<(HeaderName, HeaderValue)> {
        let name = match HeaderName::from_bytes(k.as_bytes()) {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!(header = k, "dropping response header with invalid name");
                return None;
            }
        };
        match HeaderValue::from_str(v) {
            Ok(val) => Some((name, val)),
            Err(_) => {
                tracing::warn!(header = k, "dropping response header with invalid value");
                None
            }
        }
    }

    /// Set a header, replacing any existing value.
    pub fn header(mut self, k: impl AsRef<str>, v: impl ToString) -> Resp {
        if let Some((n, val)) = Self::parse(k.as_ref(), &v.to_string()) {
            self.headers.insert(n, val);
        }
        self
    }

    /// Add a header, keeping existing values of the same name.
    pub fn append_header(mut self, k: impl AsRef<str>, v: impl ToString) -> Resp {
        if let Some((n, val)) = Self::parse(k.as_ref(), &v.to_string()) {
            self.headers.append(n, val);
        }
        self
    }

    /// Body plus `Content-Type: application/xml`.
    pub fn xml(mut self, s: impl Into<Bytes>) -> Resp {
        self.headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
        self.body = s.into();
        self
    }

    /// Serialized body plus `Content-Type: application/json; charset=utf-8`.
    pub fn json<T: Serialize + ?Sized>(mut self, v: &T) -> Resp {
        self.headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
        self.body = match serde_json::to_vec(v) {
            Ok(b) => Bytes::from(b),
            Err(e) => {
                tracing::warn!(error = %e, "json serialization failed");
                Bytes::from_static(b"null")
            }
        };
        self
    }

    /// Body plus `Content-Type: text/plain; charset=utf-8`.
    pub fn text(mut self, s: impl Into<String>) -> Resp {
        self.headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
        self.body = Bytes::from(s.into());
        self
    }

    /// Body only; does not touch `Content-Type`.
    pub fn body(mut self, b: impl Into<Bytes>) -> Resp {
        self.body = b.into();
        self
    }

    /// A body that core reads chunk by chunk while the client takes it (see [`crate::stream`]). Replaces
    /// any body set before; set no `Content-Length` unless the size is known.
    pub fn stream(mut self, s: impl crate::stream::BodyStream) -> Resp {
        self.stream = Some(Box::new(s));
        self
    }

    pub fn build(self) -> Response {
        let mut r = http::Response::new(self.body);
        *r.status_mut() = self.status;
        *r.headers_mut() = self.headers;
        if let Some(s) = self.stream {
            crate::stream::attach(&mut r, BoxedStream(s));
        }
        r
    }
}

struct BoxedStream(Box<dyn crate::stream::BodyStream>);

impl crate::stream::BodyStream for BoxedStream {
    fn poll_chunk(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<std::io::Result<Bytes>>> {
        self.0.poll_chunk(cx)
    }
}

impl std::fmt::Debug for Resp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resp")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body", &self.body)
            .field("streamed", &self.stream.is_some())
            .finish()
    }
}

impl From<Resp> for Response {
    fn from(r: Resp) -> Response {
        r.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_and_json_content_types() {
        let r = Resp::new(200u16).xml("<a/>").build();
        assert_eq!(r.headers()["content-type"], "application/xml");
        assert_eq!(r.body().as_ref(), b"<a/>");
        let r = Resp::new(StatusCode::CREATED).json(&serde_json::json!({"a": 1})).build();
        assert_eq!(r.status(), 201);
        assert_eq!(r.headers()["content-type"], "application/json; charset=utf-8");
        assert_eq!(r.body().as_ref(), br#"{"a":1}"#);
    }

    #[test]
    fn later_header_overrides_content_type() {
        let r = Resp::new(200u16).xml("x").header("Content-Type", "text/xml").build();
        assert_eq!(r.headers()["content-type"], "text/xml");
        assert_eq!(r.headers().get_all("content-type").iter().count(), 1);
    }

    #[test]
    fn invalid_header_is_dropped() {
        let r = Resp::new(200u16).header("bad name", "v").header("x-ok", "line\nbreak").header("x-fine", 5).build();
        assert!(r.headers().get("x-ok").is_none());
        assert_eq!(r.headers().len(), 1);
        assert_eq!(r.headers()["x-fine"], "5");
    }

    #[test]
    fn invalid_status_is_500() {
        assert_eq!(Resp::new(99u16).build().status(), 500);
        assert_eq!(Resp::new(7000u16).build().status(), 500);
    }

    #[test]
    fn append_keeps_values() {
        let r = Resp::new(200u16).append_header("set-cookie", "a=1").append_header("set-cookie", "b=2").build();
        assert_eq!(r.headers().get_all("set-cookie").iter().count(), 2);
    }
}
