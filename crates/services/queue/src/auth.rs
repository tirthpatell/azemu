//! Who may do what.
//!
//! Shared Key and SAS signatures are verified for `devstoreaccount1` (policy in
//! `azemu_core::storage::sig`: placeholders, other accounts and unknown newer layouts
//! are not checked; `AZEMU_LOOSE_AUTH=1` turns it off). What a SAS says about itself is
//! honoured for every account: its start and expiry, the services and resource types of an account SAS, its permissions,
//! its protocol, and (through `si`) the stored access policy of the queue.

use azemu_core::storage::auth::{classify, Auth, SasParams};
use azemu_core::storage::sig::{verify_sas, verify_shared_key, SasTarget, StorageService};
use azemu_core::storage::StorageAccounts;
use azemu_core::time::{parse_iso8601, rfc1123};
use azemu_core::{Request, Response};

use crate::errors;
use crate::routing::Op;
use crate::state::SignedId;

/// What an operation needs from an account SAS or a queue SAS.
struct Need {
    /// Account SAS resource type: `s` service, `c` container (queue), `o` object (messages).
    resource: char,
    /// Account SAS permissions; holding any one of them is enough.
    account_perms: &'static str,
    /// Queue SAS permission (r, a, u, p). `None`: a queue SAS cannot do this.
    queue_perm: Option<char>,
}

fn need(op: Op) -> Need {
    let n = |resource, account_perms, queue_perm| Need { resource, account_perms, queue_perm };
    match op {
        Op::ListQueues => n('s', "l", None),
        Op::GetServiceProperties | Op::GetServiceStats => n('s', "r", None),
        Op::SetServiceProperties => n('s', "w", None),
        Op::GetUserDelegationKey => n('s', "", None),
        Op::CreateQueue => n('c', "wc", None),
        Op::DeleteQueue => n('c', "d", None),
        Op::GetQueueMetadata | Op::GetQueueAcl => n('c', "r", Some('r')),
        Op::SetQueueMetadata | Op::SetQueueAcl => n('c', "w", None),
        Op::PutMessage => n('o', "a", Some('a')),
        Op::GetMessages | Op::DeleteMessage => n('o', "p", Some('p')),
        Op::PeekMessages => n('o', "r", Some('r')),
        Op::ClearMessages => n('o', "d", None),
        Op::UpdateMessage => n('o', "u", Some('u')),
    }
}

/// `si` of the request, if the SAS uses a stored access policy.
pub fn policy_id(req: &Request) -> Option<String> {
    match classify(req) {
        Auth::Sas(p) => p.si.filter(|s| !s.is_empty()),
        _ => None,
    }
}

/// Check the credentials of a request. `policy` is what the queue's stored access
/// policies say about the `si` the SAS names.
pub fn authorize(req: &Request, op: Op, policy: Lookup<'_>, accounts: &StorageAccounts, queue: Option<&str>) -> Result<(), Response> {
    let ctx = &req.ctx;
    match classify(req) {
        Auth::Bearer => Ok(()),
        Auth::SharedKey { .. } => match verify_shared_key(req, StorageService::Queue, accounts).rejection() {
            Some(detail) => Err(errors::authentication_failed(ctx, Some(detail))),
            None => Ok(()),
        },
        Auth::Anonymous => Err(errors::authentication_failed(ctx, None)),
        Auth::Sas(sas) => {
            let target = SasTarget::Queue { queue };
            match verify_sas(req, StorageService::Queue, accounts, &target).rejection() {
                Some(detail) => Err(errors::authentication_failed(ctx, Some(detail))),
                None => check_sas(req, op, sas, policy),
            }
        }
    }
}

/// Result of looking up the stored access policy that a SAS names.
pub enum Lookup<'a> {
    /// The SAS does not use a stored policy.
    NotUsed,
    /// It names one, and the queue is not there.
    NoQueue,
    /// It names one that the queue does not have.
    Missing,
    Found(&'a SignedId),
}

fn check_sas(req: &Request, op: Op, mut sas: SasParams, policy: Lookup<'_>) -> Result<(), Response> {
    let ctx = &req.ctx;
    match &policy {
        Lookup::NotUsed | Lookup::NoQueue => {}
        Lookup::Missing => return Err(errors::authentication_failed(ctx, None)),
        Lookup::Found(p) => {
            let p: &SignedId = p;
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

    let now = ctx.time;
    if sas.expired(now) || sas.not_yet_valid(now) {
        let fmt = |v: &Option<String>| v.as_deref().and_then(parse_iso8601).map(rfc1123).unwrap_or_default();
        let detail = format!(
            "Signature not valid in the specified time frame: Start [{}] - Expiry [{}] - Current [{}]",
            fmt(&sas.st),
            fmt(&sas.se),
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

    // A policy of a queue that does not exist cannot say what is allowed; let the
    // operation answer 404.
    if matches!(policy, Lookup::NoQueue) && sas.sp.is_none() {
        return Ok(());
    }

    let need = need(op);
    let perms = sas.permissions();
    if let Some(ss) = sas.ss.as_deref() {
        // account SAS
        if !ss.contains('q') {
            return Err(errors::service_mismatch(ctx));
        }
        if !sas.srt.as_deref().unwrap_or("").contains(need.resource) {
            return Err(errors::resource_type_mismatch(ctx));
        }
        if !need.account_perms.chars().any(|c| perms.contains(c)) {
            return Err(errors::permission_mismatch(ctx));
        }
    } else {
        // queue SAS
        match need.queue_perm {
            Some(c) if perms.contains(c) => {}
            _ => return Err(errors::permission_mismatch(ctx)),
        }
    }
    Ok(())
}
