//! Strict authorisation for Blob and Data Lake (`docs/strict-auth.md`): which data action an operation needs.
//!
//! The table follows Microsoft's "Authorize with Microsoft Entra ID (REST API), Permissions for calling data operations". Where what Azure
//! did in November 2026 differs from that table, what Azure did wins (and the difference is in the docs): a Data Reader may list with every
//! `include` but `tags`, may get a user delegation key and list containers (`actions` of the role, not data actions), a blob batch is accepted
//! and each part is judged alone.

use azemu_core::auth::{Auth, Need, Principal, Resource};
use azemu_core::storage::strict::{self, Kind, Refusal};
use azemu_core::{Request, Response};

use crate::errors::ApiError;
use crate::ops::Op;
use crate::router;

const SVC: &str = "Microsoft.Storage/storageAccounts/blobServices";
const BLOBS: &str = "Microsoft.Storage/storageAccounts/blobServices/containers/blobs";
const CONTAINERS: &str = "Microsoft.Storage/storageAccounts/blobServices/containers";

fn blobs(a: &str) -> Need {
    Need::data(&format!("{BLOBS}/{a}"))
}
fn containers(a: &str) -> Need {
    Need::control(&format!("{CONTAINERS}/{a}"))
}
fn write_or_add() -> Need {
    Need::any(vec![blobs("write"), blobs("add/action")])
}

/// What an operation needs, on which resource, and which refusal code goes with it. `None`: nothing is checked (a preflight, an operation
/// the service does not know, a batch, which is judged part by part).
pub fn need_of(op: Op, req: &Request) -> Option<(Need, Resource, Refusal)> {
    let acct = req.account.to_ascii_lowercase();
    let (container, _blob, _) = router::target_of(req);
    let c = container.unwrap_or_default();
    let on_account = Resource::storage(&acct);
    let on_container = Resource::container(&acct, &c);
    let m = Refusal::Mismatch;
    Some(match op {
        Op::Options | Op::Unknown | Op::BatchAccount | Op::BatchContainer => return None,
        // service
        Op::ListContainers => (containers("read"), on_account, m),
        Op::GetServiceProperties | Op::GetServiceStats => (Need::control(&format!("{SVC}/read")), on_account, m),
        Op::SetServiceProperties => (Need::control(&format!("{SVC}/write")), on_account, m),
        Op::GetUserDelegationKey => (Need::control(&format!("{SVC}/generateUserDelegationKey/action")), on_account, m),
        Op::GetAccountInfo => (Need::control(&format!("{SVC}/getInfo/action")), on_account, m),
        Op::FindBlobsAccount => (blobs("filter/action"), on_account, m),
        // container
        Op::CreateContainer => (containers("write"), on_container, Refusal::Failure),
        Op::DeleteContainer => (containers("delete"), on_container, m),
        Op::GetContainerProperties | Op::GetContainerMetadata | Op::CreateSession => (containers("read"), on_container, m),
        Op::SetContainerMetadata | Op::LeaseContainer | Op::UndeleteContainer | Op::RenameContainer | Op::SetLegalHold => {
            (containers("write"), on_container, m)
        }
        Op::GetContainerAcl => (containers("getAcl/action"), on_container, m),
        Op::SetContainerAcl => (containers("setAcl/action"), on_container, m),
        Op::ListBlobs => {
            let tags = req.query.get("include").is_some_and(|i| i.split(',').any(|p| p.trim().eq_ignore_ascii_case("tags")));
            let need = if tags { Need::all(vec![blobs("read"), blobs("tags/read")]) } else { blobs("read") };
            (need, on_container, m)
        }
        Op::FindBlobsContainer => (blobs("filter/action"), on_container, m),
        // blob
        Op::PutBlob | Op::PutBlobFromUrl | Op::CopyBlob | Op::SnapshotBlob | Op::AppendBlock | Op::AppendBlockFromUrl => {
            (write_or_add(), on_container, m)
        }
        Op::PutBlock
        | Op::PutBlockFromUrl
        | Op::PutBlockList
        | Op::PutPage
        | Op::PutPageFromUrl
        | Op::AbortCopy
        | Op::IncrementalCopy
        | Op::SetBlobProperties
        | Op::SetBlobMetadata
        | Op::LeaseBlob
        | Op::SetBlobTier
        | Op::SealBlob
        | Op::SetExpiry => (blobs("write"), on_container, m),
        Op::GetBlob | Op::GetBlobProperties | Op::GetBlobMetadata | Op::GetBlockList | Op::GetPageRanges | Op::QueryBlob => {
            (blobs("read"), on_container, m)
        }
        Op::DeleteBlob => (blobs("delete"), on_container, m),
        Op::UndeleteBlob => (containers("write"), on_container, m),
        Op::GetBlobTags => (blobs("tags/read"), on_container, m),
        Op::SetBlobTags => (blobs("tags/write"), on_container, m),
        Op::SetImmutabilityPolicy | Op::DeleteImmutabilityPolicy => (blobs("immutableStorage/runAsSuperUser/action"), on_container, m),
    })
}

/// What a request that carries a bearer token is told before anything else looks at it: the answer when it is refused, the principal when it
/// is allowed (a batch carries it on to its parts).
pub enum Gate {
    /// No bearer token, or strict mode is off: as before.
    Skip,
    Refused(Box<Response>),
    Allowed(Box<Principal>),
}

pub fn gate(auth: &Auth, req: &Request, op: Op) -> Gate {
    if !auth.applies(req) {
        return Gate::Skip;
    }
    let account = req.account.to_ascii_lowercase();
    let who = match strict::authenticate(auth, req, Kind::Blob, &account) {
        Ok(w) => w,
        Err(r) => return Gate::Refused(Box::new(r)),
    };
    // Set Blob Service Properties reads its body first: a body that is not a valid document is 400 even for a principal with no role.
    if op == Op::SetServiceProperties {
        let valid = std::str::from_utf8(&req.body)
            .ok()
            .and_then(|t| crate::xml::parse(t).ok())
            .is_some_and(|n| n.name == "StorageServiceProperties");
        if !valid {
            return Gate::Allowed(Box::new(who));
        }
    }
    if let Some((need, res, which)) = need_of(op, req) {
        if let Some(r) = strict::authorize(auth, req, Kind::Blob, &who, &need, &res, which) {
            return Gate::Refused(Box::new(r));
        }
    }
    // what reads a source needs the right to read it
    if let Some(r) = check_source(auth, req, op, &who, &account) {
        return Gate::Refused(Box::new(r));
    }
    Gate::Allowed(Box::new(who))
}

/// Copy Blob, Put Blob From URL, Put Block / Append Block / Put Page From URL and Incremental Copy read a blob that the request names in
/// `x-ms-copy-source`. Microsoft's table of permissions asks for `blobs/read` on that source as well as the write on the destination.
fn reads_a_source(op: Op) -> bool {
    matches!(
        op,
        Op::CopyBlob | Op::PutBlobFromUrl | Op::PutBlockFromUrl | Op::AppendBlockFromUrl | Op::PutPageFromUrl | Op::IncrementalCopy
    )
}

/// The source of a copy, judged for the one who reads it. `Some(answer)` when the right to read it is missing.
///
/// The source is resolved the way the copy resolves it (`copy::parse_url`), not by a second reading of the URL: the host with
/// `-secondary`, an escaped or upper case account and the path style all name the same blob for the copy, so they are the same source here.
/// Whose right it is:
/// * a source with a SAS (`sig`) is read with what the SAS says (a SAS is read for its form and its terms, as for any request);
/// * `x-ms-copy-source-authorization: Bearer <token>`: the principal of that token, which must verify for Storage and have the right on
///   the source;
/// * a source in the account of the request: the principal of the request;
/// * a source in another account with neither: public access decides, as it does without strict mode.
///
/// What Azure answers for a caller that may not read the source was not probed; the answers are the documented `CannotVerifyCopySource`
/// with the status and code of the source's own refusal in its details (`copy::cannot_verify`).
fn check_source(auth: &Auth, req: &Request, op: Op, who: &Principal, account: &str) -> Option<Response> {
    if !reads_a_source(op) {
        return None;
    }
    // a source that cannot be read as an address is the copy's own 400, answered after the credentials are known good
    let src = crate::copy::parse_url(req.header("x-ms-copy-source")?)?;
    if src.query.has("sig") {
        return None;
    }
    let refuse = |status: u16, code: &str, msg: &str| Some(crate::copy::cannot_verify(status, code, msg).into_response(&req.ctx));
    let on_source = Resource::container(&src.account, &src.container);
    let what = format!("{} {} (the source)", req.method, req.decoded_path());
    let may_read = |p: &Principal| auth.authorize(&what, p, &blobs("read"), &on_source).is_ok();
    const MISMATCH: &str = "This request is not authorized to perform this operation using this permission.";
    if let Some(h) = req.header("x-ms-copy-source-authorization") {
        let token = match h.trim().split_once(' ') {
            Some((scheme, t)) if scheme.eq_ignore_ascii_case("bearer") && !t.trim().is_empty() => t.trim(),
            // not repeated: it is a credential
            _ => return Some(ApiError::invalid_header("x-ms-copy-source-authorization", "(not a Bearer token)").into_response(&req.ctx)),
        };
        return match auth.authenticate_token(token, req.ctx.time.unix_timestamp(), &strict::audiences(Kind::Blob, &src.account)) {
            Ok(p) if may_read(&p) => None,
            Ok(_) => refuse(403, "AuthorizationPermissionMismatch", MISMATCH),
            Err(_) => refuse(
                401,
                "InvalidAuthenticationInfo",
                "Server failed to authenticate the request. Please refer to the information in the www-authenticate header.",
            ),
        };
    }
    if src.account == account && !may_read(who) {
        return refuse(403, "AuthorizationPermissionMismatch", MISMATCH);
    }
    None
}

/// A part of a blob batch: what it needs, judged with the principal of the outer request. `Some(answer)` when it is refused.
pub fn check_part(auth: &Auth, who: &Principal, part: &Request) -> Option<Response> {
    let op = router::op_of(part);
    let (need, res, which) = need_of(op, part)?;
    strict::authorize(auth, part, Kind::Blob, who, &need, &res, which)
}

// -- Data Lake ---------------------------------------------------------------------------------------------------------

use crate::dfs::ops::Op as DOp;

/// What a Data Lake operation needs (the same actions as Blob: a file system is a container, a path is a blob).
pub fn dfs_need_of(op: DOp, req: &Request) -> Option<(Need, Resource, Refusal)> {
    let acct = req.account.to_ascii_lowercase();
    let fs = req.path.trim_start_matches('/').split('/').next().map(router::decode).unwrap_or_default();
    let on_account = Resource::storage(&acct);
    let on_fs = Resource::container(&acct, &fs);
    let m = Refusal::Mismatch;
    Some(match op {
        DOp::Unknown => return None,
        DOp::AccountInfo => (Need::control(&format!("{SVC}/getInfo/action")), on_account, m),
        DOp::ListFileSystems => (containers("read"), on_account, m),
        DOp::CreateFileSystem | DOp::SetFileSystemProperties => (containers("write"), on_fs, m),
        DOp::GetFileSystemProperties => (containers("read"), on_fs, m),
        DOp::DeleteFileSystem => (containers("delete"), on_fs, m),
        DOp::ListPaths | DOp::ReadPath | DOp::GetPathProperties => (blobs("read"), on_fs, m),
        DOp::CreatePath { .. } | DOp::Append | DOp::Flush | DOp::SetProperties | DOp::LeasePath | DOp::SetExpiry => {
            (blobs("write"), on_fs, m)
        }
        DOp::RenamePath => (blobs("move/action"), on_fs, m),
        DOp::DeletePath => (blobs("delete"), on_fs, m),
        DOp::SetAccessControl | DOp::SetAccessControlRecursive => (blobs("modifyPermissions/action"), on_fs, m),
        DOp::Undelete => (containers("write"), on_fs, m),
    })
}

pub fn dfs_gate(auth: &Auth, req: &Request, op: DOp) -> Gate {
    if !auth.applies(req) {
        return Gate::Skip;
    }
    let account = req.account.to_ascii_lowercase();
    let who = match strict::authenticate(auth, req, Kind::DataLake, &account) {
        Ok(w) => w,
        Err(r) => return Gate::Refused(Box::new(r)),
    };
    if let Some((need, res, which)) = dfs_need_of(op, req) {
        if let Some(r) = strict::authorize(auth, req, Kind::DataLake, &who, &need, &res, which) {
            return Gate::Refused(Box::new(r));
        }
    }
    // a rename takes the path away from where it was: the source is judged too, whichever file system it is in (the one of the request
    // is judged above, for the destination). A source that cannot be read as an address is the rename's own 400.
    // (a source that carries a SAS is read with what the SAS says, as a copy source is)
    let raw_source = req.header("x-ms-rename-source").unwrap_or("");
    let source_sas = raw_source.split_once('?').is_some_and(|(_, q)| azemu_core::Query::parse(q).has("sig"));
    if op == DOp::RenamePath && !source_sas {
        if let Ok((fs, _)) = crate::dfs::rename_source(raw_source) {
            let need = Need::all(vec![blobs("move/action"), blobs("read")]);
            if let Some(r) =
                strict::authorize(auth, req, Kind::DataLake, &who, &need, &Resource::container(&account, &fs), Refusal::Mismatch)
            {
                return Gate::Refused(Box::new(r));
            }
        }
    }
    Gate::Allowed(Box::new(who))
}
