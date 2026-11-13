//! Response headers common to blob operations.

use azemu_core::resp::Resp;
use azemu_core::time::rfc1123;

use crate::state::{Blob, BlobKind};
use crate::util::{b64, crc64_b64};

/// Properties every Get Blob / Get Blob Properties response carries.
pub fn blob_headers(mut r: Resp, b: &Blob, is_current: bool) -> Resp {
    r = r
        .header("ETag", &b.etag)
        .header("Last-Modified", rfc1123(b.last_modified))
        .header("Content-Type", &b.props.content_type)
        .header("Accept-Ranges", "bytes")
        .header("x-ms-blob-type", b.kind.name())
        .header("x-ms-creation-time", rfc1123(b.creation_time))
        .header("x-ms-lease-status", b.lease.status_str())
        .header("x-ms-lease-state", b.lease.state_str());
    if let Some(d) = b.lease.duration_str() {
        r = r.header("x-ms-lease-duration", d);
    }
    if let Some(v) = &b.props.content_encoding {
        r = r.header("Content-Encoding", v);
    }
    if let Some(v) = &b.props.content_language {
        r = r.header("Content-Language", v);
    }
    if let Some(v) = &b.props.content_disposition {
        r = r.header("Content-Disposition", v);
    }
    if let Some(v) = &b.props.cache_control {
        r = r.header("Cache-Control", v);
    }
    for (k, v) in &b.metadata {
        r = r.header(format!("x-ms-meta-{k}"), v);
    }
    r = r.header("x-ms-server-encrypted", "true");
    if let Some(k) = &b.cpk_sha256 {
        r = r.header("x-ms-encryption-key-sha256", k);
    }
    if let Some(s) = &b.encryption_scope {
        r = r.header("x-ms-encryption-scope", s);
    }
    if let Some((tier, inferred)) = b.tier_info() {
        // Azure only sends this header when the tier is inferred; sending `false` for an
        // explicit tier is harmless and keeps SDK clients that expect a bool happy.
        r = r.header("x-ms-access-tier", tier).header("x-ms-access-tier-inferred", inferred.to_string());
    }
    if b.tier.as_deref() == Some("Smart") {
        r = r.header("x-ms-smart-access-tier", "Hot");
    }
    if let Some(t) = b.tier_changed {
        r = r.header("x-ms-access-tier-change-time", rfc1123(t));
    }
    if let Some(s) = &b.archive_status {
        r = r.header("x-ms-archive-status", s);
    }
    if let Some(s) = &b.rehydrate_priority {
        r = r.header("x-ms-rehydrate-priority", s);
    }
    if let Some(c) = &b.copy {
        r = r
            .header("x-ms-copy-id", &c.id)
            .header("x-ms-copy-source", &c.source)
            .header("x-ms-copy-status", &c.status)
            .header("x-ms-copy-progress", &c.progress)
            .header("x-ms-copy-completion-time", rfc1123(c.completed));
    }
    if !b.tags.is_empty() {
        r = r.header("x-ms-tag-count", b.tags.len());
    }
    if let Some(v) = &b.version_id {
        r = r.header("x-ms-version-id", v);
        if b.snapshot.is_none() {
            r = r.header("x-ms-is-current-version", if is_current { "true" } else { "false" });
        }
    }
    match b.kind {
        BlobKind::Append => {
            r = r.header("x-ms-blob-committed-block-count", b.block_count());
            if b.sealed {
                r = r.header("x-ms-blob-sealed", "true");
            }
        }
        BlobKind::Page => {
            r = r.header("x-ms-blob-sequence-number", b.sequence);
        }
        BlobKind::Block => {}
    }
    if let Some(i) = &b.immutability {
        r = r.header("x-ms-immutability-policy-until-date", rfc1123(i.until)).header("x-ms-immutability-policy-mode", &i.mode);
    }
    if b.legal_hold {
        r = r.header("x-ms-legal-hold", "true");
    }
    if let Some(d) = &b.dfs {
        r = r
            .header("x-ms-owner", &d.owner)
            .header("x-ms-group", &d.group)
            .header("x-ms-permissions", d.permissions())
            .header("x-ms-acl", d.acl_string())
            .header("x-ms-resource-type", d.resource_type());
        if let Some(t) = d.expiry {
            r = r.header("x-ms-expiry-time", rfc1123(t));
        }
        if let Some(c) = &d.encryption_context {
            r = r.header("x-ms-encryption-context", c);
        }
    }
    r
}

/// Content-MD5 of the blob, when one is stored.
pub fn md5_header(b: &Blob) -> Option<String> {
    b.props.content_md5.as_ref().map(|m| b64(m))
}

/// Headers for the success answer of a write that changed a blob.
pub fn write_headers(r: Resp, b: &Blob) -> Resp {
    let mut r = r.header("ETag", &b.etag).header("Last-Modified", rfc1123(b.last_modified)).header("x-ms-request-server-encrypted", "true");
    if let Some(v) = &b.version_id {
        r = r.header("x-ms-version-id", v);
    }
    if let Some(k) = &b.cpk_sha256 {
        r = r.header("x-ms-encryption-key-sha256", k);
    }
    if let Some(s) = &b.encryption_scope {
        r = r.header("x-ms-encryption-scope", s);
    }
    r
}

/// `Content-MD5` and `x-ms-content-crc64` of a body.
pub fn hash_headers(r: Resp, md5: &[u8], crc: u64) -> Resp {
    r.header("Content-MD5", b64(md5)).header("x-ms-content-crc64", crc64_b64(crc))
}
