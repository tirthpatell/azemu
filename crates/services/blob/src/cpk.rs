//! Customer-provided keys (`x-ms-encryption-key`, `x-ms-encryption-key-sha256`, `x-ms-encryption-algorithm`).
//!
//! Nothing is encrypted here. What is kept is what a test can see on Azure: the hash of the key a blob was written
//! with (`Blob::cpk_sha256`), and the checks that depend on it. Every request that carries the headers is checked for
//! consistency (`validate`), and a blob written with a key refuses reads and writes with another key or none
//! (`store::check_cpk`). The headers are described in "Provide an encryption key on a request to Blob storage"
//! (REST documentation) and in the 2026 Blob service specification (`EncryptionKey`, `EncryptionKeySha256`,
//! `EncryptionAlgorithm`).
//!
//! What is from documentation and what is a guess:
//! - key, hash and algorithm go together; the algorithm is `AES256`; the key is a base64 AES-256 key (32 bytes); the hash
//!   is the base64 SHA-256 of the key bytes (specification text: "Must be provided if the x-ms-encryption-key header is
//!   provided"). A request that sends a key without the other two is `400 MissingRequiredHeader`, a bad value is
//!   `400 InvalidHeaderValue` naming the header, a hash that is not the hash of the key is `400 InvalidHeaderValue` with
//!   `HeaderName x-ms-encryption-key` (a report of the real service's answer to a wrongly encoded hash; no recording).
//! - A blob written with a key and read or written without one is `409 BlobUsesCustomerSpecifiedEncryption` (recorded).
//! - The same blob with another key: the same error code and status. The real answer is not documented in the specification
//!   or in the recordings; this is the only CPK error code the SDKs know.
//! - A key sent for a blob that was not written with one is accepted (the real service refuses; not checked).

use azemu_core::Request;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sha2::{Digest, Sha256};

use crate::errors::ApiError;
use crate::util::hdr;

/// The three header names of one set: the request's own key, or the source key of a copy from a URL.
const SETS: [(&str, &str, &str); 2] = [
    ("x-ms-encryption-key", "x-ms-encryption-key-sha256", "x-ms-encryption-algorithm"),
    ("x-ms-source-encryption-key", "x-ms-source-encryption-key-sha256", "x-ms-source-encryption-algorithm"),
];

/// Check the key headers of a request against each other. `Ok` when there are none.
pub fn validate(req: &Request) -> Result<(), ApiError> {
    for (key_h, sha_h, alg_h) in SETS {
        let (key, sha, alg) = (hdr(req, key_h), hdr(req, sha_h), hdr(req, alg_h));
        if let Some(sha) = sha {
            // a hash alone is allowed (requests that only read properties); it must be a SHA-256 hash
            if STANDARD.decode(sha.trim()).map(|b| b.len()) != Ok(32) {
                return Err(ApiError::invalid_header(sha_h, sha));
            }
        }
        let Some(key) = key else { continue };
        let sha = sha.ok_or_else(|| ApiError::missing_header(sha_h))?;
        let alg = alg.ok_or_else(|| ApiError::missing_header(alg_h))?;
        if alg != "AES256" {
            return Err(ApiError::invalid_header(alg_h, alg));
        }
        // the key itself is a secret: it is not echoed in the error
        let bytes = STANDARD
            .decode(key.trim())
            .ok()
            .filter(|b| b.len() == 32)
            .ok_or_else(|| ApiError::new("InvalidHeaderValue").detail("HeaderName", key_h))?;
        if STANDARD.encode(Sha256::digest(&bytes)) != sha.trim() {
            return Err(ApiError::new("InvalidHeaderValue")
                .detail("HeaderName", key_h)
                .msg("The value for one of the HTTP headers is not in the correct format: the hash does not match the encryption key."));
        }
    }
    Ok(())
}

/// A write that carries a hash needs the key too (the data cannot be encrypted without it).
pub fn require_key_with_hash(req: &Request) -> Result<(), ApiError> {
    if hdr(req, "x-ms-encryption-key-sha256").is_some() && hdr(req, "x-ms-encryption-key").is_none() {
        return Err(ApiError::missing_header("x-ms-encryption-key"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use azemu_core::http::{HeaderMap, HeaderValue, Method};
    use azemu_core::{Endpoint, Query, Request, RequestCtx};

    fn req(headers: &[(&str, String)]) -> Request {
        let mut h = HeaderMap::new();
        for (k, v) in headers {
            h.insert(azemu_core::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        Request {
            method: Method::PUT,
            path: "/c/b".into(),
            query: Query::parse(""),
            headers: h,
            body: Default::default(),
            account: "acct".into(),
            ctx: RequestCtx {
                request_id: "r".into(),
                time: azemu_core::time::OffsetDateTime::UNIX_EPOCH,
                endpoint: Endpoint { scheme: "http", authority: "a".into(), path_style: false, account: "acct".into() },
            },
        }
    }

    fn pair(k: &[u8]) -> (String, String) {
        (STANDARD.encode(k), STANDARD.encode(Sha256::digest(k)))
    }

    #[test]
    fn consistent_headers_pass_and_inconsistent_ones_are_refused() {
        let (key, sha) = pair(&[7u8; 32]);
        let ok = req(&[
            ("x-ms-encryption-key", key.clone()),
            ("x-ms-encryption-key-sha256", sha.clone()),
            ("x-ms-encryption-algorithm", "AES256".into()),
        ]);
        assert!(validate(&ok).is_ok());
        assert!(validate(&req(&[])).is_ok());
        assert!(validate(&req(&[("x-ms-encryption-key-sha256", sha.clone())])).is_ok(), "a hash alone is for reads");
        let code = |r: Request| validate(&r).unwrap_err();
        // missing companions
        assert_eq!(
            code(req(&[("x-ms-encryption-key", key.clone()), ("x-ms-encryption-algorithm", "AES256".into())])).code,
            "MissingRequiredHeader"
        );
        assert_eq!(
            code(req(&[("x-ms-encryption-key", key.clone()), ("x-ms-encryption-key-sha256", sha.clone())])).code,
            "MissingRequiredHeader"
        );
        // wrong algorithm, bad key, a hash that is not the hash of the key (the review's `AAAA`), a hash of the wrong size
        let with = |k: &str, s: &str, a: &str| {
            req(&[("x-ms-encryption-key", k.into()), ("x-ms-encryption-key-sha256", s.into()), ("x-ms-encryption-algorithm", a.into())])
        };
        assert_eq!(code(with(&key, &sha, "AES128")).code, "InvalidHeaderValue");
        assert_eq!(code(with("not base64!", &sha, "AES256")).code, "InvalidHeaderValue");
        assert_eq!(code(with(&STANDARD.encode([1u8; 16]), &sha, "AES256")).code, "InvalidHeaderValue");
        let e = code(with(&key, "AAAA", "AES256"));
        assert_eq!(e.code, "InvalidHeaderValue");
        let e2 = code(with(&key, &pair(&[8u8; 32]).1, "AES256"));
        assert_eq!(e2.code, "InvalidHeaderValue");
        assert!(e2.details.iter().any(|(k, v)| k == "HeaderName" && v == "x-ms-encryption-key"));
        // the key is not echoed back
        assert!(!format!("{:?}", e2.details).contains(&key));
        // the source set of a copy is checked the same way
        let src = req(&[
            ("x-ms-source-encryption-key", key.clone()),
            ("x-ms-source-encryption-key-sha256", "AAAA".into()),
            ("x-ms-source-encryption-algorithm", "AES256".into()),
        ]);
        assert_eq!(code(src).code, "InvalidHeaderValue");
    }
}
