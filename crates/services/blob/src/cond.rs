//! Conditional headers for blobs and containers. Each of the four headers is
//! evaluated on its own (as Azure does); `x-ms-if-tags` is evaluated against the
//! blob's tags.

use std::collections::BTreeMap;

use azemu_core::etag::EtagMatch;
use azemu_core::time::{parse_rfc1123, truncate_to_secs};
use azemu_core::Request;

use crate::errors::{err, ApiError};
use crate::state::Time;
use crate::tags;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// GET / HEAD: a failed If-None-Match / If-Modified-Since is a 304.
    Read,
    /// A write: everything that fails is 412.
    Write,
    /// A write that creates a blob: `If-None-Match: *` on an existing blob is 409 BlobAlreadyExists.
    Create,
    /// Same, but the request carries the blob's lease id: Azure then lets `If-None-Match: *` pass.
    CreateLeased,
}

/// Evaluate If-Match / If-None-Match / If-Modified-Since / If-Unmodified-Since.
/// `prefix` is `""` for the request's own conditions, `"x-ms-blob-"` for the blob
/// conditions of the tag operations and `"x-ms-source-"` for a copy source.
pub fn check(req: &Request, prefix: &str, resource: Option<(&str, Time)>, kind: Kind) -> Result<(), ApiError> {
    let get = |n: &str| req.header(&format!("{prefix}{n}"));
    let if_match = get("if-match");
    let if_none_match = get("if-none-match");
    let if_modified = get("if-modified-since").and_then(parse_rfc1123);
    let if_unmodified = get("if-unmodified-since").and_then(parse_rfc1123);

    let failed = |e: ApiError| -> ApiError {
        if prefix != "x-ms-source-" {
            e
        } else {
            ApiError::new("CannotVerifyCopySource").status(e.status).msg("Could not verify the copy source within the specified time.")
        }
    };

    let Some((etag, last_modified)) = resource else {
        if if_match.is_some() || if_modified.is_some() || if_unmodified.is_some() {
            return Err(failed(err("ConditionNotMet")));
        }
        return Ok(());
    };
    let lm = truncate_to_secs(last_modified);
    let not_met = || failed(err("ConditionNotMet"));

    if let Some(im) = if_match {
        if !EtagMatch::parse(im).matches(etag) {
            return Err(not_met());
        }
    }
    if let Some(d) = if_unmodified {
        if lm > truncate_to_secs(d) {
            return Err(not_met());
        }
    }
    if let Some(inm) = if_none_match {
        if EtagMatch::parse(inm).matches(etag) {
            if kind == Kind::CreateLeased && inm.trim() == "*" {
                // allowed
            } else {
                return Err(match kind {
                    Kind::Read => not_modified(etag, last_modified, prefix),
                    Kind::Create if inm.trim() == "*" => failed(err("BlobAlreadyExists")),
                    _ => not_met(),
                });
            }
        }
    }
    if let Some(d) = if_modified {
        if lm <= truncate_to_secs(d) {
            return Err(match kind {
                Kind::Read => not_modified(etag, last_modified, prefix),
                _ => not_met(),
            });
        }
    }
    Ok(())
}

fn not_modified(_etag: &str, _lm: Time, prefix: &str) -> ApiError {
    if prefix != "x-ms-source-" {
        // Real Azure's 304 carries no ETag or Last-Modified, only x-ms-error-code
        // (every 304 in the recordings).
        ApiError::not_modified()
    } else {
        ApiError::new("CannotVerifyCopySource").status(304).msg("Could not verify the copy source within the specified time.")
    }
}

/// `x-ms-if-tags`: the expression must hold for the blob's tags.
pub fn check_tags(req: &Request, tags_of_blob: Option<&BTreeMap<String, String>>) -> Result<(), ApiError> {
    check_tags_header(req, "x-ms-if-tags", tags_of_blob)
}

pub fn check_tags_header(req: &Request, header: &str, tags_of_blob: Option<&BTreeMap<String, String>>) -> Result<(), ApiError> {
    let Some(expr) = req.header(header) else { return Ok(()) };
    let parsed = tags::parse(expr).map_err(|_| ApiError::invalid_header(header, expr))?;
    let empty = BTreeMap::new();
    let ctx = tags::EvalCtx { tags: tags_of_blob.unwrap_or(&empty), container: "" };
    if tags::holds(&parsed, &ctx) {
        Ok(())
    } else {
        Err(err("ConditionNotMet"))
    }
}

/// Conditions on a container (If-Modified-Since / If-Unmodified-Since only apply to
/// some operations; Azure also accepts the others and treats ETag as an opaque value).
pub fn check_container(req: &Request, etag: &str, lm: Time, kind: Kind) -> Result<(), ApiError> {
    check(req, "", Some((etag, lm)), kind)
}
