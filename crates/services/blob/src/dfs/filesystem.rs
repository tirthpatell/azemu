//! Filesystem operations: a filesystem is a container.

use azemu_core::etag;
use azemu_core::resp::Resp;

use crate::cond::{self, Kind};
use crate::cx::Cx;
use crate::dfs::model::{adopt, purge_expired};
use crate::dfs::util::{format_properties, http_date, parse_properties};
use crate::errors::{err, ApiError, ApiResult};
use crate::state::Container;
use crate::util::validate_container_name;

/// The container behind a filesystem name, switched to hierarchical and brought up to date
/// (blob-endpoint writes adopted, expired paths gone).
pub fn open<'a>(cx: &'a mut Cx, fs: &str) -> Result<&'a mut Container, ApiError> {
    let now = cx.now;
    let c = cx.container(fs)?;
    let mut changed = false;
    if !c.hns {
        // first touch: everything written through the blob endpoint so far is taken in
        c.hns = true;
        c.dirty = true;
        changed = true;
    }
    changed |= c.dirty || !c.touched.is_empty();
    adopt(c, now);
    purge_expired(c, now);
    cx.st.changed |= changed;
    // `c` was borrowed from `cx`; take it again
    cx.container(fs)
}

pub fn create(cx: &mut Cx, fs: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    validate_container_name(fs)?;
    let metadata = parse_properties(req)?.unwrap_or_default();
    let acct = cx.account();
    if acct.containers.contains_key(fs) {
        return Err(err("ContainerAlreadyExists"));
    }
    let mut c = Container::new(fs, now, etag::new_etag());
    c.metadata = metadata;
    c.hns = true;
    let r = Resp::new(201u16)
        .header("ETag", &c.etag)
        .header("Last-Modified", http_date(c.last_modified))
        .header("x-ms-namespace-enabled", "true")
        .build();
    acct.containers.insert(fs.to_string(), c);
    Ok(r)
}

pub fn get_properties(cx: &mut Cx, fs: &str) -> ApiResult {
    let c = open(cx, fs)?;
    Ok(Resp::new(200u16)
        .header("ETag", &c.etag)
        .header("Last-Modified", http_date(c.last_modified))
        .header("x-ms-properties", format_properties(&c.metadata))
        .header("x-ms-namespace-enabled", "true")
        .build())
}

pub fn set_properties(cx: &mut Cx, fs: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let metadata = parse_properties(req)?.unwrap_or_default();
    let c = open(cx, fs)?;
    cond::check_container(req, &c.etag, c.last_modified, Kind::Write)?;
    c.metadata = metadata;
    c.touch(now);
    Ok(Resp::new(200u16)
        .header("ETag", &c.etag)
        .header("Last-Modified", http_date(c.last_modified))
        .header("x-ms-namespace-enabled", "true")
        .build())
}

pub fn delete(cx: &mut Cx, fs: &str) -> ApiResult {
    crate::container::delete(cx, fs)
}
