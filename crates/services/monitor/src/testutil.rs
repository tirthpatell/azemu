//! Helpers for the unit tests.

use azemu_core::http::{HeaderMap, HeaderValue, Method};
use azemu_core::{Endpoint, Query, Request, RequestCtx};

/// A POST to `/v2/track` with a body and an optional `Content-Encoding`.
pub(crate) fn request(body: Vec<u8>, encoding: Option<&str>) -> Request {
    let mut headers = HeaderMap::new();
    if let Some(e) = encoding {
        headers.insert("content-encoding", HeaderValue::from_str(e).unwrap());
    }
    Request {
        method: Method::POST,
        path: "/v2/track".into(),
        query: Query::default(),
        headers,
        body: body.into(),
        account: String::new(),
        ctx: RequestCtx {
            request_id: "r".into(),
            time: time::OffsetDateTime::now_utc(),
            endpoint: Endpoint { scheme: "http", authority: "h".into(), path_style: false, account: String::new() },
        },
    }
}
