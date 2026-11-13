//! X.509 work for certificates: building self-signed certificates and CSRs from a
//! policy (in `cert_build`), reading imported certificates, PEM and PKCS#12 in both
//! directions. Pure Rust (x509-parser for reading, p12-keystore).

use std::net::IpAddr;

use serde_json::{json, Map, Value};
use sha1::{Digest, Sha1};
use x509_parser::asn1_rs::{Any, FromBer};
use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};

use crate::common::{b64u, b64u_decode};
use crate::crypto::{Curve, KeyMaterial};

type R<T> = Result<T, String>;

pub use crate::cert_build::{build, validate_subject, Built};

/// What we read out of a certificate.
pub struct Parsed {
    pub subject: String,
    pub not_before: i64,
    pub not_after: i64,
    pub sans: Value,
    pub ekus: Vec<String>,
    pub key_usage: Vec<String>,
    pub is_ca: bool,
    /// `RSA` or `EC`, with the key size (RSA) or curve (EC).
    pub kty: &'static str,
    pub key_size: Option<u32>,
    pub curve: Option<Curve>,
    pub spki: Vec<u8>,
}

pub fn thumbprint(der: &[u8]) -> String {
    b64u(&Sha1::digest(der))
}

// ---------------------------------------------------------------------------
// PEM
// ---------------------------------------------------------------------------

pub fn pem_encode(label: &str, der: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    let b = STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// All `(label, DER)` blocks in a PEM text.
pub fn pem_blocks(text: &str) -> Vec<(String, Vec<u8>)> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("-----BEGIN ") {
        let after = &rest[start + 11..];
        let Some(label_end) = after.find("-----") else { break };
        let label = after[..label_end].to_string();
        let body_start = &after[label_end + 5..];
        let end_marker = format!("-----END {label}-----");
        let Some(end) = body_start.find(&end_marker) else { break };
        let b64: String = body_start[..end].chars().filter(|c| !c.is_whitespace()).collect();
        if let Ok(der) = STANDARD.decode(b64.as_bytes()) {
            out.push((label, der));
        }
        rest = &body_start[end + end_marker.len()..];
    }
    out
}

// ---------------------------------------------------------------------------
// Key material <-> PKCS#8
// ---------------------------------------------------------------------------

macro_rules! per_ec_curve {
    ($curve:expr, $m:ident, $body:block) => {
        match $curve {
            Curve::P256 => {
                use p256 as $m;
                $body
            }
            Curve::P384 => {
                use p384 as $m;
                $body
            }
            Curve::P521 => {
                use p521 as $m;
                $body
            }
            Curve::P256K => {
                use k256 as $m;
                $body
            }
        }
    };
}

pub fn pkcs8_der(m: &KeyMaterial) -> R<Vec<u8>> {
    use rsa::pkcs8::EncodePrivateKey;
    match m {
        KeyMaterial::Rsa(k) => Ok(k.to_pkcs8_der().map_err(|e| e.to_string())?.as_bytes().to_vec()),
        KeyMaterial::Ec { curve, secret } => per_ec_curve!(*curve, c, {
            use c::pkcs8::EncodePrivateKey as _;
            let sk = c::SecretKey::from_slice(secret).map_err(|e| e.to_string())?;
            Ok(sk.to_pkcs8_der().map_err(|e| e.to_string())?.as_bytes().to_vec())
        }),
        KeyMaterial::Oct(_) => Err("a symmetric key cannot back a certificate".into()),
    }
}

/// Key material from a PKCS#8 private key (RSA or one of our curves).
pub fn material_from_pkcs8(der: &[u8]) -> R<KeyMaterial> {
    use rsa::pkcs8::DecodePrivateKey;
    if let Ok(k) = rsa::PrivateKey::from_pkcs8_der(der) {
        return Ok(KeyMaterial::Rsa(std::sync::Arc::new(k)));
    }
    for curve in [Curve::P256, Curve::P384, Curve::P521, Curve::P256K] {
        let secret: Option<Vec<u8>> = per_ec_curve!(curve, c, {
            use c::pkcs8::DecodePrivateKey as _;
            c::SecretKey::from_pkcs8_der(der).ok().map(|k| k.to_bytes().to_vec())
        });
        if let Some(secret) = secret {
            return Ok(KeyMaterial::Ec { curve, secret });
        }
    }
    Err("the private key is not a supported RSA or EC PKCS#8 key".into())
}

/// A private key out of a PEM label + DER.
pub fn material_from_pem_block(label: &str, der: &[u8]) -> R<KeyMaterial> {
    match label {
        "PRIVATE KEY" => material_from_pkcs8(der),
        "RSA PRIVATE KEY" => {
            use rsa::pkcs1::DecodeRsaPrivateKey;
            let k = rsa::PrivateKey::from_pkcs1_der(der).map_err(|e| e.to_string())?;
            Ok(KeyMaterial::Rsa(std::sync::Arc::new(k)))
        }
        "EC PRIVATE KEY" => {
            for curve in [Curve::P256, Curve::P384, Curve::P521, Curve::P256K] {
                let secret: Option<Vec<u8>> =
                    per_ec_curve!(curve, c, { c::SecretKey::from_sec1_der(der).ok().map(|k| k.to_bytes().to_vec()) });
                if let Some(secret) = secret {
                    return Ok(KeyMaterial::Ec { curve, secret });
                }
            }
            Err("the EC private key uses an unsupported curve".into())
        }
        "ENCRYPTED PRIVATE KEY" => Err("encrypted PEM private keys are not supported".into()),
        other => Err(format!("unsupported PEM block '{other}'")),
    }
}

// ---------------------------------------------------------------------------
// Reading certificates
// ---------------------------------------------------------------------------

fn short_name(oid: &str) -> Option<&'static str> {
    Some(match oid {
        "2.5.4.3" => "CN",
        "2.5.4.10" => "O",
        "2.5.4.11" => "OU",
        "2.5.4.6" => "C",
        "2.5.4.7" => "L",
        "2.5.4.8" => "S",
        "1.2.840.113549.1.9.1" => "E",
        "0.9.2342.19200300.100.1.25" => "DC",
        _ => return None,
    })
}

pub fn parse_cert(der: &[u8]) -> R<Parsed> {
    let (_, cert) = X509Certificate::from_der(der).map_err(|e| format!("not a valid X.509 certificate: {e}"))?;
    let mut parts: Vec<String> = Vec::new();
    for rdn in cert.subject().iter_rdn() {
        for attr in rdn.iter() {
            if let (Some(n), Ok(v)) = (short_name(&attr.attr_type().to_id_string()), attr.as_str()) {
                parts.push(format!("{n}={v}"));
            }
        }
    }
    parts.reverse();
    let mut sans = Map::new();
    if let Ok(Some(ext)) = cert.subject_alternative_name() {
        let mut push = |key: &str, v: String| {
            if let Some(a) = sans.entry(key.to_string()).or_insert_with(|| json!([])).as_array_mut() {
                a.push(json!(v));
            }
        };
        for g in &ext.value.general_names {
            match g {
                GeneralName::DNSName(d) => push("dns_names", d.to_string()),
                GeneralName::RFC822Name(e) => push("emails", e.to_string()),
                GeneralName::URI(u) => push("uris", u.to_string()),
                GeneralName::OtherName(o, raw) if o.to_id_string() == "1.3.6.1.4.1.311.20.2.3" => {
                    if let Some(upn) = upn_text(raw) {
                        push("upns", upn);
                    }
                }
                GeneralName::IPAddress(b) => {
                    let ip = match b.len() {
                        4 => Some(IpAddr::from([b[0], b[1], b[2], b[3]])),
                        16 => <[u8; 16]>::try_from(*b).ok().map(IpAddr::from),
                        _ => None,
                    };
                    if let Some(ip) = ip {
                        push("ipAddresses", ip.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    let mut ekus = Vec::new();
    if let Ok(Some(e)) = cert.extended_key_usage() {
        let e = e.value;
        if e.server_auth {
            ekus.push("1.3.6.1.5.5.7.3.1".to_string());
        }
        if e.client_auth {
            ekus.push("1.3.6.1.5.5.7.3.2".to_string());
        }
        if e.code_signing {
            ekus.push("1.3.6.1.5.5.7.3.3".to_string());
        }
        if e.email_protection {
            ekus.push("1.3.6.1.5.5.7.3.4".to_string());
        }
        if e.time_stamping {
            ekus.push("1.3.6.1.5.5.7.3.8".to_string());
        }
        if e.ocsp_signing {
            ekus.push("1.3.6.1.5.5.7.3.9".to_string());
        }
        ekus.extend(e.other.iter().map(|o| o.to_id_string()));
    }
    let mut ku = Vec::new();
    if let Ok(Some(k)) = cert.key_usage() {
        let k = k.value;
        for (on, name) in [
            (k.digital_signature(), "digitalSignature"),
            (k.non_repudiation(), "nonRepudiation"),
            (k.key_encipherment(), "keyEncipherment"),
            (k.data_encipherment(), "dataEncipherment"),
            (k.key_agreement(), "keyAgreement"),
            (k.key_cert_sign(), "keyCertSign"),
            (k.crl_sign(), "cRLSign"),
            (k.encipher_only(), "encipherOnly"),
            (k.decipher_only(), "decipherOnly"),
        ] {
            if on {
                ku.push(name.to_string());
            }
        }
    }
    let is_ca = cert.basic_constraints().ok().flatten().map(|b| b.value.ca).unwrap_or(false);
    let spki = cert.public_key();
    let alg_oid = spki.algorithm.algorithm.to_id_string();
    let (kty, key_size, curve) = if alg_oid == "1.2.840.113549.1.1.1" {
        let bits = match spki.parsed() {
            Ok(x509_parser::public_key::PublicKey::RSA(r)) => {
                let m = r.modulus;
                let lead = m.iter().take_while(|b| **b == 0).count();
                crate::jwk::rsa_bits(&m[lead..])
            }
            _ => 0,
        };
        ("RSA", Some(bits), None)
    } else {
        let curve_oid = spki.algorithm.parameters.as_ref().and_then(|p| p.as_oid().ok()).map(|o| o.to_id_string()).unwrap_or_default();
        let curve = match curve_oid.as_str() {
            "1.2.840.10045.3.1.7" => Some(Curve::P256),
            "1.3.132.0.34" => Some(Curve::P384),
            "1.3.132.0.35" => Some(Curve::P521),
            "1.3.132.0.10" => Some(Curve::P256K),
            _ => None,
        };
        ("EC", None, curve)
    };
    Ok(Parsed {
        subject: parts.join(", "),
        not_before: cert.validity().not_before.timestamp(),
        not_after: cert.validity().not_after.timestamp(),
        sans: Value::Object(sans),
        ekus,
        key_usage: ku,
        is_ca,
        kty,
        key_size,
        curve,
        spki: spki.raw.to_vec(),
    })
}

/// The SubjectPublicKeyInfo DER of the public half of `m`, for comparing a merged
/// certificate with the key.
pub fn spki_of_material(m: &KeyMaterial) -> R<Vec<u8>> {
    crate::cert_build::spki_der(m)
}

/// The text of a UPN `otherName` value (`[0] EXPLICIT UTF8String`).
fn upn_text(raw: &[u8]) -> Option<String> {
    let (_, any) = Any::from_ber(raw).ok()?;
    let inner = if any.header.tag().0 == 0 && any.header.class() == x509_parser::asn1_rs::Class::ContextSpecific {
        Any::from_ber(any.data).ok()?.1
    } else {
        any
    };
    String::from_utf8(inner.data.to_vec()).ok()
}

// ---------------------------------------------------------------------------
// PKCS#12
// ---------------------------------------------------------------------------

pub fn pkcs12_export(m: &KeyMaterial, chain: &[Vec<u8>]) -> R<Vec<u8>> {
    use p12_keystore::{Certificate, KeyStore, KeyStoreEntry, LocalKeyId, PrivateKey, PrivateKeyChain};
    let key = PrivateKey::from_der(&pkcs8_der(m)?).map_err(|e| e.to_string())?;
    let certs: Vec<Certificate> = chain.iter().map(|d| Certificate::from_der(d).map_err(|e| e.to_string())).collect::<R<_>>()?;
    let mut store = KeyStore::new();
    store.add_entry("1", KeyStoreEntry::PrivateKeyChain(PrivateKeyChain::new(LocalKeyId(vec![1]), key, certs)));
    store.writer("").write().map_err(|e| e.to_string())
}

/// Private key and certificates (leaf first) from a PFX. Encrypted stores go through
/// p12-keystore; unencrypted ones (which it does not read) through a small BER walk.
pub fn pkcs12_import(data: &[u8], password: &str) -> R<(KeyMaterial, Vec<Vec<u8>>)> {
    use p12_keystore::{KeyStore, Pkcs12ImportPolicy};
    match KeyStore::from_pkcs12(data, password, Pkcs12ImportPolicy::Strict) {
        Ok(store) => {
            if let Some((_, chain)) = store.private_key_chain() {
                let m = material_from_pkcs8(chain.key().as_der())?;
                let certs = chain.certs().iter().map(|c| c.as_der().to_vec()).collect();
                return Ok((m, certs));
            }
        }
        Err(e) => {
            if let Ok(r) = plain_pkcs12(data) {
                return Ok(r);
            }
            return Err(format!("cannot read the PKCS#12 file (wrong password?): {e}"));
        }
    }
    plain_pkcs12(data)
}

fn ber_children(data: &[u8]) -> Vec<Any<'_>> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        match Any::from_ber(rest) {
            Ok((r, a)) => {
                out.push(a);
                rest = r;
            }
            Err(_) => break,
        }
    }
    out
}

fn oid_of(a: &Any<'_>) -> String {
    a.clone().as_oid().map(|o| o.to_id_string()).unwrap_or_default()
}

/// First child of a context-specific `[0] EXPLICIT` wrapper.
fn explicit0<'a>(a: &Any<'a>) -> Option<Any<'a>> {
    ber_children(a.data).into_iter().next()
}

fn plain_pkcs12(data: &[u8]) -> R<(KeyMaterial, Vec<Vec<u8>>)> {
    let bad = || "unsupported or invalid PKCS#12 structure".to_string();
    let pfx = ber_children(data).into_iter().next().ok_or_else(bad)?;
    let items = ber_children(pfx.data);
    let auth_safe = items.get(1).ok_or_else(bad)?; // ContentInfo
    let ci = ber_children(auth_safe.data);
    let content = ci.get(1).and_then(explicit0).ok_or_else(bad)?; // OCTET STRING
    let mut key: Option<KeyMaterial> = None;
    let mut certs: Vec<Vec<u8>> = Vec::new();
    for info in ber_children(ber_children(content.data).first().map(|s| s.data).unwrap_or(&[])) {
        let parts = ber_children(info.data);
        if parts.first().map(oid_of).as_deref() != Some("1.2.840.113549.1.7.1") {
            return Err(bad());
        }
        let Some(octets) = parts.get(1).and_then(explicit0) else { continue };
        let Some(contents) = ber_children(octets.data).into_iter().next() else { continue };
        for bag in ber_children(contents.data) {
            let b = ber_children(bag.data);
            let (Some(id), Some(value)) = (b.first().map(oid_of), b.get(1).and_then(explicit0)) else { continue };
            match id.as_str() {
                "1.2.840.113549.1.12.10.1.1" => {
                    // keyBag: the PKCS#8 structure itself
                    let mut der = vec![0x30];
                    push_len(&mut der, value.data.len());
                    der.extend_from_slice(value.data);
                    key = Some(material_from_pkcs8(&der)?);
                }
                "1.2.840.113549.1.12.10.1.3" => {
                    let cb = ber_children(value.data);
                    if let Some(c) = cb.get(1).and_then(explicit0) {
                        certs.push(c.data.to_vec());
                    }
                }
                _ => {}
            }
        }
    }
    match key {
        Some(k) if !certs.is_empty() => Ok((k, certs)),
        _ => Err(bad()),
    }
}

fn push_len(out: &mut Vec<u8>, n: usize) {
    if n < 0x80 {
        out.push(n as u8);
    } else {
        let bytes = n.to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
}

/// Decode base64 (standard or url alphabet) of an import request's `value`.
pub fn decode_value(text: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    STANDARD.decode(text.trim().as_bytes()).ok().or_else(|| b64u_decode(text.trim()))
}

/// A certificate's DER from bytes that may be DER already, PEM, or base64 text of the
/// DER (the Python SDK sends the base64 text of a certificate base64-encoded again).
pub fn certificate_der(bytes: Vec<u8>) -> Option<Vec<u8>> {
    if bytes.first() == Some(&0x30) {
        return Some(bytes);
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.contains("-----BEGIN") {
        return pem_blocks(&text).into_iter().find(|(l, _)| l == "CERTIFICATE").map(|(_, d)| d);
    }
    let der = decode_value(&text)?;
    (der.first() == Some(&0x30)).then_some(der)
}
