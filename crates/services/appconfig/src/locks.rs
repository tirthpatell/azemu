//! `PUT /locks/{key}` (make read-only) and `DELETE /locks/{key}` (make writable again).

use azemu_core::conditions::{Access, Outcome};
use azemu_core::{Request, Response};

use crate::auth;
use crate::errors;
use crate::keyvalues::{conditions, key_from_path, label_param, resource};
use crate::render::kv_response;
use crate::state::{NewSetting, Store};

/// Locking and unlocking give the setting a new etag and a new revision.
pub fn set_lock(store: &mut Store, req: &Request, locked: bool) -> Response {
    let key = match key_from_path(req, "locks") {
        Ok(k) => k,
        Err(r) => return r,
    };
    let label = label_param(req);
    let Some(existing) = store.live_get(&key, label.as_deref()) else {
        return errors::not_found();
    };
    if !matches!(conditions(req).evaluate(resource(Some(existing)), Access::Write), Outcome::Proceed) {
        return errors::precondition_failed();
    }
    let new = NewSetting {
        key,
        label,
        value: existing.value.clone(),
        content_type: existing.content_type.clone(),
        description: existing.description.clone(),
        tags: existing.tags.clone(),
        locked,
    };
    match store.write(new, req.ctx.time) {
        Ok(s) => kv_response(200, &s, None, auth::is_newer(auth::version(req))),
        Err(_) => errors::quota_exceeded("Storage Quota"),
    }
}
