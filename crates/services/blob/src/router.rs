//! Request routing: split the path, identify the operation, authorize, dispatch.

use azemu_core::http::Method;
use azemu_core::storage::{self, StorageAccounts};
use azemu_core::{Request, Response};
use percent_encoding::percent_decode_str;

use crate::auth::{self, Target};
use crate::cx::Cx;
use crate::errors::{err, ApiError, ApiResult};
use crate::ops::{identify, Op};
use crate::pre::Pre;
use crate::state::State;
use crate::{append, batch, blob, block, container, copy, events, lease, list, page, props, service_props, tags};

pub const SERVER: &str = "Windows-Azure-Blob/1.0 Microsoft-HTTPAPI/2.0";

/// Handle one request against the state. Never panics on bad input.
///
/// `accounts` is `Some` for a request that arrived on the wire (its signature is checked)
/// and `None` for a sub-request of a batch (the outer request carried the signature).
pub fn run(st: &mut State, req: &Request, accounts: Option<&StorageAccounts>, prep: &Pre) -> Response {
    let result = run_inner(st, req, accounts, prep);
    let mut resp = match result {
        Ok(r) => r,
        Err(e) => e.into_response(&req.ctx),
    };
    if req.header("origin").is_some() && req.method != Method::OPTIONS {
        let rules = service_props::cors_rules(&st.account(&req.account.to_ascii_lowercase()).props);
        storage::cors::apply(&mut resp, req, &rules);
    }
    storage::finish(resp, req, SERVER)
}

/// The operation a request asks for (before any authorization).
pub(crate) fn op_of(req: &Request) -> Op {
    let (_, _, depth) = parse_target(req);
    identify(req, depth)
}

/// `(container, blob)` the request names (a blob in `$root` when the path has one segment).
pub(crate) fn target_of(req: &Request) -> (Option<String>, Option<String>, u8) {
    parse_target(req)
}

pub(crate) fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().to_string()
}

/// `(container, blob, depth)` from the path.
fn parse_target(req: &Request) -> (Option<String>, Option<String>, u8) {
    let p = req.path.trim_start_matches('/');
    if p.is_empty() {
        return (None, None, 0);
    }
    let (c, rest) = p.split_once('/').unwrap_or((p, ""));
    if !rest.is_empty() {
        return (Some(decode(c)), Some(decode(rest)), 2);
    }
    let restype = req.query.get("restype").map(|s| s.to_ascii_lowercase());
    if !matches!(restype.as_deref(), Some("container") | Some("account")) && req.method != Method::OPTIONS {
        // A path with one segment and no `restype=container` names a blob in `$root`.
        return (Some("$root".to_string()), Some(decode(c)), 2);
    }
    (Some(decode(c)), None, 1)
}

pub(crate) fn check_version(req: &Request) -> Result<(), ApiError> {
    if let Some(v) = req.header("x-ms-version") {
        let ok = v.len() == 10
            && v.chars().enumerate().all(|(i, c)| if i == 4 || i == 7 { c == '-' } else { c.is_ascii_digit() })
            && v[..4].parse::<u32>().map(|y| (2008..2030).contains(&y)).unwrap_or(false);
        if !ok {
            return Err(ApiError::invalid_header("x-ms-version", v));
        }
    }
    Ok(())
}

fn run_inner(st: &mut State, req: &Request, accounts: Option<&StorageAccounts>, prep: &Pre) -> ApiResult {
    let (container, blob, depth) = parse_target(req);
    let op = identify(req, depth);
    check_version(req)?;
    let acct_name = req.account.to_ascii_lowercase();

    let authz = {
        let acct = st.account(&acct_name);
        let c = container.as_deref().and_then(|n| acct.containers.get(n));
        let blob_exists = match (c, blob.as_deref()) {
            (Some(c), Some(b)) => c.blobs.get(b).map(|e| e.live().is_some()).unwrap_or(false),
            _ => false,
        };
        let t = Target {
            container: c,
            public_access_allowed: acct.public_access_allowed,
            blob_exists,
            is_version: req.query.has("versionid"),
            container_name: container.as_deref(),
            blob_name: blob.as_deref(),
            accounts,
        };
        auth::authorize(req, op, &t)?
    };

    // A hierarchical container (Data Lake) first takes in what earlier blob-endpoint writes
    // left without attributes or parent directories.
    let mut adopted = false;
    if let Some(c) = container.as_deref().and_then(|n| st.account(&acct_name).containers.get_mut(n)) {
        if c.hns {
            adopted = c.dirty || !c.touched.is_empty();
            crate::dfs::model::adopt(c, req.ctx.time);
        }
    }
    st.changed |= adopted;

    // In a hierarchical container a blob name follows the Data Lake rules (`names.rs`): no `.` or `..` segments, at most 61
    // segments, and no blob below a file or in place of a directory.
    if let (Some(cn), Some(bn)) = (container.as_deref(), blob.as_deref()) {
        let writes = matches!(op, Op::PutBlob | Op::PutBlockList | Op::CopyBlob | Op::PutBlobFromUrl);
        if writes || matches!(op, Op::PutBlock | Op::PutBlockFromUrl) {
            if let Some(c) = st.account(&acct_name).containers.get(cn).filter(|c| c.hns) {
                crate::names::check(bn, true)?;
                crate::names::check_parents(c, bn, writes)?;
            }
        }
    }

    let pre = if st.events_wanted { events::before_blob(st, &acct_name, op, container.as_deref(), blob.as_deref()) } else { None };
    let mut cx = Cx { req, st, now: req.ctx.time, acct: acct_name, authz, pre: prep };
    let result = dispatch(&mut cx, op, container.as_deref(), blob.as_deref());
    if let Ok(resp) = &result {
        if cx.st.events_wanted {
            let acct = cx.acct.clone();
            events::after_blob(cx.st, req, &acct, op, container.as_deref(), blob.as_deref(), pre, resp);
        }
    }
    // (a query only reads, although it is a POST)
    if !matches!(req.method, Method::GET | Method::HEAD | Method::OPTIONS) && op != Op::QueryBlob {
        // Keep the old versions and soft-deleted snapshots inside their byte limits.
        crate::store::trim(cx.st.account(&cx.acct));
        if let Some(c) = container.as_deref().and_then(|n| cx.st.account(&cx.acct).containers.get_mut(n)) {
            if c.hns {
                match blob.as_deref() {
                    Some(b) => c.touched.push(b.to_string()),
                    None => c.dirty = true,
                }
            }
        }
    }
    result
}

fn is_hns(cx: &mut Cx, container: &str) -> bool {
    cx.account().containers.get(container).map(|c| c.hns).unwrap_or(false)
}

fn dispatch(cx: &mut Cx, op: Op, container: Option<&str>, blob: Option<&str>) -> ApiResult {
    let c = container.unwrap_or("");
    let b = blob.unwrap_or("");
    match op {
        // Preflight: no credentials, judged against the account's CORS rules (shared code in core).
        Op::Options => Ok(storage::cors::preflight(cx.req, &service_props::cors_rules(&cx.account().props))),
        // service
        Op::ListContainers => list::list_containers(cx),
        Op::GetServiceProperties => service_props::get_properties(cx),
        Op::SetServiceProperties => service_props::set_properties(cx),
        Op::GetServiceStats => service_props::get_stats(cx),
        Op::GetUserDelegationKey => service_props::user_delegation_key(cx),
        Op::GetAccountInfo => service_props::account_info(cx),
        Op::FindBlobsAccount => tags::find_blobs(cx, None),
        Op::BatchAccount => batch::run_batch(cx, None),
        // container
        Op::CreateContainer => container::create(cx, c),
        Op::DeleteContainer => container::delete(cx, c),
        Op::GetContainerProperties | Op::GetContainerMetadata => container::get_properties(cx, c),
        Op::SetContainerMetadata => container::set_metadata(cx, c),
        Op::GetContainerAcl => container::get_acl(cx, c),
        Op::SetContainerAcl => container::set_acl(cx, c),
        Op::LeaseContainer => lease::lease_container(cx, c),
        Op::ListBlobs => list::list_blobs(cx, c),
        Op::FindBlobsContainer => tags::find_blobs(cx, Some(c)),
        Op::BatchContainer => batch::run_batch(cx, Some(c)),
        Op::UndeleteContainer => container::undelete(cx, c),
        // blob
        Op::PutBlob => blob::put_blob(cx, c, b),
        Op::PutBlobFromUrl => copy::put_blob_from_url(cx, c, b),
        Op::CopyBlob => copy::copy_blob(cx, c, b),
        Op::AbortCopy => copy::abort_copy(cx, c, b),
        Op::GetBlob => blob::get_blob(cx, c, b, false),
        Op::GetBlobProperties | Op::GetBlobMetadata => blob::get_blob(cx, c, b, true),
        Op::DeleteBlob => blob::delete_blob(cx, c, b),
        Op::UndeleteBlob if cx.req.header("x-ms-undelete-source").is_some() && is_hns(cx, c) => crate::dfs::undelete_path(cx, c, b),
        Op::UndeleteBlob => blob::undelete_blob(cx, c, b),
        Op::SetExpiry if is_hns(cx, c) => crate::dfs::set_expiry(cx, c, b),
        Op::SetBlobProperties => props::set_properties(cx, c, b),
        Op::SetBlobMetadata => props::set_metadata(cx, c, b),
        Op::SnapshotBlob => blob::snapshot_blob(cx, c, b),
        Op::LeaseBlob => lease::lease_blob(cx, c, b),
        Op::SetBlobTier => props::set_tier(cx, c, b),
        Op::GetBlobTags => tags::get_tags(cx, c, b),
        Op::SetBlobTags => tags::set_tags(cx, c, b),
        Op::PutBlock => block::put_block(cx, c, b),
        Op::PutBlockFromUrl => block::put_block_from_url(cx, c, b),
        Op::PutBlockList => block::put_block_list(cx, c, b),
        Op::GetBlockList => block::get_block_list(cx, c, b),
        Op::AppendBlock => append::append_block(cx, c, b),
        Op::AppendBlockFromUrl => append::append_block_from_url(cx, c, b),
        Op::SealBlob => append::seal(cx, c, b),
        Op::PutPage => page::put_page(cx, c, b),
        Op::PutPageFromUrl => page::put_page_from_url(cx, c, b),
        Op::GetPageRanges => page::get_page_ranges(cx, c, b),
        Op::SetImmutabilityPolicy => props::set_immutability(cx, c, b),
        Op::DeleteImmutabilityPolicy => props::delete_immutability(cx, c, b),
        Op::SetLegalHold => props::set_legal_hold(cx, c, b),
        Op::QueryBlob => crate::query::query_blob(cx, c, b, false),
        // not built
        Op::RenameContainer | Op::CreateSession | Op::IncrementalCopy | Op::SetExpiry => Err(ApiError::not_implemented(op.name())),
        Op::Unknown => Err(unknown(cx)),
    }
}

/// A request that matches no operation.
fn unknown(cx: &Cx) -> ApiError {
    let req = cx.req;
    let m = req.method.as_str();
    if !matches!(m, "GET" | "HEAD" | "PUT" | "POST" | "DELETE" | "OPTIONS") {
        return err("UnsupportedHttpVerb");
    }
    let comp = req.query.get("comp").unwrap_or("");
    ApiError::new("NotImplemented")
        .msg(format!(
            "blob: {m} {}{} is not implemented by this emulator.",
            req.decoded_path(),
            if comp.is_empty() { String::new() } else { format!("?comp={comp}") }
        ))
        .unsupported(format!("Blob_Unknown ({m}{})", if comp.is_empty() { String::new() } else { format!(" comp={comp}") }))
}
