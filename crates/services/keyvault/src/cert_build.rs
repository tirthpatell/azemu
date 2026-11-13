//! Building X.509 certificates and PKCS#10 requests from a certificate policy.
//!
//! The DER is written by hand so the certificate has exactly the profile Key Vault
//! issues for a self-signed policy (recorded in the certificates recordings):
//! version 3, a 16 byte serial number, `sha256WithRSAEncryption` (or ECDSA with the
//! hash that goes with the curve), issuer = subject, and the extensions in the order
//! key usage (critical), basic constraints (not critical), extended key usage,
//! subject alternative name (critical only when the subject is empty), authority key
//! identifier and subject key identifier (both the SHA-1 of the public key).
//! Signing uses the same pure Rust code as the key operations (`crypto::sign`), which
//! is also what makes ECDSA with P-521 and secp256k1 possible.

use std::net::IpAddr;

use serde_json::{json, Value};
use sha1::{Digest as _, Sha1};
use sha2::{Sha256, Sha384, Sha512};
use time::OffsetDateTime;

use crate::crypto::{self, Curve, KeyMaterial, PublicParts};

type R<T> = Result<T, String>;

pub struct Built {
    pub cert_der: Vec<u8>,
    pub csr_der: Vec<u8>,
}

// ---------------------------------------------------------------------------
// DER writing
// ---------------------------------------------------------------------------

fn len_bytes(n: usize) -> Vec<u8> {
    if n < 0x80 {
        vec![n as u8]
    } else {
        let b = n.to_be_bytes();
        let skip = b.iter().take_while(|x| **x == 0).count();
        let mut out = vec![0x80 | (b.len() - skip) as u8];
        out.extend_from_slice(&b[skip..]);
        out
    }
}

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend(len_bytes(content.len()));
    out.extend_from_slice(content);
    out
}

fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}

fn set(parts: &[Vec<u8>]) -> Vec<u8> {
    tlv(0x31, &parts.concat())
}

/// A non-negative INTEGER from big-endian bytes.
fn integer(be: &[u8]) -> Vec<u8> {
    let start = be.iter().position(|b| *b != 0).unwrap_or(be.len().saturating_sub(1));
    let mut v = be[start..].to_vec();
    if v.is_empty() {
        v.push(0);
    }
    if v[0] & 0x80 != 0 {
        v.insert(0, 0);
    }
    tlv(0x02, &v)
}

fn oid(dotted: &str) -> R<Vec<u8>> {
    let arcs: Vec<u64> =
        dotted.split('.').map(|p| p.parse::<u64>().map_err(|_| format!("'{dotted}' is not an object identifier"))).collect::<R<_>>()?;
    if arcs.len() < 2 || arcs[0] > 2 || (arcs[0] < 2 && arcs[1] >= 40) {
        return Err(format!("'{dotted}' is not an object identifier"));
    }
    let mut body = Vec::new();
    let mut push = |mut n: u64| {
        let mut tmp = vec![(n & 0x7f) as u8];
        n >>= 7;
        while n > 0 {
            tmp.push(((n & 0x7f) as u8) | 0x80);
            n >>= 7;
        }
        tmp.reverse();
        body.extend(tmp);
    };
    push(arcs[0] * 40 + arcs[1]);
    for a in &arcs[2..] {
        push(*a);
    }
    Ok(tlv(0x06, &body))
}

fn oid_const(dotted: &str) -> Vec<u8> {
    oid(dotted).unwrap_or_default()
}

fn bit_string(bytes: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8];
    v.extend_from_slice(bytes);
    tlv(0x03, &v)
}

fn octet_string(bytes: &[u8]) -> Vec<u8> {
    tlv(0x04, bytes)
}

fn boolean_true() -> Vec<u8> {
    vec![0x01, 0x01, 0xff]
}

fn null() -> Vec<u8> {
    vec![0x05, 0x00]
}

fn time_der(t: OffsetDateTime) -> Vec<u8> {
    let t = t.to_offset(time::UtcOffset::UTC);
    if (1950..2050).contains(&t.year()) {
        let s = format!("{:02}{:02}{:02}{:02}{:02}{:02}Z", t.year() % 100, t.month() as u8, t.day(), t.hour(), t.minute(), t.second());
        tlv(0x17, s.as_bytes())
    } else {
        let s = format!("{:04}{:02}{:02}{:02}{:02}{:02}Z", t.year(), t.month() as u8, t.day(), t.hour(), t.minute(), t.second());
        tlv(0x18, s.as_bytes())
    }
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum StrKind {
    /// PrintableString when the characters allow it, UTF8String otherwise
    Text,
    /// always PrintableString (country, serial number)
    Printable,
    /// IA5String (e-mail, domain component)
    Ia5,
}

fn attribute_type(name: &str) -> Option<(String, StrKind)> {
    let t = name.trim();
    let upper = t.to_ascii_uppercase();
    let (oid, kind) = match upper.as_str() {
        "CN" => ("2.5.4.3", StrKind::Text),
        "SN" => ("2.5.4.4", StrKind::Text),
        "SERIALNUMBER" => ("2.5.4.5", StrKind::Printable),
        "C" => ("2.5.4.6", StrKind::Printable),
        "L" => ("2.5.4.7", StrKind::Text),
        "S" | "ST" => ("2.5.4.8", StrKind::Text),
        "STREET" => ("2.5.4.9", StrKind::Text),
        "O" => ("2.5.4.10", StrKind::Text),
        "OU" => ("2.5.4.11", StrKind::Text),
        "T" | "TITLE" => ("2.5.4.12", StrKind::Text),
        "G" | "GN" | "GIVENNAME" => ("2.5.4.42", StrKind::Text),
        "I" | "INITIALS" => ("2.5.4.43", StrKind::Text),
        "E" | "EMAIL" | "EMAILADDRESS" => ("1.2.840.113549.1.9.1", StrKind::Ia5),
        "DC" => ("0.9.2342.19200300.100.1.25", StrKind::Ia5),
        "UID" => ("0.9.2342.19200300.100.1.1", StrKind::Text),
        _ => {
            // a dotted object identifier is allowed too
            let dotted = t.strip_prefix("OID.").or_else(|| t.strip_prefix("oid.")).unwrap_or(t);
            if dotted.contains('.') && oid_check(dotted) {
                return Some((dotted.to_string(), StrKind::Text));
            }
            return None;
        }
    };
    Some((oid.to_string(), kind))
}

fn oid_check(d: &str) -> bool {
    oid(d).is_ok()
}

/// Split at the separators that are not escaped with a backslash.
fn split_unescaped(s: &str, sep: char) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut escaped = false;
    for ch in s.chars() {
        if escaped {
            if let Some(last) = out.last_mut() {
                last.push('\\');
                last.push(ch);
            }
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == sep {
            out.push(String::new());
        } else if let Some(last) = out.last_mut() {
            last.push(ch);
        }
    }
    if escaped {
        if let Some(last) = out.last_mut() {
            last.push('\\');
        }
    }
    out
}

fn unescape(v: &str) -> String {
    let v = v.trim();
    let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(v);
    let mut out = String::new();
    let mut escaped = false;
    for ch in v.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    out
}

/// The attributes of a subject string (`CN=a, O=b`) as `(OID, kind, value)`, in the order
/// they appear in the DER (RFC 4514 lists the most specific part first, the DER the last).
/// A string that is not a distinguished name is an error.
fn parse_subject(subject: &str) -> R<Vec<(String, StrKind, String)>> {
    let invalid =
        || format!("The subject '{subject}' is not a valid X.500 distinguished name (expected for example 'CN=example.com, O=Contoso').");
    let mut out = Vec::new();
    if subject.trim().is_empty() {
        return Ok(out);
    }
    for part in split_unescaped(subject, ',') {
        if part.trim().is_empty() {
            return Err(invalid());
        }
        let pieces = split_unescaped(&part, '=');
        if pieces.len() < 2 {
            return Err(invalid());
        }
        let key = &pieces[0];
        let value = pieces[1..].join("=");
        let Some((oid, kind)) = attribute_type(key) else { return Err(invalid()) };
        let value = unescape(&value);
        if value.is_empty() || (kind == StrKind::Printable && !is_printable(&value)) || (kind == StrKind::Ia5 && !value.is_ascii()) {
            return Err(invalid());
        }
        out.push((oid, kind, value));
    }
    out.reverse();
    Ok(out)
}

/// Check a subject string; `Err` carries the message for a `400 BadParameter`.
pub fn validate_subject(subject: &str) -> R<()> {
    parse_subject(subject).map(|_| ())
}

fn is_printable(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_alphanumeric() || " '()+,-./:=?".contains(c))
}

fn name_der(attrs: &[(String, StrKind, String)]) -> R<Vec<u8>> {
    let mut rdns = Vec::new();
    for (o, kind, value) in attrs {
        let v = match kind {
            StrKind::Ia5 => tlv(0x16, value.as_bytes()),
            StrKind::Printable => tlv(0x13, value.as_bytes()),
            StrKind::Text if is_printable(value) => tlv(0x13, value.as_bytes()),
            StrKind::Text => tlv(0x0c, value.as_bytes()),
        };
        rdns.push(set(&[seq(&[oid(o)?, v])]));
    }
    Ok(seq(&rdns))
}

// ---------------------------------------------------------------------------
// Keys and signatures
// ---------------------------------------------------------------------------

fn curve_oid(c: Curve) -> &'static str {
    match c {
        Curve::P256 => "1.2.840.10045.3.1.7",
        Curve::P384 => "1.3.132.0.34",
        Curve::P521 => "1.3.132.0.35",
        Curve::P256K => "1.3.132.0.10",
    }
}

/// `(SubjectPublicKeyInfo DER, the key bits inside it)`.
fn spki(m: &KeyMaterial) -> R<(Vec<u8>, Vec<u8>)> {
    match crypto::public_parts(m).map_err(|e| format!("{e:?}"))? {
        PublicParts::Rsa { n, e } => {
            let key = seq(&[integer(&n), integer(&e)]);
            let alg = seq(&[oid_const("1.2.840.113549.1.1.1"), null()]);
            Ok((seq(&[alg, bit_string(&key)]), key))
        }
        PublicParts::Ec { crv, x, y } => {
            let mut point = vec![4u8];
            point.extend_from_slice(&x);
            point.extend_from_slice(&y);
            let alg = seq(&[oid_const("1.2.840.10045.2.1"), oid_const(curve_oid(crv))]);
            Ok((seq(&[alg, bit_string(&point)]), point))
        }
        PublicParts::Oct => Err("a symmetric key cannot back a certificate".into()),
    }
}

/// The SubjectPublicKeyInfo DER of the public half of `m`, for comparing a merged
/// certificate with the key.
pub fn spki_der(m: &KeyMaterial) -> R<Vec<u8>> {
    spki(m).map(|s| s.0)
}

/// The signature AlgorithmIdentifier for `m` and the name of the algorithm `crypto::sign` takes.
fn sig_alg(m: &KeyMaterial) -> R<(Vec<u8>, &'static str)> {
    match m {
        KeyMaterial::Rsa(_) => Ok((seq(&[oid_const("1.2.840.113549.1.1.11"), null()]), "RS256")),
        KeyMaterial::Ec { curve, .. } => {
            let (alg, oid_s) = match curve {
                Curve::P256 => ("ES256", "1.2.840.10045.4.3.2"),
                Curve::P256K => ("ES256K", "1.2.840.10045.4.3.2"),
                Curve::P384 => ("ES384", "1.2.840.10045.4.3.3"),
                Curve::P521 => ("ES512", "1.2.840.10045.4.3.4"),
            };
            Ok((seq(&[oid_const(oid_s)]), alg))
        }
        KeyMaterial::Oct(_) => Err("a symmetric key cannot back a certificate".into()),
    }
}

/// The signature value (a DER ECDSA-Sig-Value for EC keys) over `data`.
fn sign(m: &KeyMaterial, alg: &str, data: &[u8]) -> R<Vec<u8>> {
    let digest: Vec<u8> = match alg {
        "ES384" => Sha384::digest(data).to_vec(),
        "ES512" => Sha512::digest(data).to_vec(),
        _ => Sha256::digest(data).to_vec(),
    };
    let raw = crypto::sign(m, alg, &digest).map_err(|e| format!("cannot sign with the key: {e:?}"))?;
    if alg.starts_with("ES") {
        let half = raw.len() / 2;
        Ok(seq(&[integer(&raw[..half]), integer(&raw[half..])]))
    } else {
        Ok(raw)
    }
}

// ---------------------------------------------------------------------------
// Extensions
// ---------------------------------------------------------------------------

fn extension(oid_s: &str, critical: bool, value: &[u8]) -> Vec<u8> {
    let mut parts = vec![oid_const(oid_s)];
    if critical {
        parts.push(boolean_true());
    }
    parts.push(octet_string(value));
    seq(&parts)
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
}

fn key_usage_bit(name: &str) -> Option<u32> {
    Some(match name {
        "digitalSignature" => 0,
        "nonRepudiation" | "contentCommitment" => 1,
        "keyEncipherment" => 2,
        "dataEncipherment" => 3,
        "keyAgreement" => 4,
        "keyCertSign" => 5,
        "cRLSign" => 6,
        "encipherOnly" => 7,
        "decipherOnly" => 8,
        _ => return None,
    })
}

fn key_usage_der(names: &[String]) -> Option<Vec<u8>> {
    let mut bits = 0u16;
    for n in names {
        if let Some(b) = key_usage_bit(n) {
            bits |= 1 << (15 - b);
        }
    }
    if bits == 0 {
        return None;
    }
    let bytes = bits.to_be_bytes();
    let (content, used) =
        if bytes[1] == 0 { (vec![bytes[0]], bytes[0].trailing_zeros()) } else { (bytes.to_vec(), bytes[1].trailing_zeros()) };
    let mut v = vec![used as u8];
    v.extend(content);
    Some(tlv(0x03, &v))
}

/// The GeneralNames of the SAN extension, or `None` when the policy has no SANs.
fn san_der(sans: &Value) -> R<Option<Vec<u8>>> {
    let mut names: Vec<Vec<u8>> = Vec::new();
    for d in strings(sans.get("dns_names")) {
        if d.is_empty() || !d.is_ascii() {
            return Err(format!("bad DNS name '{d}'"));
        }
        names.push(tlv(0x82, d.as_bytes()));
    }
    for e in strings(sans.get("emails")) {
        if !e.contains('@') || !e.is_ascii() {
            return Err(format!("bad e-mail address '{e}'"));
        }
        names.push(tlv(0x81, e.as_bytes()));
    }
    for u in strings(sans.get("uris")) {
        if u.is_empty() || !u.is_ascii() {
            return Err(format!("bad URI '{u}'"));
        }
        names.push(tlv(0x86, u.as_bytes()));
    }
    let ips = [strings(sans.get("ipAddresses")), strings(sans.get("ip_addresses"))].concat();
    for ip in ips {
        let addr: IpAddr = ip.parse().map_err(|_| format!("bad IP address '{ip}'"))?;
        let bytes = match addr {
            IpAddr::V4(a) => a.octets().to_vec(),
            IpAddr::V6(a) => a.octets().to_vec(),
        };
        names.push(tlv(0x87, &bytes));
    }
    for upn in strings(sans.get("upns")) {
        let name = tlv(0xa0, &[oid_const("1.3.6.1.4.1.311.20.2.3"), tlv(0xa0, &tlv(0x0c, upn.as_bytes()))].concat());
        names.push(name);
    }
    Ok(if names.is_empty() { None } else { Some(seq(&names)) })
}

struct Profile {
    key_usage: Option<Vec<u8>>,
    is_ca: bool,
    ekus: Vec<Vec<u8>>,
    san: Option<Vec<u8>>,
}

fn profile(x509: &Value) -> R<Profile> {
    let sans = x509.get("sans").cloned().unwrap_or(json!({}));
    let ekus: Vec<Vec<u8>> = strings(x509.get("ekus")).iter().filter_map(|o| oid(o).ok()).collect();
    Ok(Profile {
        key_usage: key_usage_der(&strings(x509.get("key_usage"))),
        is_ca: x509.pointer("/basic_constraints/ca").and_then(|c| c.as_bool()).unwrap_or(false),
        ekus,
        san: san_der(&sans)?,
    })
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

fn add_months(t: OffsetDateTime, months: i64) -> OffsetDateTime {
    let total = t.year() as i64 * 12 + (t.month() as i64 - 1) + months;
    let (year, month0) = (total.div_euclid(12), total.rem_euclid(12));
    let month = time::Month::try_from((month0 + 1) as u8).unwrap_or(time::Month::January);
    let mut day = t.day();
    loop {
        if let Ok(d) = time::Date::from_calendar_date(year as i32, month, day) {
            return t.replace_date(d);
        }
        day -= 1;
        if day == 0 {
            return t;
        }
    }
}

// ---------------------------------------------------------------------------
// Certificate and request
// ---------------------------------------------------------------------------

/// Build a self-signed certificate and the CSR for `policy`, using `m` as the key.
pub fn build(policy: &Value, m: &KeyMaterial, now: OffsetDateTime) -> R<Built> {
    let x509 = policy.get("x509_props").cloned().unwrap_or(json!({}));
    let subject = x509.get("subject").and_then(|s| s.as_str()).unwrap_or("");
    let name = name_der(&parse_subject(subject)?)?;
    let prof = profile(&x509)?;
    let (spki_der, key_bits) = spki(m)?;
    let key_id = Sha1::digest(&key_bits).to_vec();

    let months = x509.get("validity_months").and_then(|v| v.as_i64()).unwrap_or(12).clamp(1, 1200);
    let now = OffsetDateTime::from_unix_timestamp(now.unix_timestamp()).unwrap_or(now);
    let not_before = now - time::Duration::minutes(10);
    let not_after = add_months(now, months);

    let mut serial = [0u8; 16];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut serial);
    serial[0] &= 0x7f;
    serial[0] |= 0x01;

    // extensions, in the order Key Vault writes them
    let mut exts: Vec<Vec<u8>> = Vec::new();
    if let Some(ku) = &prof.key_usage {
        exts.push(extension("2.5.29.15", true, ku));
    }
    let bc = if prof.is_ca { seq(&[boolean_true()]) } else { seq(&[]) };
    exts.push(extension("2.5.29.19", prof.is_ca, &bc));
    if !prof.ekus.is_empty() {
        exts.push(extension("2.5.29.37", false, &seq(&prof.ekus)));
    }
    if let Some(san) = &prof.san {
        exts.push(extension("2.5.29.17", subject.trim().is_empty(), san));
    }
    exts.push(extension("2.5.29.35", false, &seq(&[tlv(0x80, &key_id)])));
    exts.push(extension("2.5.29.14", false, &octet_string(&key_id)));

    let (sig_alg, alg) = sig_alg(m)?;
    let tbs = seq(&[
        tlv(0xa0, &integer(&[2])),
        integer(&serial),
        sig_alg.clone(),
        name.clone(),
        seq(&[time_der(not_before), time_der(not_after)]),
        name.clone(),
        spki_der.clone(),
        tlv(0xa3, &seq(&exts)),
    ]);
    let signature = sign(m, alg, &tbs)?;
    let cert_der = seq(&[tbs, sig_alg.clone(), bit_string(&signature)]);

    // the request carries what the policy asks for; the issuer adds identifiers and validity
    // (in the order of the requests Key Vault makes: basic constraints last)
    let mut req_exts: Vec<Vec<u8>> = Vec::new();
    if let Some(ku) = &prof.key_usage {
        req_exts.push(extension("2.5.29.15", true, ku));
    }
    if !prof.ekus.is_empty() {
        req_exts.push(extension("2.5.29.37", false, &seq(&prof.ekus)));
    }
    if let Some(san) = &prof.san {
        req_exts.push(extension("2.5.29.17", subject.trim().is_empty(), san));
    }
    req_exts.push(extension("2.5.29.19", prof.is_ca, &bc));
    let attrs = tlv(0xa0, &seq(&[oid_const("1.2.840.113549.1.9.14"), set(&[seq(&req_exts)])]));
    let info = seq(&[integer(&[0]), name, spki_der, attrs]);
    let csr_sig = sign(m, alg, &info)?;
    let csr_der = seq(&[info, sig_alg, bit_string(&csr_sig)]);
    Ok(Built { cert_der, csr_der })
}

#[cfg(test)]
mod tests {
    use super::*;
    use x509_parser::prelude::{FromDer, X509Certificate};

    fn policy(subject: &str, sans: Value) -> Value {
        json!({"x509_props": {"subject": subject, "sans": sans, "ekus": ["1.3.6.1.5.5.7.3.1", "1.3.6.1.5.5.7.3.2"],
            "key_usage": ["digitalSignature", "keyEncipherment"], "validity_months": 12, "basic_constraints": {"ca": false}}})
    }

    fn ext_list(der: &[u8]) -> Vec<(String, bool)> {
        let (_, c) = X509Certificate::from_der(der).unwrap();
        c.extensions().iter().map(|e| (e.oid.to_id_string(), e.critical)).collect()
    }

    #[test]
    fn profile_of_a_default_certificate() {
        let k = crypto::generate_ec(Curve::P256);
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let b = build(&policy("CN=DefaultPolicy", json!({})), &k, now).unwrap();
        let (_, c) = X509Certificate::from_der(&b.cert_der).unwrap();
        assert_eq!(c.version().0, 2);
        assert_eq!(c.tbs_certificate.raw_serial().len(), 16);
        assert_eq!(c.issuer(), c.subject());
        assert_eq!(c.validity().not_before.timestamp(), 1_800_000_000 - 600);
        assert_eq!(
            ext_list(&b.cert_der),
            vec![
                ("2.5.29.15".to_string(), true),
                ("2.5.29.19".to_string(), false),
                ("2.5.29.37".to_string(), false),
                ("2.5.29.35".to_string(), false),
                ("2.5.29.14".to_string(), false),
            ]
        );
        let ski = c.get_extension_unique(&x509_parser::oid_registry::OID_X509_EXT_SUBJECT_KEY_IDENTIFIER).unwrap().unwrap();
        let aki = c.get_extension_unique(&x509_parser::oid_registry::OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER).unwrap().unwrap();
        let (
            x509_parser::extensions::ParsedExtension::SubjectKeyIdentifier(s),
            x509_parser::extensions::ParsedExtension::AuthorityKeyIdentifier(a),
        ) = (ski.parsed_extension(), aki.parsed_extension())
        else {
            panic!("identifiers not parsed");
        };
        assert_eq!(a.key_identifier.as_ref().unwrap().0, s.0);
        assert_eq!(s.0.len(), 20);
    }

    #[test]
    fn san_of_every_kind_and_critical_with_empty_subject() {
        let k = crypto::generate_ec(Curve::P256);
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let sans = json!({"dns_names": ["a.example"], "emails": ["x@example.test"], "uris": ["https://e.test/x"],
            "ipAddresses": ["10.0.0.1", "::1"], "upns": ["john@example.test"]});
        let b = build(&policy("", sans.clone()), &k, now).unwrap();
        let (_, c) = X509Certificate::from_der(&b.cert_der).unwrap();
        assert_eq!(c.subject().as_raw().len(), 2);
        let parsed = crate::cert_x509::parse_cert(&b.cert_der).unwrap();
        assert_eq!(parsed.sans, sans);
        assert!(ext_list(&b.cert_der).contains(&("2.5.29.17".to_string(), true)));
        let b = build(&policy("CN=x", sans), &k, now).unwrap();
        assert!(ext_list(&b.cert_der).contains(&("2.5.29.17".to_string(), false)));
        let raw = b.cert_der.windows(11).any(|w| w == [0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x14, 0x02]);
        assert!(raw, "the UPN otherName OID is in the DER");
    }

    #[test]
    fn signature_algorithms_by_key_type() {
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        for (curve, oid_s) in [
            (Curve::P256, "1.2.840.10045.4.3.2"),
            (Curve::P384, "1.2.840.10045.4.3.3"),
            (Curve::P521, "1.2.840.10045.4.3.4"),
            (Curve::P256K, "1.2.840.10045.4.3.2"),
        ] {
            let k = crypto::generate_ec(curve);
            let b = build(&policy("CN=ec", json!({})), &k, now).unwrap();
            let (_, c) = X509Certificate::from_der(&b.cert_der).unwrap();
            assert_eq!(c.signature_algorithm.algorithm.to_id_string(), oid_s);
            assert!(!c.signature_value.data.is_empty());
        }
    }

    #[test]
    fn subjects() {
        assert!(validate_subject("CN=a, O=b, C=US").is_ok());
        assert!(validate_subject("").is_ok());
        assert!(validate_subject("CN=a\\, b").is_ok());
        assert!(validate_subject("this is not a distinguished name").is_err());
        assert!(validate_subject("CN=").is_err());
        assert!(validate_subject("Bogus=1").is_err());
        assert!(validate_subject("CN=a,,O=b").is_err());
    }
}
