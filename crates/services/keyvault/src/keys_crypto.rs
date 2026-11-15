//! The cryptographic key operations: sign, verify, encrypt, decrypt, wrapKey,
//! unwrapKey. They run for real with the stored private key, off the async executor.

use azemu_core::{Request, Response};
use serde_json::{json, Map, Value};

use crate::common::*;
use crate::crypto::{self, CryptoError, Curve, KeyMaterial};
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

/// What the service says when a property of the body is missing or wrong (the name is empty for most of them).
fn invalid_property(name: &str) -> String {
    format!("Property {name} has invalid value\r\n")
}

/// A base64url property of the body. A missing one adds to `problems`; so does one that is not base64url.
fn read_b64(body: &Map<String, Value>, name: &str, problems: &mut String) -> Option<Vec<u8>> {
    match body.get(name) {
        Some(Value::String(s)) => match b64u_decode(s) {
            Some(b) => Some(b),
            None => {
                problems.push_str(&format!("Content of '{name}' is not base64url.\r\nProperty '{name}' is required.\r\n"));
                None
            }
        },
        _ if name == "digest" => {
            problems.push_str(&invalid_property(""));
            None
        }
        _ => {
            problems.push_str(&format!("Property '{name}' is required.\r\n"));
            None
        }
    }
}

/// `alg`, `value` and, for verify, `digest` of a key operation. Every problem is listed, in the order the service lists them.
/// `alg`, `value`, and for verify `digest`.
type Params = (String, Vec<u8>, Option<Vec<u8>>);

fn read_params(body: &Map<String, Value>, op: Op) -> Result<Params, Response> {
    let mut problems = String::new();
    let alg = match body.get("alg") {
        Some(Value::String(a)) if !a.is_empty() => Some(a.clone()),
        _ => {
            problems.push_str(&invalid_property(""));
            None
        }
    };
    let digest = if op == Op::Verify { read_b64(body, "digest", &mut problems) } else { None };
    let value = read_b64(body, "value", &mut problems);
    match (alg, value) {
        (Some(alg), Some(value)) if problems.is_empty() => Ok((alg, value, digest)),
        _ => Err(bad_parameter(&problems)),
    }
}

/// The curve a signature algorithm needs.
fn curve_of_alg(alg: &str) -> Option<Curve> {
    match alg {
        "ES256" => Some(Curve::P256),
        "ES384" => Some(Curve::P384),
        "ES512" => Some(Curve::P521),
        "ES256K" => Some(Curve::P256K),
        _ => None,
    }
}

/// Is `alg` a valid algorithm for this operation on this key? The texts are the service's own (tried in November 2026).
fn check_algorithm(op: Op, alg: &str, key: &KeyInfo) -> Result<(), Response> {
    let sign_like = matches!(op, Op::Sign | Op::Verify);
    let known = if sign_like { crypto::sign_digest_len(alg).is_some() } else { crypto::is_rsa_encrypt_alg(alg) };
    if !known {
        return Err(bad_parameter(&format!("Invalid algorithm: {alg}")));
    }
    let incompatible = |what: &str| Err(bad_parameter(&format!("Key and signing algorithm are incompatible. Key {} {what}", key.kid)));
    match (sign_like, key.family) {
        (true, Family::Rsa) if alg.starts_with("ES") => {
            incompatible(&format!("is of type '{}', and algorithm '{alg}' can only be used with a key of type 'EC' or 'EC-HSM'.", key.kty))
        }
        (true, Family::Ec) if !alg.starts_with("ES") => incompatible(&format!(
            "is of type '{}', and algorithm '{alg}' can only be used with a key of type 'RSA' or 'RSA-HSM'.",
            key.kty
        )),
        (true, Family::Ec) => match (&key.material, curve_of_alg(alg)) {
            (KeyMaterial::Ec { curve, .. }, Some(want)) if *curve != want => incompatible(&format!(
                "uses curve '{}', and algorithm '{alg}' can only be used with curve '{}'.",
                curve.name(),
                want.name()
            )),
            _ => Ok(()),
        },
        (true, Family::Rsa) | (false, Family::Rsa) => Ok(()),
        _ => Err(bad_parameter(&format!("The algorithm '{alg}' is not supported for key type {}.", key.kty))),
    }
}

/// What the checks of an operation need to know about the key.
struct KeyInfo {
    kid: String,
    kty: String,
    family: Family,
    material: KeyMaterial,
}

impl KeyVaultService {
    pub async fn key_operation(&self, req: &Request, name: &str, version: Option<&str>, op: Op) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let now = now_secs(req);
        let base = req.ctx.endpoint.base_url();
        // Take what we need out of the state; never hold the lock while computing.
        let (kid, kty, material, family, key_ops, enabled, nbf, exp) = {
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
                v.kty.clone(),
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
                &format!("Operation {opn} is not allowed because the 'not before' constraint on this key is not met."),
                "KeyOperationForbidden",
            );
        }
        if exp.map(|e| e <= now).unwrap_or(false) {
            return forbidden(&format!("Operation {opn} is not allowed on an expired key."), "KeyOperationForbidden");
        }
        if !key_ops.iter().any(|k| k == opn) {
            return forbidden(&format!("Operation {opn} is not permitted on this key."), "KeyOperationForbidden");
        }

        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let (alg, value, digest) = match read_params(&body, op) {
            Ok(p) => p,
            Err(r) => return r,
        };
        if family == Family::Oct {
            return not_implemented(&format!("{opn} with a symmetric (oct) key"), &format!("KeyVault_{opn} (symmetric key)"));
        }
        let info = KeyInfo { kid: kid.clone(), kty, family, material: material.clone() };
        if let Err(r) = check_algorithm(op, &alg, &info) {
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
