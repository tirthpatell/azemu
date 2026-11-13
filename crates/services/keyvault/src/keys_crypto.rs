//! The cryptographic key operations: sign, verify, encrypt, decrypt, wrapKey,
//! unwrapKey. They run for real with the stored private key, off the async executor.

use azemu_core::{Request, Response};
use serde_json::json;

use crate::common::*;
use crate::crypto::{self, CryptoError, KeyMaterial};
use crate::errors::*;
use crate::jwk::Family;
use crate::KeyVaultService;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Sign,
    Verify,
    Encrypt,
    Decrypt,
    WrapKey,
    UnwrapKey,
}

impl Op {
    pub fn parse(segment: &str) -> Option<Op> {
        match segment.to_ascii_lowercase().as_str() {
            "sign" => Some(Op::Sign),
            "verify" => Some(Op::Verify),
            "encrypt" => Some(Op::Encrypt),
            "decrypt" => Some(Op::Decrypt),
            "wrapkey" => Some(Op::WrapKey),
            "unwrapkey" => Some(Op::UnwrapKey),
            _ => None,
        }
    }

    /// The name used in `key_ops` and in messages.
    pub fn name(self) -> &'static str {
        match self {
            Op::Sign => "sign",
            Op::Verify => "verify",
            Op::Encrypt => "encrypt",
            Op::Decrypt => "decrypt",
            Op::WrapKey => "wrapKey",
            Op::UnwrapKey => "unwrapKey",
        }
    }
}

fn crypto_error(e: CryptoError) -> Response {
    match e {
        CryptoError::Algorithm(m) | CryptoError::Input(m) | CryptoError::Failed(m) => bad_parameter(&m),
    }
}

fn family_name(f: Family) -> &'static str {
    match f {
        Family::Rsa => "RSA",
        Family::Ec => "EC",
        Family::Oct => "oct",
    }
}

/// Is `alg` a valid algorithm for this operation on a key of this family?
fn check_algorithm(op: Op, alg: &str, family: Family) -> Result<(), Response> {
    let sign_like = matches!(op, Op::Sign | Op::Verify);
    let rsa_sign = alg.starts_with("RS") || alg.starts_with("PS");
    let known = if sign_like { crypto::sign_digest_len(alg).is_some() } else { crypto::is_rsa_encrypt_alg(alg) };
    let fits = match (sign_like, family) {
        (true, Family::Rsa) => rsa_sign,
        (true, Family::Ec) => alg.starts_with("ES"),
        (false, Family::Rsa) => known,
        _ => false,
    };
    if !known {
        return Err(bad_parameter(&format!("The algorithm '{alg}' is not supported for the {} operation.", op.name())));
    }
    if !fits {
        return Err(bad_parameter(&format!("The algorithm '{alg}' is not supported for key type {}.", family_name(family))));
    }
    Ok(())
}

impl KeyVaultService {
    pub async fn key_operation(&self, req: &Request, name: &str, version: Option<&str>, op: Op) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let now = now_secs(req);
        let base = req.ctx.endpoint.base_url();
        // Take what we need out of the state; never hold the lock while computing.
        let (kid, material, family, key_ops, enabled, nbf, exp) = {
            let mut st = self.state.lock();
            let vault = st.vault(&req.account, now);
            let what = match version {
                Some(v) if !v.is_empty() => format!("{name}/{v}"),
                _ => name.to_string(),
            };
            let Some(item) = vault.keys.active(name) else {
                return not_found(Kind::Key, &what);
            };
            let Some(v) = item.pick(version) else {
                return not_found(Kind::Key, &what);
            };
            (
                format!("{base}/keys/{}/{}", item.name, v.version),
                v.material.clone(),
                v.family,
                v.key_ops.clone(),
                v.attrs.enabled,
                v.attrs.nbf,
                v.attrs.exp,
            )
        };
        let opn = op.name();
        if !enabled {
            return forbidden(&format!("Operation {opn} is not allowed on a disabled key."), "KeyDisabled");
        }
        if nbf.map(|n| n > now).unwrap_or(false) {
            return forbidden(
                &format!("Operation {opn} is not allowed on this key: the key is not yet valid (nbf)."),
                "KeyOperationForbidden",
            );
        }
        if exp.map(|e| e <= now).unwrap_or(false) {
            return forbidden(&format!("Operation {opn} is not allowed on this key: the key has expired (exp)."), "KeyOperationForbidden");
        }
        if !key_ops.iter().any(|k| k == opn) {
            return forbidden(&format!("Operation {opn} is not permitted on this key."), "KeyOperationForbidden");
        }

        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let alg = match str_field(&body, "alg") {
            Ok(a) => a.to_string(),
            Err(r) => return r,
        };
        let value = match b64_field(&body, "value") {
            Ok(v) => v,
            Err(r) => return r,
        };
        let digest = if op == Op::Verify {
            match b64_field(&body, "digest") {
                Ok(d) => Some(d),
                Err(r) => return r,
            }
        } else {
            None
        };
        if family == Family::Oct {
            return not_implemented(&format!("{opn} with a symmetric (oct) key"), &format!("KeyVault_{opn} (symmetric key)"));
        }
        if let Err(r) = check_algorithm(op, &alg, family) {
            return r;
        }
        let result = tokio::task::spawn_blocking(move || run(&material, op, &alg, &value, digest.as_deref())).await;
        match result {
            Ok(Ok(Output::Bytes(b))) => json_response(200, &json!({ "kid": kid, "value": b64u(&b) })),
            Ok(Ok(Output::Bool(ok))) => json_response(200, &json!({ "value": ok })),
            Ok(Err(e)) => crypto_error(e),
            Err(_) => json_response(500, &json!({"error": {"code": "InternalError", "message": "the key operation failed"}})),
        }
    }
}

enum Output {
    Bytes(Vec<u8>),
    Bool(bool),
}

fn run(m: &KeyMaterial, op: Op, alg: &str, value: &[u8], digest: Option<&[u8]>) -> Result<Output, CryptoError> {
    match op {
        // For sign the request's `value` is the digest; for verify `digest` is the digest
        // and `value` the signature.
        Op::Sign => crypto::sign(m, alg, value).map(Output::Bytes),
        Op::Verify => crypto::verify(m, alg, digest.unwrap_or(&[]), value).map(Output::Bool),
        Op::Encrypt | Op::WrapKey => crypto::rsa_encrypt(m, alg, value).map(Output::Bytes),
        Op::Decrypt | Op::UnwrapKey => crypto::rsa_decrypt(m, alg, value).map(Output::Bytes),
    }
}
