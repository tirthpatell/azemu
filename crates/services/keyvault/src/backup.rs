//! Backup blobs. Real Key Vault returns an opaque, encrypted blob that only the same
//! vault region can restore. Ours is a length-prefixed marker string (the same
//! prefixes real backups start with) followed by JSON, so a restore can tell a
//! secret backup from a key backup and reject garbage.

use azemu_core::{json_error, Response};
use serde_json::{Map, Value};

use crate::common::{b64u, b64u_decode, invalid_property};
use crate::errors::Kind;

fn marker(kind: Kind) -> &'static str {
    match kind {
        Kind::Secret => "AzureKeyVaultSecretBackupV1.microsoft.com",
        Kind::Key => "AzureKeyVaultKeyBackupV1.microsoft.com",
        Kind::Certificate => "AzureKeyVaultCertificateBackupV1.microsoft.com",
    }
}

/// The base64url text for the `value` field of a backup response.
pub fn encode(kind: Kind, payload: &Value) -> String {
    let m = marker(kind);
    let mut bytes = vec![m.len() as u8];
    bytes.extend_from_slice(m.as_bytes());
    bytes.extend_from_slice(payload.to_string().as_bytes());
    b64u(&bytes)
}

/// What a restore says to a `value` that is not a backup of this vault (tried in November 2026): the words are the error code.
pub fn corrupt() -> Response {
    json_error::response(400, "Malformed backup blob", "Backup blob is corrupt.")
}

/// The `value` of a restore request: text, and not empty.
pub fn value_of(body: &Map<String, Value>) -> Result<String, Response> {
    match body.get("value") {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        _ => Err(invalid_property("")),
    }
}

/// Parse the `value` of a restore request.
pub fn decode(kind: Kind, text: &str) -> Result<Value, Response> {
    let invalid = corrupt;
    let bytes = b64u_decode(text).ok_or_else(invalid)?;
    let m = marker(kind);
    if bytes.first().copied() != Some(m.len() as u8) || bytes.get(1..1 + m.len()) != Some(m.as_bytes()) {
        return Err(invalid());
    }
    serde_json::from_slice(&bytes[1 + m.len()..]).map_err(|_| invalid())
}
