//! Who may do what. The form of a signature is checked for every account and the signature itself for
//! `devstoreaccount1` (see `azemu_core::storage::sig` for the policy: other accounts and
//! unknown newer layouts are not compared; `AZEMU_LOOSE_AUTH=1` turns it off). A SAS is
//! also honoured for what it says, for every account: expiry, start, protocol, IP
//! range, services, resource types, permissions and stored access policy. A request
//! with no credentials is anonymous and only allowed on containers and blobs with
//! public access (`check_anonymous` says what each refusal is).

use azemu_core::resp::Resp;
use azemu_core::storage::auth::{classify, Auth, SasParams};
use azemu_core::storage::credentials::{
    bad_authorization, sip_matches, without_credentials, BadAuthorization, Flavor, NoCredentials, CHALLENGE,
};
use azemu_core::storage::sig::{verify_sas, verify_shared_key, Rejection, SasTarget, StorageService};
use azemu_core::storage::StorageAccounts;
use azemu_core::time::{self, parse_iso8601};
use azemu_core::Request;

use crate::errors::{err, ApiError};
use crate::ops::{Anon, Level, Need, Op};
use crate::state::{Container, Time};

/// What the request was authorized as.
#[derive(Default, Clone)]
pub struct Authz {
    /// SAS fields when a SAS was used (response header overrides, scope).
    pub sas: Option<SasParams>,
}

/// Facts about the target the check needs.
pub struct Target<'a> {
    pub container: Option<&'a Container>,
    /// The account allows public access to containers.
    pub public_access_allowed: bool,
    /// The blob exists (live) - decides between "create" and "write" permissions.
    pub blob_exists: bool,
    pub is_version: bool,
    /// Container and blob the URL names (percent-decoded), for the signed resource.
    pub container_name: Option<&'a str>,
    pub blob_name: Option<&'a str>,
    /// Where to look up account keys; `None` skips signature checks (batch sub-requests).
    pub accounts: Option<&'a StorageAccounts>,
}

pub fn authorize(req: &Request, op: Op, t: &Target<'_>) -> Result<Authz, ApiError> {
    if op == Op::Options {
        return Ok(Authz::default());
    }
    let need = op.need();
    if let Some(e) = malformed_authorization(req) {
        return Err(e);
    }
    match classify(req) {
        Auth::Bearer => Ok(Authz::default()),
        Auth::SharedKey { .. } => {
            if let Some(accounts) = t.accounts {
                if let Some(why) = verify_shared_key(req, StorageService::Blob, accounts).rejection() {
                    return Err(rejected(why));
                }
            }
            Ok(Authz::default())
        }
        Auth::Sas(p) => {
            if let Some(accounts) = t.accounts {
                // A directory SAS (`sr=d`) is signed for the directory: the first `sdd` segments.
                let dir_path: Option<String> =
                    match (p.sr.as_deref(), t.blob_name, req.query.get("sdd").and_then(|v| v.parse::<usize>().ok())) {
                        (Some("d"), Some(b), Some(d)) => Some(b.split('/').take(d).collect::<Vec<_>>().join("/")),
                        _ => None,
                    };
                let target = SasTarget::Blob { container: t.container_name.unwrap_or(""), blob: dir_path.as_deref().or(t.blob_name) };
                if let Some(why) = verify_sas(req, StorageService::Blob, accounts, &target).rejection() {
                    return Err(rejected(why));
                }
            }
            check_sas(req, op, &need, &p, t)?;
            Ok(Authz { sas: Some(p) })
        }
        Auth::Anonymous => {
            check_anonymous(req, &need, t)?;
            Ok(Authz { sas: None })
        }
    }
}

/// What Azure answers a request with no credential, or with one it cannot use at all: 401 `NoAuthenticationInformation`
/// with the challenge; for an `x-ms-version` from before Entra ID could sign in (older than 2019-12-12) 404
/// `ResourceNotFound`, so that an anonymous caller learns nothing about what exists.
pub fn no_auth_info(req: &Request) -> ApiError {
    match without_credentials(req, Flavor::Blob) {
        NoCredentials::Challenge => ApiError::new("NoAuthenticationInformation").header("WWW-Authenticate", CHALLENGE),
        NoCredentials::NotFound => err("ResourceNotFound"),
    }
}

/// What Azure answers a request with no credential (checked in November 2026 against a general-purpose v2 account that
/// allows public access). Where the request is of a kind that is never allowed without one (everything but reading blobs,
/// and listing and reading containers) it is `no_auth_info`, whatever the container is. For the readings, an account that
/// disallows public access answers 409 `PublicAccessNotPermitted`; a container that is not public, or not public enough
/// for this operation, or that does not exist, is `no_auth_info`: an anonymous request is not told whether it exists.
fn check_anonymous(req: &Request, need: &Need, t: &Target<'_>) -> Result<(), ApiError> {
    if matches!(need.anon, Anon::No) {
        return Err(no_auth_info(req));
    }
    if !t.public_access_allowed {
        return Err(public_access_not_permitted());
    }
    let public = t.container.and_then(|c| c.public_access.as_deref());
    let ok = match need.anon {
        Anon::No => false,
        Anon::Blob => matches!(public, Some("blob" | "container")),
        Anon::Container => matches!(public, Some("container")),
    };
    if ok {
        return Ok(());
    }
    Err(no_auth_info(req))
}

/// 409: the account does not allow public access, so an anonymous request cannot be served.
pub fn public_access_not_permitted() -> ApiError {
    ApiError::new("PublicAccessNotPermitted")
}

/// An `Authorization` header that is there and that no scheme azemu knows can be read from. One word only (or a `Bearer`
/// with no token): 400 `InvalidAuthenticationInfo` ("Authentication information is not given in the correct format.
/// Check the value of Authorization header."). A scheme Azure does not know (`Foo bar`): 403 `AuthenticationFailed`.
/// An empty header counts as none.
pub fn malformed_authorization(req: &Request) -> Option<ApiError> {
    Some(match bad_authorization(req)? {
        BadAuthorization::Format => ApiError::new("InvalidAuthenticationInfo"),
        BadAuthorization::UnknownScheme(_) => ApiError::new("AuthenticationFailed"),
    })
}

/// How a refused credential is answered.
pub fn rejected(why: Rejection<'_>) -> ApiError {
    match why {
        Rejection::AuthenticationFailed(detail) => auth_failed(detail.to_string()),
        Rejection::InvalidAuthenticationInfo => ApiError::new("InvalidAuthenticationInfo"),
    }
}

pub fn auth_failed(detail: String) -> ApiError {
    ApiError::new("AuthenticationFailed").detail("AuthenticationErrorDetail", detail)
}

fn parse_t(s: &Option<String>) -> Option<Time> {
    s.as_deref().and_then(parse_iso8601)
}

/// Everything about a SAS that does not depend on the operation: the time window, the stored
/// access policy, protocol and source IP. Returns the permissions it grants (`sp`, or the
/// policy's), `None` when it names none.
pub fn check_sas_window(req: &Request, p: &SasParams, container: Option<&Container>) -> Result<Option<String>, ApiError> {
    let now = req.ctx.time;

    // Stored access policy: supplies permissions and, when the SAS has none, the times.
    let mut perms: Option<String> = p.sp.clone();
    let mut start = parse_t(&p.st);
    let mut expiry = parse_t(&p.se);
    if let Some(si) = &p.si {
        let Some(c) = container else {
            return Err(auth_failed(format!("Signed identifier {si} cannot be used on this resource.")));
        };
        let Some(pol) = c.policies.iter().find(|x| &x.id == si) else {
            return Err(auth_failed(format!("Signed identifier not found: {si}")));
        };
        if perms.is_none() {
            perms = pol.permission.clone();
        }
        if start.is_none() {
            start = pol.start.as_deref().and_then(parse_iso8601);
        }
        if expiry.is_none() {
            expiry = pol.expiry.as_deref().and_then(parse_iso8601);
        }
    }

    // A start that is not given is the time of the request. Azure refuses an expiry that is not after the start before it
    // asks whether the SAS is valid now, and says so in other words for an account SAS ("has to") and a service SAS ("must").
    let frame = |s: Option<Time>, e: Option<Time>| {
        auth_failed(format!(
            "Signature not valid in the specified time frame: Start [{}] - Expiry [{}] - Current [{}]",
            s.map(time::rfc1123).unwrap_or_default(),
            e.map(time::rfc1123).unwrap_or_default(),
            time::rfc1123(now)
        ))
    };
    if let Some(e) = expiry {
        let from = start.unwrap_or(now);
        if e <= from {
            let word = if p.ss.is_some() || p.srt.is_some() { "has to" } else { "must" };
            return Err(auth_failed(format!(
                "Signed expiry time [{}] {word} be after signed start time [{}]",
                time::rfc1123(e),
                time::rfc1123(from)
            )));
        }
    }
    if start.is_some_and(|s| s > now) || expiry.is_some_and(|e| e <= now) {
        return Err(frame(start, expiry));
    }

    if let Some(spr) = &p.spr {
        if spr.eq_ignore_ascii_case("https") && req.ctx.endpoint.scheme != "https" {
            return Err(err("AuthorizationProtocolMismatch"));
        }
    }
    if let Some(sip) = p.sip.as_deref().filter(|s| !s.is_empty()) {
        match sip_matches(req, sip) {
            Ok(true) => {}
            Ok(false) => return Err(err("AuthorizationFailure")),
            Err(detail) => return Err(auth_failed(detail.to_string())),
        }
    }
    Ok(perms)
}

fn check_sas(req: &Request, op: Op, need: &Need, p: &SasParams, t: &Target<'_>) -> Result<(), ApiError> {
    let perms = check_sas_window(req, p, t.container)?;

    let account_sas = p.ss.is_some() || p.srt.is_some();
    if account_sas {
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
            "d" => need.level == Level::Object,
            _ => true,
        };
        if !ok {
            return Err(err("AuthorizationResourceTypeMismatch"));
        }
    }

    // A SAS without permissions (and no policy) is treated as unrestricted.
    let Some(perms) = perms else { return Ok(()) };
    let mut required = need.perms;
    if !t.blob_exists {
        if let Some(n) = need.perms_new {
            required = n;
        }
    }
    if op == Op::DeleteBlob && t.is_version {
        required = "x";
    }
    if required.is_empty() || required.chars().any(|c| perms.contains(c)) {
        return Ok(());
    }
    Err(err("AuthorizationPermissionMismatch"))
}

/// Response-header overrides carried by a SAS (`rscc`, `rscd`, ...).
pub fn apply_sas_overrides(mut r: Resp, authz: &Authz) -> Resp {
    if let Some(p) = &authz.sas {
        for (v, h) in [
            (&p.rsct, "Content-Type"),
            (&p.rscd, "Content-Disposition"),
            (&p.rscc, "Cache-Control"),
            (&p.rscl, "Content-Language"),
            (&p.rsce, "Content-Encoding"),
        ] {
            if let Some(v) = v {
                if !v.is_empty() {
                    r = r.header(h, v);
                }
            }
        }
    }
    r
}
