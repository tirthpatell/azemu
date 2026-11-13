//! JWK <-> key material. Output is the public JWK exactly as Key Vault returns it
//! (`kid`, `kty`, `key_ops`, `n`, `e`, `crv`, `x`, `y`; never a private field). Input
//! is the JWK of an import request or of one of our own backups.

use azemu_core::Response;
use serde_json::{json, Map, Value};

use crate::common::{b64u, b64u_decode};
use crate::crypto::{self, Curve, KeyMaterial, PublicParts};
use crate::errors::bad_parameter;

/// What a `kty` means for us.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Family {
    Rsa,
    Ec,
    Oct,
}

/// `(family, canonical kty)`; the `-HSM` kinds are accepted and echoed back (the key
/// material is software either way).
pub fn parse_kty(s: &str) -> Option<(Family, &'static str)> {
    match s.to_ascii_lowercase().as_str() {
        "rsa" => Some((Family::Rsa, "RSA")),
        "rsa-hsm" => Some((Family::Rsa, "RSA-HSM")),
        "ec" => Some((Family::Ec, "EC")),
        "ec-hsm" => Some((Family::Ec, "EC-HSM")),
        "oct" => Some((Family::Oct, "oct")),
        "oct-hsm" => Some((Family::Oct, "oct-HSM")),
        _ => None,
    }
}

pub const KEY_OPS: [&str; 8] = ["encrypt", "decrypt", "sign", "verify", "wrapKey", "unwrapKey", "import", "export"];

pub fn default_key_ops(f: Family) -> Vec<String> {
    let ops: &[&str] = match f {
        Family::Rsa => &["encrypt", "decrypt", "sign", "verify", "wrapKey", "unwrapKey"],
        Family::Ec => &["sign", "verify"],
        Family::Oct => &["encrypt", "decrypt", "wrapKey", "unwrapKey", "sign", "verify"],
    };
    ops.iter().map(|s| s.to_string()).collect()
}

/// Validate and read a `key_ops` array. `None` when the field is absent.
pub fn parse_key_ops(body: &Map<String, Value>, field: &str) -> Result<Option<Vec<String>>, Response> {
    match body.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => {
            let mut out = Vec::new();
            for v in a {
                let Some(s) = v.as_str() else {
                    return Err(bad_parameter("Every key operation must be a string."));
                };
                match KEY_OPS.iter().find(|k| k.eq_ignore_ascii_case(s)) {
                    Some(k) => out.push((*k).to_string()),
                    None => return Err(bad_parameter(&format!("'{s}' is not a valid key operation."))),
                }
            }
            Ok(Some(out))
        }
        Some(_) => Err(bad_parameter("'key_ops' must be an array of strings.")),
    }
}

/// The public JWK for a key version.
pub fn public_jwk(kid: &str, kty: &str, key_ops: &[String], m: &KeyMaterial) -> Value {
    let mut o = Map::new();
    o.insert("kid".into(), json!(kid));
    o.insert("kty".into(), json!(kty));
    o.insert("key_ops".into(), json!(key_ops));
    match crypto::public_parts(m) {
        Ok(PublicParts::Rsa { n, e }) => {
            o.insert("n".into(), json!(b64u(&n)));
            o.insert("e".into(), json!(b64u(&e)));
        }
        Ok(PublicParts::Ec { crv, x, y }) => {
            o.insert("crv".into(), json!(crv.name()));
            o.insert("x".into(), json!(b64u(&x)));
            o.insert("y".into(), json!(b64u(&y)));
        }
        Ok(PublicParts::Oct) | Err(_) => {}
    }
    Value::Object(o)
}

fn field(jwk: &Map<String, Value>, name: &str) -> Result<Vec<u8>, Response> {
    match jwk.get(name) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::String(s)) => b64u_decode(s).ok_or_else(|| bad_parameter(&format!("The key field '{name}' is not valid base64url."))),
        Some(_) => Err(bad_parameter(&format!("The key field '{name}' must be a string."))),
    }
}

/// What an import (or a backup) carries.
pub struct ImportedKey {
    pub material: KeyMaterial,
    pub key_ops: Option<Vec<String>>,
    pub key_size: Option<u32>,
}

/// Build key material from the `key` object of an import request. Blocking for RSA
/// (key validation does big-number work); callers run it with `spawn_blocking`.
pub fn import_jwk(jwk: &Map<String, Value>, kty: &str) -> Result<ImportedKey, Response> {
    let (family, _) = parse_kty(kty).ok_or_else(|| bad_parameter(&format!("The key type '{kty}' is not supported.")))?;
    let key_ops = parse_key_ops(jwk, "key_ops")?;
    match family {
        Family::Rsa => {
            let (n, e, d, p, q) = (field(jwk, "n")?, field(jwk, "e")?, field(jwk, "d")?, field(jwk, "p")?, field(jwk, "q")?);
            if n.is_empty() || e.is_empty() {
                return Err(bad_parameter("An RSA key needs the 'n' and 'e' fields."));
            }
            if d.is_empty() {
                return Err(bad_parameter("The private key material ('d') is required to import an RSA key."));
            }
            let size = rsa_bits(&n);
            if !(2048..=4096).contains(&size) {
                return Err(bad_parameter("RSA key sizes between 2048 and 4096 bits are supported."));
            }
            let material = crypto::rsa_from_parts(&n, &e, &d, &p, &q).map_err(|e| bad_parameter(&format!("{e:?}")))?;
            Ok(ImportedKey { material, key_ops, key_size: Some(size) })
        }
        Family::Ec => {
            let crv = match jwk.get("crv").and_then(|c| c.as_str()) {
                Some(c) => Curve::parse(c).ok_or_else(|| bad_parameter(&format!("The curve '{c}' is not supported.")))?,
                None => return Err(bad_parameter("An EC key needs the 'crv' field.")),
            };
            let (x, y, d) = (field(jwk, "x")?, field(jwk, "y")?, field(jwk, "d")?);
            if d.is_empty() {
                return Err(bad_parameter("The private key material ('d') is required to import an EC key."));
            }
            let material = crypto::ec_from_parts(crv, &d, &x, &y).map_err(|e| bad_parameter(&format!("{e:?}")))?;
            Ok(ImportedKey { material, key_ops, key_size: None })
        }
        Family::Oct => {
            let k = field(jwk, "k")?;
            if !matches!(k.len(), 16 | 24 | 32) {
                return Err(bad_parameter("A symmetric key must be 128, 192 or 256 bits."));
            }
            let size = (k.len() * 8) as u32;
            Ok(ImportedKey { material: KeyMaterial::Oct(k), key_ops, key_size: Some(size) })
        }
    }
}

pub fn rsa_bits(n: &[u8]) -> u32 {
    let lead = n.iter().take_while(|b| **b == 0).count();
    let rest = &n[lead..];
    match rest.first() {
        Some(b) => ((rest.len() - 1) * 8 + (8 - b.leading_zeros() as usize)) as u32,
        None => 0,
    }
}

/// Private material as JSON for backups (never sent to clients).
pub fn material_to_json(m: &KeyMaterial) -> Value {
    match m {
        KeyMaterial::Rsa(k) => {
            let c = crypto::rsa_components(k);
            json!({"type": "rsa", "n": b64u(&c.n), "e": b64u(&c.e), "d": b64u(&c.d), "p": b64u(&c.p), "q": b64u(&c.q)})
        }
        KeyMaterial::Ec { curve, secret } => json!({"type": "ec", "crv": curve.name(), "d": b64u(secret)}),
        KeyMaterial::Oct(k) => json!({"type": "oct", "k": b64u(k)}),
    }
}

pub fn material_from_json(v: &Value) -> Option<KeyMaterial> {
    let o = v.as_object()?;
    let bytes = |n: &str| o.get(n).and_then(|x| x.as_str()).and_then(b64u_decode);
    match o.get("type")?.as_str()? {
        "rsa" => crypto::rsa_from_parts(&bytes("n")?, &bytes("e")?, &bytes("d")?, &bytes("p")?, &bytes("q")?).ok(),
        "ec" => {
            let curve = Curve::parse(o.get("crv")?.as_str()?)?;
            crypto::ec_from_parts(curve, &bytes("d")?, &[], &[]).ok()
        }
        "oct" => Some(KeyMaterial::Oct(bytes("k")?)),
        _ => None,
    }
}
