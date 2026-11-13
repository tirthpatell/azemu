//! Small helpers: form parsing, base64url, deterministic GUIDs.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use percent_encoding::percent_decode_str;
use sha2::{Digest, Sha256};

pub fn b64(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

pub fn unb64(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

/// `application/x-www-form-urlencoded` pairs (`+` is a space). Used for the
/// request body and for query strings, because scopes contain spaces.
#[derive(Default, Debug)]
pub struct Params(Vec<(String, String)>);

impl Params {
    pub fn parse(raw: &str) -> Params {
        let dec = |s: &str| percent_decode_str(&s.replace('+', " ")).decode_utf8_lossy().into_owned();
        let pairs = raw
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| match kv.split_once('=') {
                Some((k, v)) => (dec(k), dec(v)),
                None => (dec(kv), String::new()),
            })
            .collect();
        Params(pairs)
    }

    /// First value for `name` (case-insensitive name), empty values included.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// First value for `name`, `None` when absent or empty.
    pub fn nonempty(&self, name: &str) -> Option<&str> {
        self.get(name).filter(|v| !v.is_empty())
    }
}

/// A stable GUID derived from a namespace and a name (SHA-256, UUID v5 layout).
pub fn derive_guid(ns: &str, name: &str) -> String {
    let mut h = Sha256::new();
    h.update(ns.as_bytes());
    h.update([0u8]);
    h.update(name.as_bytes());
    let d = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(b).to_string()
}

pub fn is_guid(s: &str) -> bool {
    s.len() == 36 && uuid::Uuid::parse_str(s).is_ok()
}

pub fn new_guid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The tenant id (a GUID) for whatever tenant segment the client used.
/// A GUID is kept; `common`, `organizations`, `consumers` and an empty value map
/// to the default tenant; any other name maps to a stable GUID.
pub fn tenant_guid(tenant: &str) -> String {
    let t = tenant.trim().to_ascii_lowercase();
    if is_guid(&t) {
        t
    } else if t.is_empty() || matches!(t.as_str(), "common" | "organizations" | "consumers") {
        default_tenant()
    } else {
        derive_guid("tenant", &t)
    }
}

pub fn default_tenant() -> String {
    derive_guid("tenant", "azemu-default")
}

/// `2026-01-15 10:00:00Z`, the timestamp format Entra puts in error bodies.
pub fn entra_timestamp(t: azemu_core::time::OffsetDateTime) -> String {
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_decoding() {
        let p = Params::parse("scope=https%3A%2F%2Fa.b%2F.default+openid&x=&flag");
        assert_eq!(p.get("scope"), Some("https://a.b/.default openid"));
        assert_eq!(p.get("SCOPE"), Some("https://a.b/.default openid"));
        assert_eq!(p.nonempty("x"), None);
        assert_eq!(p.get("x"), Some(""));
        assert_eq!(p.get("flag"), Some(""));
    }

    #[test]
    fn guids_are_stable() {
        assert_eq!(derive_guid("a", "b"), derive_guid("a", "b"));
        assert_ne!(derive_guid("a", "b"), derive_guid("a", "c"));
        assert!(is_guid(&derive_guid("a", "b")));
        assert_eq!(tenant_guid("common"), tenant_guid("organizations"));
        assert_eq!(tenant_guid("Contoso.com"), tenant_guid("contoso.com"));
        let g = "11111111-2222-3333-4444-555555555555";
        assert_eq!(tenant_guid(&g.to_uppercase()), g);
    }
}
