//! Maps method and path to a handler. Anything else answers 501 naming the operation.

use azemu_core::{Request, Response};

use crate::state::Store;
use crate::{errors, keyvalues, lists, locks, snapshots};

pub fn route(store: &mut Store, req: &Request) -> Response {
    let path = req.path.strip_prefix('/').unwrap_or(&req.path);
    let (first, rest) = match path.split_once('/') {
        Some((f, r)) => (f, Some(r)),
        None => (path, None),
    };
    // HEAD is answered like GET; core drops the body and keeps the length.
    let method = match req.method.as_str() {
        "HEAD" => "GET",
        m => m,
    };
    snapshots::purge_expired(store, req.ctx.time);
    store.enforce(req.ctx.time);
    match (method, first, rest) {
        ("GET", "kv", None) => lists::kv_list(store, req),
        ("GET", "kv", Some(_)) => keyvalues::get(store, req),
        ("PUT", "kv", Some(_)) => keyvalues::put(store, req),
        ("DELETE", "kv", Some(_)) => keyvalues::delete(store, req),
        ("PUT", "locks", Some(_)) => locks::set_lock(store, req, true),
        ("DELETE", "locks", Some(_)) => locks::set_lock(store, req, false),
        ("GET", "keys", None) => lists::keys(store, req),
        ("GET", "labels", None) => lists::labels(store, req),
        ("GET", "revisions", None) => lists::revisions(store, req),
        ("GET", "snapshots", None) => snapshots::list(store, req),
        ("GET", "snapshots", Some(_)) => snapshots::get(store, req),
        ("PUT", "snapshots", Some(_)) => snapshots::put(store, req),
        ("PATCH", "snapshots", Some(_)) => snapshots::patch(store, req),
        ("GET", "operations", None) => snapshots::operation(store, req),
        _ => errors::not_implemented(req.method.as_str(), &req.decoded_path()),
    }
}
