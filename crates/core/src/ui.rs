//! `GET /_azemu/ui`: the page of the inspector. Three files compiled into the binary, no build step and nothing fetched from
//! another host (`docs/inspector.md`).

use http::{header, Method, StatusCode};

use crate::resp::Resp;
use crate::Response;

const INDEX: &str = include_str!("ui/index.html");
const SCRIPT: &str = include_str!("ui/app.js");
const STYLE: &str = include_str!("ui/app.css");

/// What the page may load and do: its own script and style, and calls to its own address. Nothing inline, nothing from another host,
/// no frames, no forms.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Is this (collapsed) admin path one of the page's?
pub(crate) fn is_ui_path(path: &str) -> bool {
    matches!(path, "/_azemu/ui" | "/_azemu/ui/" | "/_azemu/ui/app.js" | "/_azemu/ui/app.css")
}

pub(crate) fn handle(method: &Method, path: &str) -> Response {
    if !matches!(*method, Method::GET | Method::HEAD) {
        return Resp::new(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, "GET, HEAD")
            .json(&serde_json::json!({"error": {"code": "MethodNotAllowed", "message": "Method not allowed."}}))
            .build();
    }
    let (content_type, body) = match path {
        "/_azemu/ui/app.js" => ("text/javascript; charset=utf-8", SCRIPT),
        "/_azemu/ui/app.css" => ("text/css; charset=utf-8", STYLE),
        _ => ("text/html; charset=utf-8", INDEX),
    };
    Resp::new(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::REFERRER_POLICY, "no-referrer")
        .header(header::CACHE_CONTROL, "no-store")
        .header("cross-origin-opener-policy", "same-origin")
        .header("cross-origin-resource-policy", "same-origin")
        .body(bytes::Bytes::from_static(body.as_bytes()))
        .build()
}
