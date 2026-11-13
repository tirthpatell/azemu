//! Dispatch by method and path.
//!
//! * `POST /api/events` — classic publish
//! * `POST /topics/{topic}:publish` — namespace topic publish
//! * `POST /topics/{topic}/eventsubscriptions/{sub}:{receive|acknowledge|release|reject|renewLock}`
//! * `/_azemu/eventgrid/...` — admin API (see `admin.rs`)

use std::sync::Arc;

use azemu_core::{Request, Response};

use crate::{admin, errors, ns, publish, Shared};

pub async fn route(shared: &Arc<Shared>, req: &Request) -> Response {
    if req.path == "/_azemu/eventgrid" || req.path.starts_with("/_azemu/eventgrid/") {
        return admin::handle(shared, req).await;
    }
    let segs = req.segments();
    let lower: Vec<String> = segs.iter().map(|s| s.to_ascii_lowercase()).collect();
    let method = req.method.as_str();
    let what = format!("{method} {}", req.decoded_path());
    match lower.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["api", "events"] => {
            if method == "POST" {
                publish::classic(shared, req)
            } else {
                errors::method_not_allowed(&format!("{what} (events are published with POST)"))
            }
        }
        ["topics", _] if lower[1].ends_with(":publish") => {
            if method != "POST" {
                return errors::method_not_allowed(&what);
            }
            ns::publish(shared, req, strip(&segs[1]))
        }
        ["topics", _, "eventsubscriptions", _] => {
            let Some((sub, op)) = segs[3].rsplit_once(':') else {
                return errors::not_implemented(&what);
            };
            let topic = segs[1].as_str();
            if method != "POST" {
                return errors::method_not_allowed(&what);
            }
            match op.to_ascii_lowercase().as_str() {
                "receive" => ns::receive(shared, req, topic, sub).await,
                "acknowledge" => ns::acknowledge(shared, req, topic, sub),
                "release" => ns::release(shared, req, topic, sub),
                "reject" => ns::reject(shared, req, topic, sub),
                "renewlock" => ns::renew_lock(shared, req, topic, sub),
                _ => errors::not_implemented(&what),
            }
        }
        _ => errors::not_implemented(&what),
    }
}

/// `orders:publish` -> `orders`
fn strip(segment: &str) -> &str {
    segment.rsplit_once(':').map(|(a, _)| a).unwrap_or(segment)
}
