//! Backup blobs. Real Key Vault returns an opaque, encrypted blob that only the same
//! vault region can restore. Ours is a length-prefixed marker string (the same
//! prefixes real backups start with) followed by JSON, so a restore can tell a
//! secret backup from a key backup and reject garbage.

use azemu_core::Response;
use serde_json::Value;

use crate::common::{b64u, b64u_decode};
use crate::errors::{bad_parameter, Kind};

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

/// Parse the `value` of a restore request.
pub fn decode(kind: Kind, text: &str) -> Result<Value, Response> {
    let invalid = || bad_parameter("The backup blob is not valid.");
    let bytes = b64u_decode(text).ok_or_else(invalid)?;
    let m = marker(kind);
    if bytes.first().copied() != Some(m.len() as u8) || bytes.get(1..1 + m.len()) != Some(m.as_bytes()) {
        return Err(invalid());
    }
    serde_json::from_slice(&bytes[1 + m.len()..]).map_err(|_| invalid())
}
