//! User delegation keys that can be derived again from the fields a SAS carries.
//!
//! Real Azure hands out a random key and remembers it. azemu keeps no state for this:
//! the key is an HMAC of the signed fields under the account key, so a service can
//! hand a key out (Get User Delegation Key) and later verify a SAS signed with it
//! from `skoid`, `sktid`, `skt`, `ske`, `sks` and `skv` alone.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use super::hmac::hmac_sha256;
use crate::storage::StorageAccounts;
use crate::time::{iso8601_secs, parse_iso8601};

/// Times are compared as `2022-10-27T22:06:34Z`, whatever precision the client sent.
fn normalize_time(s: &str) -> String {
    parse_iso8601(s).map(iso8601_secs).unwrap_or_else(|| s.to_string())
}

/// The 32 key bytes for these signed fields. `start` and `expiry` are ISO 8601 times
/// (`SignedStart` / `SignedExpiry` of the key, `skt` / `ske` of a SAS), `service` is
/// `b` or `q`, `version` the key's signed version (`skv`).
#[allow(clippy::too_many_arguments)]
pub fn user_delegation_key(
    accounts: &StorageAccounts,
    account: &str,
    oid: &str,
    tid: &str,
    start: &str,
    expiry: &str,
    service: &str,
    version: &str,
) -> [u8; 32] {
    let account_key = B64.decode(accounts.key(account, 1)).unwrap_or_default();
    let msg = format!(
        "azemu-udk-v1\n{}\n{oid}\n{tid}\n{}\n{}\n{service}\n{version}",
        account.to_ascii_lowercase(),
        normalize_time(start),
        normalize_time(expiry),
    );
    hmac_sha256(&account_key, msg.as_bytes())
}

/// Same, base64 encoded: the `Value` element of the Get User Delegation Key answer.
#[allow(clippy::too_many_arguments)]
pub fn user_delegation_key_b64(
    accounts: &StorageAccounts,
    account: &str,
    oid: &str,
    tid: &str,
    start: &str,
    expiry: &str,
    service: &str,
    version: &str,
) -> String {
    B64.encode(user_delegation_key(accounts, account, oid, tid, start, expiry, service, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_stable_and_sensitive_to_every_field() {
        let a = StorageAccounts::new();
        let k = |acct: &str, oid: &str, start: &str, svc: &str| {
            user_delegation_key_b64(&a, acct, oid, "tid", start, "2030-01-02T00:00:00Z", svc, "2023-11-03")
        };
        let base = k("devstoreaccount1", "oid", "2030-01-01T00:00:00Z", "b");
        assert_eq!(base, k("devstoreaccount1", "oid", "2030-01-01T00:00:00Z", "b"));
        assert_eq!(base, k("DevStoreAccount1", "oid", "2030-01-01T00:00:00.0000000Z", "b"));
        assert_eq!(B64.decode(&base).unwrap().len(), 32);
        assert_ne!(base, k("other", "oid", "2030-01-01T00:00:00Z", "b"));
        assert_ne!(base, k("devstoreaccount1", "oid2", "2030-01-01T00:00:00Z", "b"));
        assert_ne!(base, k("devstoreaccount1", "oid", "2030-01-01T00:00:01Z", "b"));
        assert_ne!(base, k("devstoreaccount1", "oid", "2030-01-01T00:00:00Z", "q"));
    }
}
