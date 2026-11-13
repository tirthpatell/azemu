//! Request authentication and shared access signatures.
//!
//! Bearer tokens are accepted without checking. A Shared Key signature is checked for the
//! development account only (`azemu_core::storage::sig::verify_shared_key_table`); any other
//! account's is accepted, and `AZEMU_LOOSE_AUTH=1` turns checking off. A SAS is
//! honoured: the signature (for the development account, whose key is known,
//! and only when it looks like a real one), the validity
//! window, the protocol, the services / resource types / permissions of an
//! account SAS, and the table, permissions, key range and stored access policy
//! of a table SAS. Requests with no credentials are refused like Azure does.

use azemu_core::storage::{
    auth::{classify, Auth, SasParams},
    sig::{loose_auth, verify_shared_key_table, Check},
    StorageAccounts,
};
use azemu_core::Request;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::errors::{
    auth_failed, authorization_failure, permission_mismatch, protocol_mismatch, resource_type_mismatch, service_mismatch, ApiError, Res,
};
use crate::state::Table;

/// An operation, for permission checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    ServiceGet,
    ServiceSet,
    QueryTables,
    CreateTable,
    DeleteTable,
    AclGet,
    AclSet,
    EntityRead,
    EntityAdd,
    EntityUpdate,
    /// Insert-or-replace and insert-or-merge need both add and update.
    EntityUpsert,
    EntityDelete,
}

#[derive(Debug, Clone, Default)]
pub struct KeyRange {
    pub spk: Option<String>,
    pub srk: Option<String>,
    pub epk: Option<String>,
    pub erk: Option<String>,
}

impl KeyRange {
    /// The SAS covers `(pk, rk)` when it lies between (spk, srk) and (epk, erk)
    /// compared as a pair; a missing bound is open.
    pub fn contains(&self, pk: &str, rk: &str) -> bool {
        use std::cmp::Ordering::*;
        let cmp = |a: &str, b: &str| a.encode_utf16().cmp(b.encode_utf16());
        let pair = |ap: &str, ar: Option<&str>| -> std::cmp::Ordering {
            match cmp(pk, ap) {
                Equal => ar.map(|r| cmp(rk, r)).unwrap_or(Equal),
                o => o,
            }
        };
        if let Some(sp) = &self.spk {
            if pair(sp, self.srk.as_deref()) == Less {
                return false;
            }
        }
        if let Some(ep) = &self.epk {
            if pair(ep, self.erk.as_deref()) == Greater {
                return false;
            }
        }
        true
    }

    pub fn is_open(&self) -> bool {
        self.spk.is_none() && self.epk.is_none()
    }
}

#[derive(Debug, Clone)]
pub enum Grant {
    /// Shared key or bearer token.
    Full,
    Account {
        perms: String,
        srt: String,
    },
    Table {
        perms: String,
        range: KeyRange,
    },
}

fn has_any(perms: &str, wanted: &str) -> bool {
    wanted.chars().any(|c| perms.contains(c))
}

impl Grant {
    /// Check an operation. `table` is the table addressed (entity operations
    /// and ACLs) and `key` the entity, when the request names one.
    pub fn check(&self, op: Op, key: Option<(&str, &str)>) -> Res<()> {
        match self {
            Grant::Full => Ok(()),
            Grant::Account { perms, srt } => {
                let (types, wanted): (&str, &str) = match op {
                    Op::ServiceGet => ("s", "r"),
                    Op::ServiceSet => ("s", "w"),
                    Op::QueryTables => ("sc", "lr"),
                    Op::CreateTable => ("c", "caw"),
                    Op::DeleteTable => ("c", "dw"),
                    Op::AclGet | Op::AclSet => return Err(authorization_failure()),
                    Op::EntityRead => ("o", "r"),
                    Op::EntityAdd => ("o", "a"),
                    Op::EntityUpdate => ("o", "u"),
                    Op::EntityUpsert => ("o", "au"),
                    Op::EntityDelete => ("o", "d"),
                };
                if !types.chars().any(|c| srt.contains(c)) {
                    return Err(resource_type_mismatch());
                }
                let ok = if op == Op::EntityUpsert { perms.contains('a') && perms.contains('u') } else { has_any(perms, wanted) };
                if !ok {
                    return Err(permission_mismatch());
                }
                Ok(())
            }
            Grant::Table { perms, range } => {
                let wanted = match op {
                    Op::EntityRead => "r",
                    Op::EntityAdd => "a",
                    Op::EntityUpdate => "u",
                    Op::EntityUpsert => "au",
                    Op::EntityDelete => "d",
                    _ => return Err(authorization_failure()),
                };
                let ok = if op == Op::EntityUpsert { perms.contains('a') && perms.contains('u') } else { has_any(perms, wanted) };
                if !ok {
                    return Err(permission_mismatch());
                }
                if let Some((pk, rk)) = key {
                    if !range.contains(pk, rk) {
                        return Err(authorization_failure());
                    }
                }
                Ok(())
            }
        }
    }

    /// Whether results of a query must be limited to a key range.
    pub fn range(&self) -> Option<&KeyRange> {
        match self {
            Grant::Table { range, .. } if !range.is_open() => Some(range),
            _ => None,
        }
    }
}

fn mac(key: &[u8], data: &str) -> Option<Vec<u8>> {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).ok()?;
    m.update(data.as_bytes());
    Some(m.finalize().into_bytes().to_vec())
}

/// A signature is only checked when it is long enough to be a real one (a
/// base64 SHA-256 MAC is 44 characters). Short placeholders such as
/// `Sanitized`, as found in recorded traffic, pass unchecked.
fn looks_real(sig: &str) -> bool {
    sig.len() >= 32
}

fn signature_ok(accounts: &StorageAccounts, account: &str, sig: &[u8], string_to_sign: &str) -> bool {
    accounts.keys(account).iter().any(|k| B64.decode(k).ok().and_then(|key| mac(&key, string_to_sign)).is_some_and(|m| m == sig))
}

fn v(o: &Option<String>) -> &str {
    o.as_deref().unwrap_or("")
}

fn account_string_to_sign(account: &str, p: &SasParams, ses: &str) -> String {
    let mut s = format!(
        "{account}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
        v(&p.sp),
        v(&p.ss),
        v(&p.srt),
        v(&p.st),
        v(&p.se),
        v(&p.sip),
        v(&p.spr),
        v(&p.sv),
    );
    if p.sv.as_deref().is_some_and(|sv| sv >= "2020-12-06") {
        s.push_str(ses);
        s.push('\n');
    }
    s
}

fn table_string_to_sign(account: &str, table: &str, p: &SasParams, range: &KeyRange) -> String {
    [
        v(&p.sp),
        v(&p.st),
        v(&p.se),
        &format!("/table/{account}/{}", table.to_ascii_lowercase()),
        v(&p.si),
        v(&p.sip),
        v(&p.spr),
        v(&p.sv),
        v(&range.spk),
        v(&range.srk),
        v(&range.epk),
        v(&range.erk),
    ]
    .join("\n")
}

/// The challenge a client without credentials gets from newer API versions.
fn missing_credentials(req: &Request) -> ApiError {
    let version = req.header("x-ms-version").unwrap_or("");
    if version >= "2020-12-06" {
        ApiError {
            status: 401,
            code: "InvalidAuthenticationInfo".into(),
            message: "Server failed to authenticate the request. Please refer to the information in the www-authenticate header.".into(),
            challenge: Some(
                "Bearer authorization_uri=https://login.microsoftonline.com/00000000-0000-0000-0000-000000000000/oauth2/authorize resource_id=https://storage.azure.com".into(),
            ),
            unsupported: None,
        }
    } else {
        auth_failed()
    }
}

/// Work out what the request is allowed to do. `table` is the table named in
/// the URL, if any; it is needed to check table SAS tokens and stored policies.
pub fn authenticate(req: &Request, accounts: &StorageAccounts, table: Option<(&str, Option<&Table>)>) -> Res<Grant> {
    match classify(req) {
        Auth::Bearer => Ok(Grant::Full),
        // The signature of the well-known development account is checked (Table's own string to
        // sign); any other account, a placeholder signature and `AZEMU_LOOSE_AUTH=1` pass.
        Auth::SharedKey { .. } => match verify_shared_key_table(req, accounts) {
            Check::Invalid(_) => Err(auth_failed()),
            _ => Ok(Grant::Full),
        },
        Auth::Anonymous => {
            // Any Authorization header counts as credentials (loose auth); no header at all does not.
            if req.header("authorization").is_some_and(|a| !a.trim().is_empty()) {
                Ok(Grant::Full)
            } else {
                Err(missing_credentials(req))
            }
        }
        Auth::Sas(p) => sas(req, accounts, &p, table),
    }
}

fn sas(req: &Request, accounts: &StorageAccounts, p: &SasParams, table: Option<(&str, Option<&Table>)>) -> Res<Grant> {
    let now = req.ctx.time;
    let account = req.account.as_str();
    let is_account_sas = p.ss.is_some() || p.srt.is_some();
    let range = KeyRange {
        spk: req.query.get("spk").map(str::to_string),
        srk: req.query.get("srk").map(str::to_string),
        epk: req.query.get("epk").map(str::to_string),
        erk: req.query.get("erk").map(str::to_string),
    };

    // 1. signature. Only the well-known development account has a key a client can
    // know; for any other account name the key is made up by azemu, so a signature
    // cannot be checked fairly and is accepted.
    let checkable = account == azemu_core::storage::DEV_ACCOUNT && !loose_auth();
    if let Some(sig) = p.sig.as_deref().filter(|s| checkable && looks_real(s)) {
        let sig = B64.decode(sig).unwrap_or_default();
        let ok = if is_account_sas {
            let sts = account_string_to_sign(account, p, req.query.get("ses").unwrap_or(""));
            signature_ok(accounts, account, &sig, &sts)
        } else {
            match table {
                Some((name, _)) => signature_ok(accounts, account, &sig, &table_string_to_sign(account, name, p, &range)),
                // not a table URL: a table SAS cannot be valid here anyway
                None => false,
            }
        };
        if !ok {
            return Err(auth_failed());
        }
    }

    // 2. stored access policy (table SAS only)
    let mut perms = p.sp.clone();
    let mut start = p.st.clone();
    let mut expiry = p.se.clone();
    if let (Some(id), false) = (p.si.as_deref(), is_account_sas) {
        let found = table.and_then(|(_, t)| t).and_then(|t| t.acl.iter().find(|s| s.id == id));
        let Some(found) = found else {
            return Err(authorization_failure());
        };
        if let Some(policy) = &found.policy {
            perms = perms.or_else(|| policy.permission.clone());
            start = start.or_else(|| policy.start.clone());
            expiry = expiry.or_else(|| policy.expiry.clone());
        }
    }

    // 3. validity window and protocol
    let window = SasParams { st: start, se: expiry, ..Default::default() };
    if window.expired(now) || window.not_yet_valid(now) {
        return Err(authorization_failure());
    }
    if p.spr.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("https")) && req.ctx.endpoint.scheme != "https" {
        return Err(protocol_mismatch());
    }

    // 4. what it grants
    let perms = perms.unwrap_or_default();
    if is_account_sas {
        if !p.ss.as_deref().unwrap_or("").contains('t') {
            return Err(service_mismatch());
        }
        Ok(Grant::Account { perms, srt: p.srt.clone().unwrap_or_default() })
    } else {
        if let (Some(tn), Some((name, _))) = (req.query.get("tn"), table) {
            if !tn.eq_ignore_ascii_case(name) {
                return Err(authorization_failure());
            }
        }
        Ok(Grant::Table { perms, range })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_range_is_a_pair_range() {
        let r = KeyRange { spk: Some("test".into()), srk: Some("test1".into()), epk: Some("test".into()), erk: Some("test1".into()) };
        assert!(r.contains("test", "test1"));
        assert!(!r.contains("test", "test0"));
        assert!(!r.contains("test", "test2"));
        assert!(!r.contains("other", "x"));
        let p = KeyRange { spk: Some("q".into()), epk: Some("q".into()), ..Default::default() };
        assert!(p.contains("q", "anything"));
        assert!(!p.contains("p", "r"));
        assert!(!p.contains("r", "r"));
        assert!(KeyRange::default().contains("x", "y"));
    }

    #[test]
    fn account_permissions_and_resource_types() {
        let g = |perms: &str, srt: &str| Grant::Account { perms: perms.into(), srt: srt.into() };
        assert!(g("r", "o").check(Op::EntityRead, None).is_ok());
        assert_eq!(g("r", "c").check(Op::EntityRead, None).unwrap_err().code, "AuthorizationResourceTypeMismatch");
        assert_eq!(g("a", "o").check(Op::EntityRead, None).unwrap_err().code, "AuthorizationPermissionMismatch");
        assert!(g("au", "o").check(Op::EntityUpsert, None).is_ok());
        assert!(g("a", "o").check(Op::EntityUpsert, None).is_err());
        assert!(g("rl", "sco").check(Op::QueryTables, None).is_ok());
        assert!(g("r", "s").check(Op::ServiceGet, None).is_ok());
        assert!(g("rwdlacu", "sco").check(Op::AclGet, None).is_err());
    }

    #[test]
    fn table_sas_checks_permissions_and_range() {
        let g = Grant::Table {
            perms: "a".into(),
            range: KeyRange { spk: Some("p".into()), srk: Some("r".into()), epk: Some("p".into()), erk: Some("r".into()) },
        };
        assert!(g.check(Op::EntityAdd, Some(("p", "r"))).is_ok());
        assert_eq!(g.check(Op::EntityAdd, Some(("x", "y"))).unwrap_err().code, "AuthorizationFailure");
        assert_eq!(g.check(Op::EntityRead, None).unwrap_err().code, "AuthorizationPermissionMismatch");
        assert!(g.check(Op::CreateTable, None).is_err());
    }

    #[test]
    fn only_real_looking_signatures_are_checked() {
        assert!(!looks_real("Sanitized"));
        assert!(looks_real(&B64.encode([0u8; 32])));
        assert!(looks_real(&"A".repeat(46)));
    }

    #[test]
    fn table_signature_round_trip() {
        let accounts = StorageAccounts::new();
        let key = B64.decode(accounts.key("acct", 1)).unwrap();
        let p = SasParams {
            sp: Some("r".into()),
            se: Some("2031-01-01T00:00:00Z".into()),
            sv: Some("2019-02-02".into()),
            ..Default::default()
        };
        let sts = table_string_to_sign("acct", "MyTable", &p, &KeyRange::default());
        assert_eq!(sts, "r\n\n2031-01-01T00:00:00Z\n/table/acct/mytable\n\n\n\n2019-02-02\n\n\n\n");
        let sig = mac(&key, &sts).unwrap();
        assert!(signature_ok(&accounts, "acct", &sig, &sts));
        assert!(!signature_ok(&accounts, "acct", &sig, "other"));
    }
}
