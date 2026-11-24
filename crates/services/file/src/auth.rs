//! Who may do what.
//!
//! The form of a signature is checked for every account and the signature itself for `devstoreaccount1` (policy in
//! `azemu_core::storage::sig`: other accounts and unknown newer layouts are not compared; `AZEMU_LOOSE_AUTH=1` turns it off). What a
//! SAS says about itself is honoured for every account: its start and expiry, the services and resource types of an account SAS,
//! its permissions, its protocol, its source address and (through `si`) the stored access policy of the share. A bearer token is
//! taken as it is, with or without `x-ms-file-request-intent` (Azure answered both the same in November 2026).

use azemu_core::storage::auth::{classify, Auth, SasParams};
use azemu_core::storage::credentials::{bad_authorization, sip_matches, BadAuthorization};
use azemu_core::storage::sig::{verify_sas, verify_shared_key, Rejection, SasTarget, StorageService};
use azemu_core::storage::StorageAccounts;
use azemu_core::time::{parse_iso8601, rfc1123, OffsetDateTime};
use azemu_core::{Request, Response};

use crate::errors;
use crate::routing::Op;
use crate::state::SignedId;

/// What an operation needs from an account SAS or a service SAS.
struct Need {
    /// Account SAS resource type: `s` service, `c` container (share), `o` object (directories and files).
    resource: char,
    /// Account SAS permissions; holding any one of them is enough.
    account_perms: &'static str,
    /// Service SAS permissions (`rcwdl`); holding any one is enough. Empty: a service SAS cannot do this.
    service_perms: &'static str,
    /// A service SAS for one file (`sr=f`) can do this.
    on_file: bool,
}

fn need(op: Op) -> Need {
    let n = |resource, account_perms, service_perms, on_file| Need { resource, account_perms, service_perms, on_file };
    match op {
        Op::ListShares => n('s', "l", "", false),
        Op::GetServiceProperties => n('s', "r", "", false),
        Op::SetServiceProperties => n('s', "w", "", false),
        Op::GetUserDelegationKey | Op::NotBuilt(_) => n('s', "", "", false),
        Op::CreateShare | Op::RestoreShare => n('c', "cw", "", false),
        Op::DeleteShare => n('c', "d", "", false),
        Op::GetShareProperties | Op::GetShareMetadata | Op::GetShareAcl | Op::GetShareStats | Op::GetPermission => n('c', "r", "", false),
        Op::SetShareProperties | Op::SetShareMetadata | Op::SetShareAcl | Op::LeaseShare | Op::CreatePermission => n('c', "w", "", false),
        Op::CreateShareSnapshot => n('c', "cw", "", false),
        Op::CreateDirectory | Op::RenameDirectory => n('o', "cw", "cw", false),
        Op::GetDirectoryProperties | Op::GetDirectoryMetadata => n('o', "r", "r", false),
        Op::DeleteDirectory => n('o', "d", "d", false),
        Op::SetDirectoryProperties | Op::SetDirectoryMetadata => n('o', "w", "w", false),
        Op::ListFilesAndDirectories | Op::ListHandles => n('o', "l", "l", false),
        Op::ForceCloseHandles => n('o', "w", "w", true),
        Op::CreateFile | Op::RenameFile => n('o', "cw", "cw", true),
        Op::GetFile | Op::GetFileMetadata | Op::ListRanges => n('o', "r", "r", true),
        Op::DeleteFile => n('o', "d", "d", true),
        Op::SetFileProperties | Op::SetFileMetadata | Op::PutRange | Op::PutRangeFromUrl | Op::LeaseFile | Op::AbortCopy => {
            n('o', "w", "w", true)
        }
        Op::CopyFile => n('o', "cw", "cw", true),
    }
}

/// The stored access policy a SAS names (`si`), looked up in the share's.
pub enum Lookup<'a> {
    /// The SAS does not use a stored policy.
    NotUsed,
    /// It names one, and the share is not there.
    NoShare,
    /// It names one that the share does not have.
    Missing,
    Found(&'a SignedId),
}

/// `si` of the request, if the SAS uses a stored policy.
pub fn policy_id(req: &Request) -> Option<String> {
    match classify(req) {
        Auth::Sas(p) => p.si.filter(|s| !s.is_empty()),
        _ => None,
    }
}

/// Check the credentials of a request. `file` is the path inside the share the SAS must cover.
pub fn authorize(
    req: &Request,
    op: Op,
    policy: Lookup<'_>,
    accounts: &StorageAccounts,
    share: Option<&str>,
    path: &[String],
) -> Result<(), Response> {
    let ctx = &req.ctx;
    let rejected = |why: Rejection<'_>| match why {
        Rejection::AuthenticationFailed(detail) => errors::authentication_failed(ctx, Some(detail)),
        Rejection::InvalidAuthenticationInfo => errors::invalid_authentication_info(ctx),
    };
    match bad_authorization(req) {
        Some(BadAuthorization::Format) => return Err(errors::invalid_authentication_info(ctx)),
        Some(BadAuthorization::UnknownScheme(scheme)) => {
            return Err(errors::authentication_failed(ctx, Some(&format!("Authentication scheme {scheme} is not supported"))))
        }
        None => {}
    }
    match classify(req) {
        Auth::Bearer => Ok(()),
        Auth::SharedKey { .. } => match verify_shared_key(req, StorageService::File, accounts).rejection() {
            Some(why) => Err(rejected(why)),
            None => Ok(()),
        },
        Auth::Anonymous => Err(errors::no_credentials(req)),
        Auth::Sas(sas) => {
            let joined = path.join("/");
            let target = SasTarget::File { share: share.unwrap_or(""), path: (!path.is_empty()).then_some(joined.as_str()) };
            match verify_sas(req, StorageService::File, accounts, &target).rejection() {
                Some(why) => Err(rejected(why)),
                None => check_sas(req, op, sas, policy),
            }
        }
    }
}

fn check_sas(req: &Request, op: Op, mut sas: SasParams, policy: Lookup<'_>) -> Result<(), Response> {
    let ctx = &req.ctx;
    match &policy {
        Lookup::NotUsed | Lookup::NoShare => {}
        Lookup::Missing => return Err(errors::authentication_failed(ctx, None)),
        Lookup::Found(p) => {
            // The policy supplies whatever the token leaves out.
            if sas.sp.is_none() {
                sas.sp = p.permission.clone();
            }
            if sas.st.is_none() {
                sas.st = p.start.clone();
            }
            if sas.se.is_none() {
                sas.se = p.expiry.clone();
            }
        }
    }

    let has_policy = sas.si.as_deref().is_some_and(|s| !s.is_empty());
    if sas.se.is_none() && !has_policy {
        return Err(errors::authentication_failed(ctx, None));
    }

    // A start that is not given is the time of the request. Azure refuses an expiry that is not after the start before it asks
    // whether the SAS is valid now.
    let now = ctx.time;
    let at = |v: &Option<String>| v.as_deref().and_then(parse_iso8601);
    let (start, expiry) = (at(&sas.st), at(&sas.se));
    if let Some(e) = expiry {
        let from = start.unwrap_or(now);
        if e <= from {
            let detail = format!("Signed expiry time [{}] has to be after signed start time [{}]", rfc1123(e), rfc1123(from));
            return Err(errors::authentication_failed(ctx, Some(&detail)));
        }
    }
    if sas.expired(now) || sas.not_yet_valid(now) {
        let fmt = |t: Option<OffsetDateTime>| t.map(rfc1123).unwrap_or_default();
        let detail = format!(
            "Signature not valid in the specified time frame: Start [{}] - Expiry [{}] - Current [{}]",
            fmt(start),
            fmt(expiry),
            rfc1123(now)
        );
        return Err(errors::authentication_failed(ctx, Some(&detail)));
    }

    if let Some(spr) = sas.spr.as_deref() {
        let https_only = spr.split(',').map(str::trim).all(|p| p.eq_ignore_ascii_case("https"));
        if https_only && ctx.endpoint.scheme != "https" {
            return Err(errors::protocol_mismatch(ctx));
        }
    }

    if let Some(sip) = sas.sip.as_deref().filter(|s| !s.is_empty()) {
        match sip_matches(req, sip) {
            Ok(true) => {}
            Ok(false) => return Err(errors::authorization_failure(ctx)),
            Err(detail) => return Err(errors::authentication_failed(ctx, Some(detail))),
        }
    }

    // A policy of a share that does not exist cannot say what is allowed; let the operation answer 404.
    if matches!(policy, Lookup::NoShare) && sas.sp.is_none() {
        return Ok(());
    }

    let need = need(op);
    let perms = sas.permissions();
    if let Some(ss) = sas.ss.as_deref() {
        // account SAS
        if !ss.contains('f') {
            return Err(errors::service_mismatch(ctx));
        }
        if !sas.srt.as_deref().unwrap_or("").contains(need.resource) {
            return Err(errors::resource_type_mismatch_auth(ctx));
        }
        if !need.account_perms.chars().any(|c| perms.contains(c)) {
            return Err(errors::permission_mismatch(ctx));
        }
    } else {
        // service SAS: `sr=s` for a share, `sr=f` for one file
        if need.service_perms.is_empty() {
            return Err(errors::permission_mismatch(ctx));
        }
        if sas.sr.as_deref() == Some("f") && !need.on_file {
            return Err(errors::permission_mismatch(ctx));
        }
        if !need.service_perms.chars().any(|c| perms.contains(c)) {
            return Err(errors::permission_mismatch(ctx));
        }
    }
    Ok(())
}
