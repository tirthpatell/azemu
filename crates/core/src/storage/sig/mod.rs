//! Request signature checking for Blob and Queue: Shared Key, Shared Key Lite, account
//! SAS, service SAS and user delegation SAS. Table uses the Shared Key check
//! ([`verify_shared_key_table`], its own string to sign); its SAS check is in the table crate.
//!
//! Policy (the same for the three services):
//!
//! - What needs no key is checked for every account name, as Azure does (the rules are in
//!   `credentials.rs`): a Shared Key header whose signature is not base64 is 400
//!   `InvalidAuthenticationInfo`; a SAS `sig` that is empty, not base64 or not 32 bytes, and an `sv`
//!   Azure would not take, are 403 `AuthenticationFailed` with Azure's detail text; the date of a
//!   Shared Key request may not be more than 15 minutes old.
//! - The signature itself (the MAC) is compared only for accounts whose key a client can know:
//!   [`DEV_ACCOUNT`] (key from [`StorageAccounts`]). For any other account name the key is
//!   invented by azemu, so a well-formed signature is accepted.
//! - A SAS signed with a version newer than [`NEWEST_KNOWN_VERSION`] is checked with the
//!   newest known layout and refused when that does not match (the message says the version is
//!   newer than the layouts azemu knows, and names `AZEMU_LOOSE_AUTH`). A SAS whose layout is
//!   not certain (user delegation SAS before 2020-02-10, signed request headers and query
//!   parameters, `sv` that is not a date) is checked and accepted (with a debug log) when
//!   that does not match.
//! - `AZEMU_LOOSE_AUTH=1` (read once) turns all of this off: no signature is looked at, whatever its
//!   form, and the date is not checked. It is for recorded traffic (placeholder signatures such as
//!   `replay` and `Sanitized`, old dates) and for tools whose signatures azemu cannot reproduce. A
//!   warning at start says so.
//!
//! Expiry, permission, resource type and stored policy checks are not here: services
//! keep doing those for every account.

mod hmac;
mod sas;
mod shared_key;
mod udk;

#[cfg(test)]
mod vectors;

use std::sync::OnceLock;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use super::credentials::{date_problem, decode_header_signature, sas_signature, sas_version_ok};
use super::{StorageAccounts, DEV_ACCOUNT};
use crate::Request;

pub use hmac::hmac_sha256;
pub use sas::{
    account_sas_string_to_sign, canonical_resource, is_version, service_sas_string_to_sign, user_delegation_sas_strings, SasFields,
    SasTarget, NEWEST_KNOWN_VERSION,
};
pub use shared_key::{candidates as shared_key_strings_to_sign, parse_authorization, table_candidates as table_shared_key_strings_to_sign};
pub use udk::{user_delegation_key, user_delegation_key_b64};

/// The storage service a request is for. Decides the canonicalized resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageService {
    Blob,
    Queue,
    File,
}

impl StorageService {
    /// `blob` or `queue`.
    pub fn name(self) -> &'static str {
        match self {
            StorageService::Blob => "blob",
            StorageService::Queue => "queue",
            StorageService::File => "file",
        }
    }
}

/// Why a signature was not compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// `AZEMU_LOOSE_AUTH=1`.
    Loose,
    /// The account's key is invented by azemu.
    UnknownAccount,
    /// Well formed and different, but made with a version or layout we cannot be sure of.
    Unverifiable,
}

/// Outcome of a signature check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// The signature matches.
    Valid,
    /// Not compared; the request goes on.
    Skipped(Skip),
    /// 403 `AuthenticationFailed`. The text is the `AuthenticationErrorDetail` Azure sends.
    Invalid(String),
    /// 400 `InvalidAuthenticationInfo`: the `Authorization` header of a Shared Key request cannot be read.
    BadFormat,
}

/// How a failed check is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection<'a> {
    /// 403 `AuthenticationFailed` with this `AuthenticationErrorDetail`.
    AuthenticationFailed(&'a str),
    /// 400 `InvalidAuthenticationInfo`.
    InvalidAuthenticationInfo,
}

impl Check {
    /// How to answer when the request must be rejected.
    pub fn rejection(&self) -> Option<Rejection<'_>> {
        match self {
            Check::Invalid(d) => Some(Rejection::AuthenticationFailed(d)),
            Check::BadFormat => Some(Rejection::InvalidAuthenticationInfo),
            _ => None,
        }
    }
}

/// True when `AZEMU_LOOSE_AUTH` is on (see `crate::settings` for the spellings). Read once, at the first call; the
/// emulator makes that call when it starts and warns once.
pub fn loose_auth() -> bool {
    static LOOSE: OnceLock<bool> = OnceLock::new();
    *LOOSE.get_or_init(|| {
        let on = crate::settings::flag("AZEMU_LOOSE_AUTH", false);
        if on {
            tracing::warn!(
                "AZEMU_LOOSE_AUTH is on: storage signatures are not looked at (a placeholder such as `replay` or `Sanitized` passes, so does a wrong one) \
                 and the date of a Shared Key request is not checked; Azure refuses all of these"
            );
        }
        on
    })
}

/// Whether a client can know the key of this account, so its signatures are compared.
pub fn checkable_account(account: &str) -> bool {
    account.eq_ignore_ascii_case(DEV_ACCOUNT)
}

fn account_keys(accounts: &StorageAccounts, account: &str) -> Vec<Vec<u8>> {
    accounts.keys(account).iter().filter_map(|k| B64.decode(k).ok()).collect()
}

fn matches_any(keys: &[Vec<u8>], strings: &[String], sig: &[u8]) -> bool {
    keys.iter().any(|k| strings.iter().any(|s| hmac_sha256(k, s.as_bytes()).as_slice() == sig))
}

/// What a Shared Key request says about itself, before any key is looked at: the account and the signature bytes, and
/// whether it is Shared Key Lite. `Err` is the answer when the header cannot be read or the date is refused.
fn shared_key_claim(req: &Request) -> Result<(String, Vec<u8>, bool), Check> {
    let Some((account, sig, lite)) = parse_authorization(req) else {
        return Err(Check::BadFormat);
    };
    // Azure reads any signature that is base64, of any size (it answers a wrong size with the MAC detail), and refuses
    // one that is not base64 or is missing, or a header with no account name.
    let bytes = if account.is_empty() { None } else { decode_header_signature(&sig).filter(|b| !b.is_empty()) };
    let Some(bytes) = bytes else {
        return Err(Check::BadFormat);
    };
    if let Some(detail) = date_problem(req) {
        return Err(Check::Invalid(detail));
    }
    Ok((account, bytes, lite))
}

/// Check the signature of a `SharedKey` / `SharedKeyLite` request. Only requests with
/// such an `Authorization` header are looked at; anything else is `Skipped`.
pub fn verify_shared_key(req: &Request, service: StorageService, accounts: &StorageAccounts) -> Check {
    let _ = service; // the string to sign is the same for Blob and Queue
    if loose_auth() {
        return Check::Skipped(Skip::Loose);
    }
    let (hdr_account, sig_bytes, lite) = match shared_key_claim(req) {
        Ok(c) => c,
        Err(refused) => return refused,
    };
    if !checkable_account(&req.account) {
        return Check::Skipped(Skip::UnknownAccount);
    }
    // A request with no `x-ms-version` is signed the way requests were before 2009-09-19 (what Shared Key Lite still
    // is: the resource carries `?comp=` and the string has no headers of the request but `x-ms-*`). Tried: Azure
    // refuses a signature made the SDK way for such a request.
    let lite = lite || req.header("x-ms-version").is_none();
    let strings = shared_key_strings_to_sign(req, &hdr_account, lite);
    if matches_any(&account_keys(accounts, &req.account), &strings, &sig_bytes) {
        return Check::Valid;
    }
    let sig = parse_authorization(req).map(|(_, s, _)| s).unwrap_or_default();
    let shown = strings.first().cloned().unwrap_or_default();
    Check::Invalid(format!(
        "The MAC signature found in the HTTP request '{sig}' is not the same as any computed signature. Server used following string to sign: '{shown}'."
    ))
}

/// Check the signature of a `SharedKey` / `SharedKeyLite` request to the Table service, whose string
/// to sign differs from Blob's and Queue's (see [`table_shared_key_strings_to_sign`]). Same policy as
/// [`verify_shared_key`]: the MAC is compared only for `devstoreaccount1`; `AZEMU_LOOSE_AUTH=1` checks nothing.
pub fn verify_shared_key_table(req: &Request, accounts: &StorageAccounts) -> Check {
    if loose_auth() {
        return Check::Skipped(Skip::Loose);
    }
    let (hdr_account, sig_bytes, lite) = match shared_key_claim(req) {
        Ok(c) => c,
        Err(refused) => return refused,
    };
    if !checkable_account(&req.account) {
        return Check::Skipped(Skip::UnknownAccount);
    }
    let strings = table_shared_key_strings_to_sign(req, &hdr_account, lite);
    if matches_any(&account_keys(accounts, &req.account), &strings, &sig_bytes) {
        return Check::Valid;
    }
    let sig = parse_authorization(req).map(|(_, s, _)| s).unwrap_or_default();
    Check::Invalid(format!("The MAC signature found in the HTTP request '{sig}' is not the same as any computed signature."))
}

/// Resource paths (no leading slash) a service SAS may have been issued for, most
/// likely first.
fn resource_paths(f: &SasFields, target: &SasTarget<'_>) -> Vec<String> {
    match target {
        SasTarget::Queue { queue } => vec![queue.unwrap_or("").to_string()],
        SasTarget::File { share, path } => {
            let with_path = match path {
                Some(p) if !p.is_empty() => format!("{share}/{p}"),
                _ => share.to_string(),
            };
            match f.sr.as_str() {
                "s" => vec![share.to_string()],
                "f" => vec![with_path],
                _ => vec![with_path, share.to_string()],
            }
        }
        SasTarget::Blob { container, blob } => {
            let with_blob = match blob {
                Some(b) => format!("{container}/{b}"),
                None => container.to_string(),
            };
            match f.sr.as_str() {
                "c" => vec![container.to_string()],
                "" => {
                    let mut v = vec![with_blob];
                    let c = container.to_string();
                    if !v.contains(&c) {
                        v.push(c);
                    }
                    v
                }
                _ => vec![with_blob],
            }
        }
    }
}

/// The detail Azure gives for an `sv` it does not take.
fn sv_detail(service: StorageService) -> &'static str {
    match service {
        StorageService::Blob | StorageService::File => "SAS field 'sv' is not well formed.",
        StorageService::Queue => "sv is either not in the correct format or is not equal or later than version 2012-02-12",
    }
}

/// Check the signature of a SAS request (`sig` in the query). `target` names what the
/// request addresses (needed for a service SAS and a user delegation SAS).
pub fn verify_sas(req: &Request, service: StorageService, accounts: &StorageAccounts, target: &SasTarget<'_>) -> Check {
    if loose_auth() {
        return Check::Skipped(Skip::Loose);
    }
    let f = SasFields::from_query(&req.query);
    if f.sv.is_empty() {
        return Check::Invalid("sv is mandatory. Cannot be empty".to_string());
    }
    if !sas_version_ok(&f.sv) {
        return Check::Invalid(sv_detail(service).to_string());
    }
    let sig_bytes = match sas_signature(&f.sig) {
        Ok(b) => b,
        Err(detail) => return Check::Invalid(detail.to_string()),
    };
    if !checkable_account(&req.account) {
        return Check::Skipped(Skip::UnknownAccount);
    }
    let account = req.account.as_str();
    let ver = f.layout_version().to_string();
    let too_new = is_version(&f.sv) && f.sv.as_str() > NEWEST_KNOWN_VERSION;
    let mut uncertain = !is_version(&f.sv) || f.sv.as_str() < "2012-02-12";
    // The canonicalization of signed request headers / query parameters is described
    // differently by the documentation and the SDKs; accept when in doubt.
    uncertain |= !f.srh.is_empty() || !f.srq.is_empty();
    // A user delegation SAS for Azure Files is accepted when its signature does not match: its layout is not known here.
    uncertain |= service == StorageService::File && f.is_user_delegation();

    let (strings, keys): (Vec<String>, Vec<Vec<u8>>) = if f.is_account() {
        let mut s = vec![account_sas_string_to_sign(account, &f)];
        let lower = account.to_ascii_lowercase();
        if lower != account {
            s.push(account_sas_string_to_sign(&lower, &f));
        }
        (s, account_keys(accounts, account))
    } else {
        let time = match f.sr.as_str() {
            "bs" => req.query.get("snapshot").unwrap_or(""),
            "bv" => req.query.get("versionid").unwrap_or(""),
            _ => "",
        };
        let paths = resource_paths(&f, target);
        let mut s = Vec::new();
        let keys;
        if f.is_user_delegation() {
            let key = user_delegation_key(accounts, account, &f.skoid, &f.sktid, &f.skt, &f.ske, &f.sks, &f.skv);
            keys = vec![key.to_vec()];
            for p in &paths {
                let (v, unc) = user_delegation_sas_strings(service, account, &f, p, time, req);
                uncertain |= unc;
                for x in v {
                    if !s.contains(&x) {
                        s.push(x);
                    }
                }
            }
        } else {
            keys = account_keys(accounts, account);
            for p in &paths {
                // Before 2015-02-21 the service name is not in the resource; if the
                // version is old try both, in case a client differs.
                let prefixes: &[Option<bool>] = if ver.as_str() < "2015-04-05" { &[None, Some(false), Some(true)] } else { &[None] };
                for pre in prefixes {
                    let x = service_sas_string_to_sign(service, account, &f, p, time, *pre);
                    if !s.contains(&x) {
                        s.push(x);
                    }
                }
            }
        }
        (s, keys)
    };

    if matches_any(&keys, &strings, &sig_bytes) {
        return Check::Valid;
    }
    if uncertain {
        tracing::debug!(sv = %f.sv, "SAS signature does not match any known layout for this version; accepted");
        return Check::Skipped(Skip::Unverifiable);
    }
    let shown = strings.first().cloned().unwrap_or_default();
    let mut detail = format!("Signature did not match. String to sign used was {shown}\n");
    if too_new {
        detail.push_str(&format!(
            "azemu knows the SAS layouts up to sv={NEWEST_KNOWN_VERSION}; this SAS has sv={} and its signature does not match the newest layout. \
             If a newer SDK changed the layout, start azemu with AZEMU_LOOSE_AUTH=1 to skip signature checks.\n",
            f.sv
        ));
    }
    Check::Invalid(detail)
}
