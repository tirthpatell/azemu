//! Block blobs: Put Block, Put Block From URL, Put Block List, Get Block List.

use std::collections::HashMap;

use azemu_core::bytes::Bytes;
use azemu_core::resp::Resp;
use azemu_core::time;

use crate::blob::guard_overwrite;
use crate::copy::{read_source, source_range, SourceRead};
use crate::cx::{refresh_entry, snapshot_param, Cx};
use crate::errors::{blob_not_found, container_not_found, err, ApiError, ApiResult};
use crate::headers::{hash_headers, write_headers};
use crate::lease::{self, Scope};
use crate::state::{Blob, BlobKind, Seg};
use crate::store::{check_container_scope, check_cpk, check_scope_write, install, new_spec};
use crate::util::*;
use crate::xml::{self, Out};

/// Block ids are Base64 strings of at most 64 bytes. Ids that are not valid Base64 are
/// accepted as opaque strings (up to the Base64 length of 64 bytes): recordings of real
/// traffic have their ids replaced by placeholders such as an all-zero GUID.
fn valid_block_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 88
}

/// Staged blocks that may wait for a Put Block List, per blob.
const MAX_UNCOMMITTED: usize = 100_000;

fn block_id(req: &azemu_core::Request) -> Result<String, ApiError> {
    let id =
        req.query.get("blockid").ok_or_else(|| ApiError::new("MissingRequiredQueryParameter").detail("QueryParameterName", "blockid"))?;
    valid_block_id(id).then(|| id.to_string()).ok_or_else(|| ApiError::invalid_query("blockid", id))
}

/// Store a staged block. Creates no blob.
fn stage(cx: &mut Cx, container: &str, name: &str, id: String, data: Bytes) -> Result<(), ApiError> {
    let req = cx.req;
    let now = cx.now;
    validate_blob_name(name)?;
    let c = cx.container(container)?;
    check_container_scope(c, req)?;
    let e = c.blobs.entry(name.to_string()).or_default();
    refresh_entry(e, now);
    let res = (|| -> Result<(), ApiError> {
        if let Some(b) = e.live() {
            if b.kind != BlobKind::Block {
                return Err(err("InvalidBlobType"));
            }
            lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
            check_cpk(b, req)?;
        }
        // Every block id of a blob has the same length.
        let existing_len = e
            .uncommitted
            .keys()
            .next()
            .map(|k| k.len())
            .or_else(|| e.live().and_then(|b| b.segs.iter().find_map(|s| s.id.as_ref().map(|i| i.len()))));
        if let Some(l) = existing_len {
            if l != id.len() && !(e.uncommitted.len() == 1 && e.uncommitted.contains_key(&id)) {
                return Err(err("InvalidBlobOrBlock"));
            }
        }
        // Azure allows 100,000 uncommitted blocks per blob.
        if e.uncommitted.len() >= MAX_UNCOMMITTED && !e.uncommitted.contains_key(&id) {
            return Err(err("BlockCountExceedsLimit"));
        }
        Ok(())
    })();
    if let Err(er) = res {
        c.prune(name);
        return Err(er);
    }
    e.uncommitted.insert(id, data);
    Ok(())
}

pub fn put_block(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let id = block_id(req)?;
    let (md5, crc) = check_body_hashes(req, &req.body, cx.pre)?;
    stage(cx, container, name, id, req.body.clone())?;
    let mut r = hash_headers(Resp::new(201u16), &md5, crc).header("x-ms-request-server-encrypted", "true");
    if let Some(k) = hdr(req, "x-ms-encryption-key-sha256") {
        r = r.header("x-ms-encryption-key-sha256", k);
    }
    Ok(r.build())
}

pub fn put_block_from_url(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let id = block_id(req)?;
    let url = hdr(req, "x-ms-copy-source").ok_or_else(|| ApiError::missing_header("x-ms-copy-source"))?;
    let range = source_range(req)?;
    let SourceRead { data, .. } = read_source(cx, url, range, true)?;
    if let Some(want) = request_md5(req, "x-ms-source-content-md5")? {
        if want != cx.pre.md5(&data) {
            return Err(err("Md5Mismatch"));
        }
    }
    let (m, c) = (cx.pre.md5(&data), cx.pre.crc64(&data));
    stage(cx, container, name, id, data)?;
    Ok(hash_headers(Resp::new(201u16), &m, c).header("x-ms-request-server-encrypted", "true").build())
}

pub fn put_block_list(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    validate_blob_name(name)?;
    // The body was parsed before the lock was taken (`pre.rs`); parse here if it was not.
    let pre = cx.pre;
    let parsed;
    let root: &xml::Node = match &pre.block_list {
        Some(Ok(n)) => n,
        Some(Err(e)) => return Err(ApiError::invalid_xml(e)),
        None => {
            let text = std::str::from_utf8(&req.body).map_err(|_| ApiError::invalid_xml("The body is not valid UTF-8."))?;
            parsed = xml::parse(text).map_err(|e| ApiError::invalid_xml(&e))?;
            &parsed
        }
    };
    if root.name != "BlockList" {
        return Err(ApiError::invalid_xml("The root element must be BlockList."));
    }
    let (md5v, crc) = check_body_hashes(req, &req.body, cx.pre)?;
    let spec = new_spec(req, BlobKind::Block, false)?;
    let (containers, mut ctl) = cx.account().split();
    let c = containers.get_mut(container).ok_or_else(container_not_found)?;
    check_container_scope(c, req)?;
    let entry = c.blobs.entry(name.to_string()).or_default();
    let res = (|| -> Result<Vec<Seg>, ApiError> {
        guard_overwrite(entry, req, now)?;
        if let Some(b) = entry.live() {
            if b.kind != BlobKind::Block {
                return Err(err("InvalidBlobType"));
            }
            check_cpk(b, req)?;
            check_scope_write(b, req)?;
        }
        let mut segs: Vec<Seg> = vec![];
        // Committed blocks by id (the first one wins), so that each list item is a lookup and
        // not a walk over all the blocks.
        let mut committed_by_id: HashMap<&str, &Bytes> = HashMap::new();
        if let Some(b) = entry.live() {
            for s in &b.segs {
                if let Some(i) = s.id.as_deref() {
                    committed_by_id.entry(i).or_insert(&s.data);
                }
            }
        }
        for item in &root.children {
            let id = item.text.trim();
            let kind = item.name.as_str();
            if !matches!(kind, "Committed" | "Uncommitted" | "Latest") {
                return Err(ApiError::invalid_xml(&format!("Unexpected element {kind} in the block list.")));
            }
            if !valid_block_id(id) {
                return Err(err("InvalidBlockList"));
            }
            let committed = || committed_by_id.get(id).map(|d| (*d).clone());
            let staged = || entry.uncommitted.get(id).cloned();
            let data = match kind {
                "Committed" => committed(),
                "Uncommitted" => staged(),
                _ => staged().or_else(committed),
            };
            let data = data.ok_or_else(|| err("InvalidBlockList").msg("The specified block list is invalid."))?;
            segs.push(Seg { id: Some(id.to_string()), data });
        }
        if segs.len() > 50_000 {
            return Err(err("BlockCountExceedsLimit"));
        }
        Ok(segs)
    })();
    let segs = match res {
        Ok(s) => s,
        Err(e) => {
            c.prune(name);
            return Err(e);
        }
    };
    let mut blob: Blob = spec.build(BlobKind::Block, now);
    blob.size = segs.iter().map(|s| s.data.len() as u64).sum();
    blob.segs = segs;
    install(&mut ctl, entry, blob, now);
    let b = entry.current.as_ref().ok_or_else(blob_not_found)?;
    Ok(hash_headers(write_headers(Resp::new(201u16), b), &md5v, crc).build())
}

pub fn get_block_list(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let kind = req.query.get("blocklisttype").unwrap_or("committed").to_ascii_lowercase();
    if !["committed", "uncommitted", "all"].contains(&kind.as_str()) {
        return Err(ApiError::invalid_query("blocklisttype", &kind));
    }
    let snapshot = snapshot_param(req)?;
    let c = cx.container(container)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let blob = e.find(snapshot.as_deref(), None);
    if blob.is_none() && (e.uncommitted.is_empty() || snapshot.is_some()) {
        return Err(blob_not_found());
    }
    if let Some(b) = blob {
        if b.kind != BlobKind::Block {
            return Err(err("InvalidBlobType"));
        }
        crate::cond::check(req, "", Some((&b.etag, b.last_modified)), crate::cond::Kind::Read)?;
        crate::cond::check_tags(req, Some(&b.tags))?;
        lease::check_read(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    }
    let mut o = Out::new();
    o.open("BlockList");
    if kind == "committed" || kind == "all" {
        let named: Vec<&Seg> = blob.map(|b| b.segs.iter().filter(|s| s.id.is_some()).collect()).unwrap_or_default();
        if named.is_empty() {
            o.empty("CommittedBlocks");
        } else {
            o.open("CommittedBlocks");
            for s in named {
                o.open("Block").el("Name", s.id.as_deref().unwrap_or("")).el("Size", &s.data.len().to_string()).close("Block");
            }
            o.close("CommittedBlocks");
        }
    }
    if (kind == "uncommitted" || kind == "all") && snapshot.is_none() {
        if e.uncommitted.is_empty() {
            o.empty("UncommittedBlocks");
        } else {
            o.open("UncommittedBlocks");
            for (id, d) in &e.uncommitted {
                o.open("Block").el("Name", id).el("Size", &d.len().to_string()).close("Block");
            }
            o.close("UncommittedBlocks");
        }
    }
    o.close("BlockList");
    let mut r = Resp::new(200u16).xml(o.finish());
    r = r.header("x-ms-blob-content-length", blob.map(|b| b.size).unwrap_or(0));
    if let Some(b) = blob {
        r = r.header("ETag", &b.etag).header("Last-Modified", time::rfc1123(b.last_modified));
    }
    Ok(r.build())
}
