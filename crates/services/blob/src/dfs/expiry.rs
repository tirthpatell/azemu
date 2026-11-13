//! Path expiry: `x-ms-expiry-option` / `x-ms-expiry-time` on create and Set Expiry.

use azemu_core::resp::Resp;
use azemu_core::time::{parse_iso8601, parse_rfc1123, Duration};
use azemu_core::Request;

use crate::cond::{self, Kind};
use crate::cx::{refresh_entry, Cx};
use crate::dfs::model::{is_dir, path_conflict};
use crate::dfs::util::http_date;
use crate::errors::{blob_not_found, ApiError, ApiResult};
use crate::lease::{self, Scope};
use crate::state::Time;
use crate::util::hdr;

/// The expiry a request asks for: `None` when it says nothing, `Some(None)` for
/// `NeverExpire`, `Some(Some(t))` for a time.
pub fn resolve(req: &Request, now: Time, creation: Time) -> Result<Option<Option<Time>>, ApiError> {
    let Some(opt) = hdr(req, "x-ms-expiry-option") else { return Ok(None) };
    let given = hdr(req, "x-ms-expiry-time");
    let need_time = || given.ok_or_else(|| ApiError::missing_header("x-ms-expiry-time"));
    let relative = |base: Time| -> Result<Option<Option<Time>>, ApiError> {
        let v = need_time()?;
        let ms: i64 = v.trim().parse().map_err(|_| ApiError::invalid_header("x-ms-expiry-time", v))?;
        if ms < 0 {
            return Err(ApiError::invalid_header("x-ms-expiry-time", v));
        }
        let t = base.checked_add(Duration::milliseconds(ms)).ok_or_else(|| ApiError::invalid_header("x-ms-expiry-time", v))?;
        Ok(Some(Some(t)))
    };
    match opt.to_ascii_lowercase().as_str() {
        "neverexpire" => Ok(Some(None)),
        "relativetonow" => relative(now),
        "relativetocreation" => relative(creation),
        "absolute" => {
            let v = need_time()?;
            let t = parse_rfc1123(v).or_else(|| parse_iso8601(v)).ok_or_else(|| ApiError::invalid_header("x-ms-expiry-time", v))?;
            if t <= now {
                return Err(ApiError::invalid_header("x-ms-expiry-time", v));
            }
            Ok(Some(Some(t)))
        }
        _ => Err(ApiError::invalid_header("x-ms-expiry-option", opt)),
    }
}

/// Set Expiry (`PUT ?comp=expiry`, on both endpoints, hierarchical containers only).
pub fn set_expiry(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    if hdr(req, "x-ms-expiry-option").is_none() {
        return Err(ApiError::missing_header("x-ms-expiry-option"));
    }
    let c = cx.container(container)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let b = e.live_mut().ok_or_else(blob_not_found)?;
    cond::check(req, "", Some((&b.etag, b.last_modified)), Kind::Write)?;
    lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    if is_dir(b) {
        return Err(path_conflict());
    }
    let creation = b.creation_time;
    let wanted = resolve(req, now, creation)?.unwrap_or(None);
    if let Some(d) = b.dfs.as_mut() {
        d.expiry = wanted;
    }
    Ok(Resp::new(200u16).header("ETag", &b.etag).header("Last-Modified", http_date(b.last_modified)).build())
}
