//! Authentication for the `dfs` endpoint: the same loose rules as Blob (signatures are
//! checked for `devstoreaccount1` only, SAS content is honoured for every account), with the
//! Data Lake permission letters (`m` move, `e` execute, `o` ownership, `p` permissions) and
//! directory SAS (`sr=d`, `sdd` depth).

use azemu_core::storage::auth::{classify, Auth, SasParams};
use azemu_core::storage::sig::{verify_sas, verify_shared_key, SasTarget, StorageService};
use azemu_core::storage::StorageAccounts;
use azemu_core::Request;

use crate::auth::{auth_failed, check_sas_window, malformed_authorization, no_auth_info, public_access_not_permitted, Authz};
use crate::cx::Cx;
use crate::dfs::ops::Op;
use crate::errors::{err, ApiError};
use crate::ops::{Anon, Level};
use crate::state::Container;

pub struct Target<'a> {
    pub container: Option<&'a Container>,
    /// The account allows public access to containers.
    pub public_access_allowed: bool,
    pub path_exists: bool,
    pub container_name: Option<&'a str>,
    /// Normalized path, `None` for filesystem and account level requests.
    pub path: Option<&'a str>,
    pub accounts: &'a StorageAccounts,
}

/// The first `depth` segments of a path.
fn first_segments(path: &str, depth: usize) -> String {
    path.split('/').take(depth).collect::<Vec<_>>().join("/")
}

pub fn authorize(req: &Request, op: Op, t: &Target<'_>) -> Result<Authz, ApiError> {
    let need = op.need();
    if let Some(e) = malformed_authorization(req) {
        return Err(e);
    }
    match classify(req) {
        Auth::Bearer => Ok(Authz::default()),
        Auth::SharedKey { .. } => {
            if let Some(detail) = verify_shared_key(req, StorageService::Blob, t.accounts).rejection() {
                return Err(auth_failed(detail.to_string()));
            }
            Ok(Authz::default())
        }
        Auth::Sas(p) => {
            let sr = p.sr.as_deref().unwrap_or("");
            let depth: Option<usize> = req.query.get("sdd").and_then(|v| v.parse().ok());
            let signed_path: Option<String> = match (t.path, sr, depth) {
                (Some(path), "d", Some(d)) => Some(first_segments(path, d)),
                (Some(path), _, _) => Some(path.to_string()),
                (None, _, _) => None,
            };
            let target = SasTarget::Blob { container: t.container_name.unwrap_or(""), blob: signed_path.as_deref() };
            if let Some(detail) = verify_sas(req, StorageService::Blob, t.accounts, &target).rejection() {
                return Err(auth_failed(detail.to_string()));
            }
            let perms = check_sas_window(req, &p, t.container)?;
            check_sas(op, &p, perms, t, depth)?;
            Ok(Authz { sas: Some(p) })
        }
        Auth::Anonymous => {
            let public = t.container.and_then(|c| c.public_access.as_deref());
            let ok = match need.anon {
                Anon::No => false,
                Anon::Blob => matches!(public, Some("blob" | "container")),
                Anon::Container => matches!(public, Some("container")),
            };
            if ok {
                Ok(Authz { sas: None })
            } else if need.anon != Anon::No && !t.public_access_allowed {
                Err(public_access_not_permitted())
            } else {
                Err(no_auth_info())
            }
        }
    }
}

fn check_sas(op: Op, p: &SasParams, perms: Option<String>, t: &Target<'_>, depth: Option<usize>) -> Result<(), ApiError> {
    let need = op.need();
    if p.ss.is_some() || p.srt.is_some() {
        if let Some(ss) = &p.ss {
            if !ss.contains('b') {
                return Err(err("AuthorizationServiceMismatch"));
            }
        }
        if let Some(srt) = &p.srt {
            let letter = match need.level {
                Level::Any => None,
                Level::Service => Some('s'),
                Level::Container => Some('c'),
                Level::Object => Some('o'),
            };
            if letter.map(|l| !srt.contains(l)).unwrap_or(false) {
                return Err(err("AuthorizationResourceTypeMismatch"));
            }
        }
    } else if let Some(sr) = &p.sr {
        let ok = match sr.as_str() {
            "c" => need.level != Level::Service,
            "b" | "bs" | "bv" => need.level == Level::Object || need.level == Level::Any,
            "d" => {
                let deep = t.path.map(|x| x.split('/').count()).unwrap_or(0);
                (need.level == Level::Object || need.level == Level::Any) && depth.map(|d| deep >= d).unwrap_or(true)
            }
            _ => true,
        };
        if !ok {
            return Err(err("AuthorizationResourceTypeMismatch"));
        }
    }
    let Some(perms) = perms else { return Ok(()) };
    let mut required = need.perms;
    if !t.path_exists {
        if let Some(n) = need.perms_new {
            required = n;
        }
    }
    if required.is_empty() || required.chars().any(|c| perms.contains(c)) {
        Ok(())
    } else {
        Err(err("AuthorizationPermissionMismatch"))
    }
}

/// A SAS must carry one of `letters` (when it names permissions at all). Used for the
/// finer rules inside an operation: `o` to change the owner, `p` to change permissions.
pub fn require(cx: &Cx, letters: &str) -> Result<(), ApiError> {
    let Some(p) = &cx.authz.sas else { return Ok(()) };
    if p.ss.is_some() || p.srt.is_some() {
        return Ok(());
    }
    match p.sp.as_deref() {
        Some(sp) if !letters.chars().any(|c| sp.contains(c)) => Err(err("AuthorizationPermissionMismatch")),
        _ => Ok(()),
    }
}
