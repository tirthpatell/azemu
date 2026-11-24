//! Strict authorisation (`docs/strict-auth.md`): this service is the authority that checks the tokens it issued, and it says which object
//! id a client id has, so that a role can be given to a client id without decoding a token.

use std::sync::Arc;

use azemu_core::auth::{Claims, Fault, TokenAuthority};
use azemu_core::resp::Resp;
use azemu_core::{json_error, Request, Response};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::jwt::app_oid;
use crate::msi::resolve_identity;
use crate::util::{default_tenant, unb64, Params};

/// base64url as a token signature is read: the unused bits of the last character are ignored, so that a changed last character is a
/// wrong signature and not a malformed token.
fn unb64_lenient(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let cfg = base64::engine::GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent);
    base64::engine::GeneralPurpose::new(&base64::alphabet::URL_SAFE, cfg).decode(s).ok()
}
use crate::{IdentityService, KeySlot};

/// Checks the signature of a token with the signing key of the service.
pub struct Authority {
    pub(crate) key: Arc<KeySlot>,
}

impl TokenAuthority for Authority {
    fn verify(&self, token: &str) -> Result<Claims, Fault> {
        let parts: Vec<&str> = token.trim().split('.').collect();
        if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
            return Err(Fault::Malformed);
        }
        let header: Value = unb64(parts[0]).and_then(|b| serde_json::from_slice(&b).ok()).ok_or(Fault::Malformed)?;
        let claims: Value = unb64(parts[1]).and_then(|b| serde_json::from_slice(&b).ok()).ok_or(Fault::Malformed)?;
        let sig = unb64_lenient(parts[2]).ok_or(Fault::Malformed)?;
        // A last character whose unused bits were changed decodes to the same bytes: it is still not the signature that was issued.
        if crate::util::b64(&sig) != parts[2].trim_end_matches('=') {
            return Err(Fault::BadSignature);
        }
        if header.get("alg").and_then(Value::as_str) != Some("RS256") {
            return Err(Fault::Malformed);
        }
        let Some(key) = self.key.current.lock().clone() else {
            return Err(Fault::BadSignature);
        };
        if header.get("kid").and_then(Value::as_str).is_some_and(|k| k != key.kid) {
            return Err(Fault::BadSignature);
        }
        let digest = Sha256::digest(format!("{}.{}", parts[0], parts[1]).as_bytes());
        if !key.verify_rs256(&digest, &sig) {
            return Err(Fault::BadSignature);
        }
        let s = |k: &str| claims.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let aud = match claims.get("aud") {
            Some(Value::String(a)) => a.clone(),
            Some(Value::Array(a)) => a.first().and_then(Value::as_str).unwrap_or("").to_string(),
            _ => String::new(),
        };
        let appid = if claims.get("appid").is_some() { s("appid") } else { s("azp") };
        let oid = if claims.get("oid").is_some() { s("oid") } else { s("sub") };
        Ok(Claims {
            oid,
            tid: s("tid"),
            appid,
            aud,
            iss: s("iss"),
            exp: claims.get("exp").and_then(Value::as_i64).unwrap_or(0),
            nbf: claims.get("nbf").and_then(Value::as_i64).unwrap_or(0),
            upn: claims.get("upn").and_then(Value::as_str).map(str::to_string),
        })
    }

    fn object_id_of_client(&self, client_id: &str) -> String {
        app_oid(client_id)
    }

    fn managed_identity(&self) -> (String, String) {
        let id = resolve_identity(None, None, None);
        let oid = app_oid(&id.client_id);
        (id.client_id, oid)
    }

    fn default_tenant(&self) -> String {
        default_tenant()
    }
}

impl IdentityService {
    /// What the other services check tokens with (`azemu_core::auth::Auth::set_authority`).
    pub fn token_authority(&self) -> Arc<dyn TokenAuthority> {
        Arc::new(Authority { key: self.key.clone() })
    }
}

/// `GET /_azemu/identity/principal[?client_id=|object_id=|mi_res_id=]`: the principal behind a client id, or the default managed identity
/// when nothing is named. The object id is what a token of that client carries as `oid`, and what a role assignment names.
pub fn principal(req: &Request) -> Response {
    if req.method != "GET" && req.method != "HEAD" {
        return json_error::response(405, "MethodNotAllowed", "Use GET.");
    }
    let p = Params::parse(req.query.raw());
    let (client, object, res) =
        (p.nonempty("client_id"), p.nonempty("object_id"), p.nonempty("mi_res_id").or_else(|| p.nonempty("msi_res_id")));
    if [client, object, res].iter().flatten().count() > 1 {
        return json_error::response(400, "InvalidParameter", "Name one of client_id, object_id and mi_res_id.");
    }
    let id = resolve_identity(client, object, res);
    let oid = id.oid.clone().unwrap_or_else(|| app_oid(&id.client_id));
    let kind = if client.is_some() {
        "clientId"
    } else if object.is_some() {
        "objectId"
    } else if res.is_some() {
        "resourceId"
    } else {
        "defaultManagedIdentity"
    };
    Resp::new(200u16).json(&json!({"clientId": id.client_id, "objectId": oid, "tenantId": default_tenant(), "named": kind})).build()
}
