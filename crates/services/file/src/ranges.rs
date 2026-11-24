//! Put Range (update and clear), Put Range From URL and List Ranges.

use std::collections::BTreeMap;

use azemu_core::resp::Resp;
use azemu_core::time::{iso8601_7, rfc1123};
use azemu_core::{Request, Response};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use md5::{Digest, Md5};

use crate::copy;
use crate::dirs::{root_of, writable};
use crate::errors;
use crate::leasing::{self, Kind};
use crate::shares::{find, find_mut, snapshot_param};
use crate::state::{File, State, MAX_PUT_RANGE, PAGE};
use crate::tree;
use crate::xml;
use crate::Cx;

/// `bytes=a-b`, both ends given.
fn closed_range(v: &str) -> Option<(u64, u64)> {
    let rest = v.trim().strip_prefix("bytes=")?;
    let (a, b) = rest.split_once('-')?;
    let (a, b) = (a.trim().parse::<u64>().ok()?, b.trim().parse::<u64>().ok()?);
    (a <= b).then_some((a, b))
}

/// The number of bytes in `start..=end` (`start <= end`). A range written with the largest number is longer than any file, and
/// answers like a range that is too long, not with a panic.
fn range_len(start: u64, end: u64) -> u64 {
    (end - start).saturating_add(1)
}

/// The range of a write: `x-ms-range`, or `Range`.
fn write_range(req: &Request) -> Result<(u64, u64), Response> {
    let v = req.header("x-ms-range").or_else(|| req.header("range")).ok_or_else(|| errors::missing_header(&req.ctx, "Range"))?;
    closed_range(v).ok_or_else(|| errors::invalid_header(&req.ctx, "Range", v))
}

#[derive(PartialEq, Eq)]
enum Write {
    Update,
    Clear,
}

fn write_kind(req: &Request) -> Result<Write, Response> {
    match req.header("x-ms-write").map(str::trim) {
        None => Err(errors::missing_header(&req.ctx, "x-ms-write")),
        Some(v) if v.eq_ignore_ascii_case("update") => Ok(Write::Update),
        Some(v) if v.eq_ignore_ascii_case("clear") => Ok(Write::Clear),
        Some(v) => Err(errors::invalid_header(&req.ctx, "x-ms-write", v)),
    }
}

/// `x-ms-file-last-write-time` of a range write: `Now` (the default) or `Preserve`.
fn keeps_last_write(req: &Request) -> Result<bool, Response> {
    match req.header("x-ms-file-last-write-time").map(str::trim) {
        None => Ok(false),
        Some(v) if v.eq_ignore_ascii_case("now") => Ok(false),
        Some(v) if v.eq_ignore_ascii_case("preserve") => Ok(true),
        Some(v) => Err(errors::invalid_header(&req.ctx, "x-ms-file-last-write-time", v)),
    }
}

fn finish(f: &mut File, c: &Cx, keep_last_write: bool, md5: Option<[u8; 16]>) -> Response {
    f.touch(c.now);
    if !keep_last_write {
        f.smb.last_write = c.now;
    }
    let mut r = Resp::new(201u16)
        .header("ETag", &f.etag)
        .header("Last-Modified", rfc1123(f.last_modified))
        .header("x-ms-request-server-encrypted", "true")
        .header("x-ms-file-last-write-time", iso8601_7(f.smb.last_write));
    if let Some(m) = md5 {
        r = r.header("Content-MD5", B64.encode(m));
    }
    r.build()
}

pub fn put(state: &mut State, c: &Cx, share: &str, path: &[String]) -> Response {
    let req = c.req;
    let ctx = &req.ctx;
    if let Err(e) = writable(c) {
        return e;
    }
    let (kind, (start, end)) = match (write_kind(req), write_range(req)) {
        (Err(e), _) | (_, Err(e)) => return e,
        (Ok(k), Ok(r)) => (k, r),
    };
    let keep = match keeps_last_write(req) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let len = range_len(start, end);
    match kind {
        Write::Update if req.body.len() as u64 != len => return errors::invalid_header(ctx, "Content-Length", &req.body.len().to_string()),
        Write::Update if len > MAX_PUT_RANGE as u64 => {
            return errors::invalid_header(ctx, "Range", req.header("x-ms-range").or_else(|| req.header("range")).unwrap_or(""))
        }
        Write::Clear if !req.body.is_empty() => return errors::invalid_header(ctx, "Content-Length", &req.body.len().to_string()),
        _ => {}
    }
    let md5 = Md5::digest(&req.body);
    if kind == Write::Update {
        if let Some(given) = req.header("content-md5") {
            let ok = B64.decode(given.trim()).is_ok_and(|b| b.as_slice() == md5.as_slice());
            if !ok {
                return errors::md5_mismatch(ctx, given.trim(), &B64.encode(md5));
            }
        }
    }
    let acct = state.accounts.entry(req.account.to_ascii_lowercase()).or_default();
    let share = match find_mut_share(acct, req, share) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let f = match crate::files::locate_mut(share, path) {
        Ok(f) => f,
        Err(m) => return m.response(ctx),
    };
    if let Err(e) = leasing::check_write(req, Kind::File, &f.lease, c.now) {
        return e;
    }
    if end >= f.size {
        return errors::invalid_range(ctx);
    }
    match kind {
        Write::Update => {
            f.write(start, &req.body);
            finish(f, c, keep, Some(md5.into()))
        }
        Write::Clear => {
            f.clear(start, end + 1);
            finish(f, c, keep, None)
        }
    }
}

fn find_mut_share<'a>(acct: &'a mut crate::state::Account, req: &Request, name: &str) -> Result<&'a mut crate::state::Share, Response> {
    find_mut(acct, req, name)
}

pub fn put_from_url(state: &mut State, c: &Cx, share: &str, path: &[String]) -> Response {
    let req = c.req;
    let ctx = &req.ctx;
    if let Err(e) = writable(c) {
        return e;
    }
    let (kind, (start, end)) = match (write_kind(req), write_range(req)) {
        (Err(e), _) | (_, Err(e)) => return e,
        (Ok(k), Ok(r)) => (k, r),
    };
    if kind == Write::Clear {
        return errors::invalid_header(ctx, "x-ms-write", "clear");
    }
    let keep = match keeps_last_write(req) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let len = range_len(start, end);
    if len > MAX_PUT_RANGE as u64 {
        return errors::invalid_header(ctx, "Range", req.header("x-ms-range").or_else(|| req.header("range")).unwrap_or(""));
    }
    let Some(url) = req.header("x-ms-copy-source") else { return errors::missing_header(ctx, "x-ms-copy-source") };
    let Some(src_range) = req.header("x-ms-source-range") else { return errors::missing_header(ctx, "x-ms-source-range") };
    let Some((s_start, s_end)) = closed_range(src_range) else { return errors::invalid_header(ctx, "x-ms-source-range", src_range) };
    if range_len(s_start, s_end) != len {
        return errors::invalid_header(ctx, "x-ms-source-range", src_range);
    }
    let data = match copy::read_source(state, c, url, Some((s_start, s_end))) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let acct = state.accounts.entry(req.account.to_ascii_lowercase()).or_default();
    let share = match find_mut_share(acct, req, share) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let f = match crate::files::locate_mut(share, path) {
        Ok(f) => f,
        Err(m) => return m.response(ctx),
    };
    if let Err(e) = leasing::check_write(req, Kind::File, &f.lease, c.now) {
        return e;
    }
    if end >= f.size {
        return errors::invalid_range(ctx);
    }
    f.write(start, &data);
    finish_from_url(f, c, keep)
}

fn finish_from_url(f: &mut File, c: &Cx, keep: bool) -> Response {
    f.touch(c.now);
    if !keep {
        f.smb.last_write = c.now;
    }
    Resp::new(201u16)
        .header("ETag", &f.etag)
        .header("Last-Modified", rfc1123(f.last_modified))
        .header("x-ms-request-server-encrypted", "true")
        .header("x-ms-file-last-write-time", iso8601_7(f.smb.last_write))
        .build()
}

/// The pages of a file that hold data.
fn data_pages(f: &File) -> Vec<u64> {
    f.pages.keys().copied().filter(|p| p * PAGE < f.size).collect()
}

/// Runs of pages as inclusive byte ranges, cut at `size`.
fn runs(pages: &[u64], size: u64) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for p in pages {
        let start = p * PAGE;
        let end = (start + PAGE).min(size) - 1;
        match out.last_mut() {
            Some(last) if last.1 + 1 == start => last.1 = end,
            _ => out.push((start, end)),
        }
    }
    out
}

pub fn list(state: &State, c: &Cx, share: &str, path: &[String]) -> Response {
    let req = c.req;
    let ctx = &req.ctx;
    let snap = match snapshot_param(req) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let Some(acct) = state.accounts.get(&req.account.to_ascii_lowercase()) else { return errors::share_not_found(ctx) };
    let share = match find(acct, req, share) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let root = match root_of(share, &snap, c) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let f = match tree::file_at(root, path) {
        Ok(f) => f,
        Err(m) => return m.response(ctx),
    };
    if let Err(e) = leasing::check_read(req, Kind::File, &f.lease, c.now) {
        return e;
    }
    let window = match req.header("x-ms-range") {
        None => None,
        Some(v) => match closed_range(v) {
            Some(r) => Some(r),
            None => return errors::invalid_header(ctx, "x-ms-range", v),
        },
    };
    if let Some((_, e)) = window {
        if e >= f.size {
            return errors::invalid_range(ctx);
        }
    }
    let clip = |(a, b): (u64, u64)| -> Option<(u64, u64)> {
        match window {
            None => Some((a, b)),
            Some((s, e)) => {
                let (a, b) = (a.max(s), b.min(e));
                (a <= b).then_some((a, b))
            }
        }
    };
    let prev = match req.query.get("prevsharesnapshot") {
        None => None,
        Some(v) => match azemu_core::time::parse_iso8601(v).map(iso8601_7) {
            Some(t) => Some(t),
            None => return errors::invalid_query(ctx, "prevsharesnapshot", v, "Must be in the specific snapshot date time format."),
        },
    };
    let mut body = String::from("<Ranges>");
    let mut any = false;
    match prev {
        None => {
            for (a, b) in runs(&data_pages(f), f.size).into_iter().filter_map(clip) {
                body.push_str(&format!("<Range><Start>{a}</Start><End>{b}</End></Range>"));
                any = true;
            }
        }
        Some(t) => {
            let Some(old_snap) = share.snapshots.iter().find(|s| s.time == t) else {
                return errors::conflict(ctx, "PreviousSnapshotNotFound", "The previous snapshot was not found.");
            };
            let old = tree::file_at(&old_snap.root, path).ok();
            let (changed, cleared) = diff(old, f);
            for (a, b) in runs(&changed, f.size).into_iter().filter_map(clip) {
                body.push_str(&format!("<Range><Start>{a}</Start><End>{b}</End></Range>"));
                any = true;
            }
            // pages that held data and are zeros now, up to the larger size
            let top = old.map_or(0, |o| o.size).max(f.size);
            for (a, b) in runs(&cleared, top.max(1)).into_iter().filter_map(clip) {
                body.push_str(&format!("<ClearRange><Start>{a}</Start><End>{b}</End></ClearRange>"));
                any = true;
            }
        }
    }
    if any {
        body.push_str("</Ranges>");
    } else {
        body = "<Ranges />".to_string();
    }
    Resp::new(200u16)
        .header("Last-Modified", rfc1123(f.last_modified))
        .header("ETag", &f.etag)
        .header("x-ms-content-length", f.size)
        .xml(xml::document(&body))
        .build()
}

/// Pages that have data now and differ from the earlier file (`.0`), and pages that had data and have none now (`.1`).
fn diff(old: Option<&File>, new: &File) -> (Vec<u64>, Vec<u64>) {
    let empty: BTreeMap<u64, azemu_core::bytes::Bytes> = BTreeMap::new();
    let old_pages = old.map_or(&empty, |o| &o.pages);
    let old_size = old.map_or(0, |o| o.size);
    let mut changed = Vec::new();
    for (p, data) in new.pages.iter().filter(|(p, _)| *p * PAGE < new.size) {
        if old_pages.get(p) != Some(data) || *p * PAGE >= old_size {
            changed.push(*p);
        }
    }
    let mut cleared = Vec::new();
    for p in old_pages.keys().filter(|p| *p * PAGE < old_size) {
        if !new.pages.contains_key(p) || *p * PAGE >= new.size {
            cleared.push(*p);
        }
    }
    (changed, cleared)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_ranges() {
        assert_eq!(closed_range("bytes=0-9"), Some((0, 9)));
        assert_eq!(closed_range("bytes=5-2"), None);
        assert_eq!(closed_range("bytes=5-"), None);
        assert_eq!(closed_range("0-9"), None);
    }

    #[test]
    fn runs_join_neighbours_and_cut_at_the_size() {
        assert_eq!(runs(&[0, 1, 3], 1700), vec![(0, 1023), (1536, 1699)]);
        assert_eq!(runs(&[], 10), vec![]);
    }
}
