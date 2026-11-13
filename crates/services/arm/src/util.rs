//! Small helpers shared by the handlers.

use azemu_core::resp::Resp;
use azemu_core::Response;
use serde_json::{json, Map, Value};

use crate::error::{ArmError, ArmResult};

pub fn json_ok(status: u16, v: &Value) -> Response {
    Resp::new(status).json(v).build()
}

pub fn empty(status: u16) -> Response {
    Resp::new(status).build()
}

pub fn list(items: Vec<Value>) -> Response {
    json_ok(200, &json!({ "value": items }))
}

/// Parse a request body that must be a JSON object. An empty body is an empty object.
pub fn body_object(body: &[u8]) -> ArmResult<Value> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(Value::Object(Map::new()));
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(_) => Err(ArmError::bad_request(
            "InvalidRequestContent",
            "The request content was invalid and could not be deserialized: 'Error converting value to a JSON object.'.",
        )),
        Err(e) => Err(ArmError::bad_request(
            "InvalidRequestContent",
            format!("The request content was invalid and could not be deserialized: '{e}'."),
        )),
    }
}

/// Merge `patch` into `base`: objects merge key by key, anything else replaces.
pub fn deep_merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                if v.is_null() {
                    b.remove(k);
                } else {
                    deep_merge(b.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

/// Make sure `v["properties"]` is an object and return it.
pub fn props_mut(v: &mut Value) -> &mut Map<String, Value> {
    if !v.get("properties").map(Value::is_object).unwrap_or(false) {
        v["properties"] = Value::Object(Map::new());
    }
    match v.get_mut("properties") {
        Some(Value::Object(m)) => m,
        _ => unreachable!("properties was just made an object"),
    }
}

/// Fill in a default for a property that is missing or `null` (the azurerm provider sends `"backupPolicy": null`).
pub fn set_default(m: &mut Map<String, Value>, k: &str, v: Value) {
    if m.get(k).map(Value::is_null).unwrap_or(true) {
        m.insert(k.to_string(), v);
    }
}

/// 24 random-looking but stable base32-ish characters from any text.
pub fn short_hash(text: &str, len: usize) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for b in digest.iter().cycle().take(64) {
        acc = (acc << 8) | *b as u32;
        bits += 8;
        while bits >= 5 && out.len() < len {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
        if out.len() >= len {
            break;
        }
    }
    out
}

/// A 32-byte base64 key (44 characters) derived from a seed. Stable for a seed, so keys
/// survive a restart of the emulator; a different `generation` gives a new key.
pub fn derive_key(seed: &str, generation: u64) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("azemu-arm-key/{seed}/{generation}").as_bytes());
    base64::engine::general_purpose::STANDARD.encode(digest)
}

/// Name check result in the shape every provider's `checkNameAvailability` uses.
pub fn name_availability(available: bool, reason: &str, message: &str) -> Value {
    if available {
        json!({ "nameAvailable": true })
    } else {
        json!({ "nameAvailable": false, "reason": reason, "message": message })
    }
}

/// A 64-byte base64 key (88 characters), the size of a Cosmos DB or storage account key.
pub fn derive_key64(seed: &str, generation: u64) -> String {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    let mut bytes = engine.decode(derive_key(&format!("{seed}/a"), generation)).unwrap_or_default();
    bytes.extend(engine.decode(derive_key(&format!("{seed}/b"), generation)).unwrap_or_default());
    engine.encode(bytes)
}

/// Stands for the port of the address a client used (`:4577`) in values ARM stores. A stored value must not hold the port the
/// emulator listened on when it was written, because the next start may use another one: the port is taken out
/// when a body is stored ([`without_port`]) and put back from the request that asks for it ([`fill_port`]).
pub const PORT_TOKEN: &str = "{azemu-port}";

/// `:4577` taken from the authority the client used, or empty when it sent no port.
pub fn port_suffix_of(authority: &str) -> String {
    match authority.rsplit_once(':') {
        Some((_, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => format!(":{p}"),
        _ => String::new(),
    }
}

/// Replaces `.localhost:<port>` in every string of `v` by `.localhost{azemu-port}` (all the addresses the emulator hands out
/// are `<account>.<service>.localhost:<port>`). `suffix` is the port suffix of the request (`:4577`, or empty: nothing to do).
pub fn without_port(v: &mut Value, suffix: &str) {
    if suffix.is_empty() {
        return;
    }
    let from = format!(".localhost{suffix}");
    let to = format!(".localhost{PORT_TOKEN}");
    fn walk(v: &mut Value, from: &str, to: &str) {
        match v {
            Value::String(s) => {
                // the port must end there: `.localhost:4577` and not the start of `.localhost:45771`
                let mut out = String::new();
                let mut rest = s.as_str();
                let mut changed = false;
                while let Some(i) = rest.find(from) {
                    let after = &rest[i + from.len()..];
                    if after.starts_with(|c: char| c.is_ascii_digit()) {
                        out.push_str(&rest[..i + from.len()]);
                    } else {
                        out.push_str(&rest[..i]);
                        out.push_str(to);
                        changed = true;
                    }
                    rest = after;
                }
                if changed {
                    out.push_str(rest);
                    *s = out;
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| walk(x, from, to)),
            Value::Object(m) => m.values_mut().for_each(|x| walk(x, from, to)),
            _ => {}
        }
    }
    walk(v, &from, &to);
}

/// The answer with [`PORT_TOKEN`] replaced by the port suffix of the request that asked; `None` when it holds no token.
pub fn fill_port(body: &[u8], suffix: &str) -> Option<Vec<u8>> {
    let token = PORT_TOKEN.as_bytes();
    let first = body.windows(token.len()).position(|w| w == token)?;
    let mut out = Vec::with_capacity(body.len());
    out.extend_from_slice(&body[..first]);
    let mut i = first;
    while i < body.len() {
        if body[i..].starts_with(token) {
            out.extend_from_slice(suffix.as_bytes());
            i += token.len();
        } else {
            out.push(body[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod port_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_port_is_taken_out_and_put_back() {
        let mut v = json!({"a": "https://ns.servicebus.localhost:4577/", "b": ["http://x.blob.core.localhost:4577/c"], "n": 1, "c": "https://ns.servicebus.localhost:45771/", "d": "no address"});
        without_port(&mut v, ":4577");
        assert_eq!(v["a"], "https://ns.servicebus.localhost{azemu-port}/");
        assert_eq!(v["b"][0], "http://x.blob.core.localhost{azemu-port}/c");
        assert_eq!(v["c"], "https://ns.servicebus.localhost:45771/", "another port is left alone");
        let text = v.to_string();
        let filled = fill_port(text.as_bytes(), ":9999").unwrap();
        let back: Value = serde_json::from_slice(&filled).unwrap();
        assert_eq!(back["a"], "https://ns.servicebus.localhost:9999/");
        assert_eq!(back["b"][0], "http://x.blob.core.localhost:9999/c");
        // a request on a host without a port gets none
        let filled = fill_port(text.as_bytes(), "").unwrap();
        assert!(String::from_utf8(filled).unwrap().contains("servicebus.localhost/"));
        assert!(fill_port(b"{}", ":1").is_none());
        let mut same = json!({"a": "x"});
        without_port(&mut same, "");
        assert_eq!(same, json!({"a": "x"}));
    }
}
