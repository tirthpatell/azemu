//! Certificates: create (self-signed, or pending for an unknown issuer), import (PEM
//! and PKCS#12), get/update/list/versions, delete/recover/purge, operations, merge,
//! backup/restore. Each certificate has a linked key and a linked secret with the same
//! name and version; they live in the key and secret collections (marked `managed`).

use azemu_core::{Request, Response};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::{json, Map, Value};

use crate::auth::no_content;
use crate::cert_policy::{check_subject, issuer_name, key_spec, normalize_policy, policy_json};
use crate::cert_x509 as x509;
use crate::common::*;
use crate::crypto::{self, KeyMaterial};
use crate::errors::*;
use crate::jwk::{self, Family};
use crate::keys::{KeyExtra, KeyVersion};
use crate::paging;
use crate::secrets::SecretVersion;
use crate::state::{norm, Deleted, Item, Vault, Versioned};
use crate::KeyVaultService;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct CertVersion {
    pub version: String,
    pub der: Vec<u8>,
    pub x5t: String,
    pub attrs: Attrs,
    pub tags: Tags,
    /// `preserveCertOrder` as the request sent it.
    pub preserve_order: Option<bool>,
}

impl Versioned for CertVersion {
    fn version(&self) -> &str {
        &self.version
    }
}

/// A certificate request that is not finished (or finished and kept for `GET /pending`).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Operation {
    pub issuer: String,
    pub csr: Vec<u8>,
    /// `inProgress`, `completed`, `cancelled`
    pub status: String,
    pub request_id: String,
    pub cancellation_requested: bool,
    /// For an unknown issuer: the version and key waiting for the signed certificate.
    #[serde(with = "crate::persist::pending_key")]
    pub pending_key: Option<(String, KeyMaterial)>,
    /// `preserveCertOrder` as the request sent it.
    pub preserve_order: Option<bool>,
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct CertExtra {
    /// The policy as stored (without `id`).
    pub policy: Option<Value>,
    pub operation: Option<Operation>,
    /// A certificate whose request never produced a certificate has no version; this is the
    /// version id its bundle (in the deleted state, and in lists with `includePending`) shows.
    pub stub_version: String,
    /// The request of such a certificate was cancelled: its bundle is disabled.
    pub stub_cancelled: bool,
}

/// The bundle a certificate without a version shows (policy only, no `cer`).
pub(crate) fn stub_version(item: &Item<CertVersion, CertExtra>) -> CertVersion {
    let created = item.extra.policy.as_ref().and_then(|p| p.pointer("/attributes/created")).and_then(|c| c.as_i64()).unwrap_or(0);
    let cancelled = item.extra.stub_cancelled || item.extra.operation.as_ref().map(|o| o.cancellation_requested).unwrap_or(false);
    CertVersion {
        version: if item.extra.stub_version.is_empty() { "0".repeat(32) } else { item.extra.stub_version.clone() },
        der: vec![],
        x5t: String::new(),
        attrs: Attrs { enabled: !cancelled, nbf: None, exp: None, created, updated: created },
        tags: Tags::new(),
        preserve_order: item.extra.operation.as_ref().and_then(|o| o.preserve_order),
    }
}

const PKCS12: &str = "application/x-pkcs12";
const PEM: &str = "application/x-pem-file";

/// `GET /secrets/{name}?outContentType=…`: PFX to PEM for certificate secrets.
pub fn convert_secret(secret: &SecretVersion, out_content_type: &str) -> Result<String, Response> {
    let unsupported =
        || bad_parameter("The 'Unsupported' to 'PEM' transformation is not supported. Only PFX to PEM transformation is supported.");
    if out_content_type != PEM || secret.content_type.as_deref() != Some(PKCS12) {
        return Err(unsupported());
    }
    let data = STANDARD.decode(secret.value.as_bytes()).map_err(|_| unsupported())?;
    let (key, certs) = x509::pkcs12_import(&data, "").map_err(|e| bad_parameter(&e))?;
    pem_bundle(&key, &certs).map_err(|e| bad_parameter(&e))
}

fn pem_bundle(key: &KeyMaterial, certs: &[Vec<u8>]) -> Result<String, String> {
    let mut s = x509::pem_encode("PRIVATE KEY", &x509::pkcs8_der(key)?);
    for c in certs {
        s.push_str(&x509::pem_encode("CERTIFICATE", c));
    }
    Ok(s)
}

pub(crate) fn cert_json(c: &Ctx, name: &str, v: &CertVersion, policy: Option<&Value>, pending: bool, deleted: Option<&Deleted>) -> Value {
    let base = c.base.as_str();
    let mut m = Map::new();
    if let Some(d) = deleted {
        m.insert("recoveryId".into(), json!(format!("{base}/deletedcertificates/{name}")));
        m.insert("deletedDate".into(), json!(d.deleted_date));
        m.insert("scheduledPurgeDate".into(), json!(d.scheduled_purge_date));
    }
    m.insert("id".into(), json!(format!("{base}/certificates/{name}/{}", v.version)));
    if !v.der.is_empty() {
        m.insert("kid".into(), json!(format!("{base}/keys/{name}/{}", v.version)));
        m.insert("sid".into(), json!(format!("{base}/secrets/{name}/{}", v.version)));
        m.insert("x5t".into(), json!(v.x5t));
        m.insert("cer".into(), json!(STANDARD.encode(&v.der)));
    }
    m.insert("attributes".into(), Value::Object(v.attrs.to_json(&c.profile)));
    if !v.tags.is_empty() {
        m.insert("tags".into(), tags_json(&v.tags));
    }
    if let Some(p) = policy {
        m.insert("policy".into(), policy_json(base, name, p));
    }
    if pending {
        m.insert("pending".into(), json!({"id": format!("{base}/certificates/{name}/pending")}));
    }
    if let Some(p) = v.preserve_order {
        m.insert("preserveCertOrder".into(), json!(p));
    }
    Value::Object(m)
}

/// A list entry: `id`, `x5t`, attributes (without the recovery fields), tags.
fn item_json(c: &Ctx, name: &str, v: &CertVersion, with_version: bool, deleted: Option<&Deleted>) -> Value {
    let base = c.base.as_str();
    let mut m = Map::new();
    let id = if with_version { format!("{base}/certificates/{name}/{}", v.version) } else { format!("{base}/certificates/{name}") };
    m.insert("id".into(), json!(id));
    if !v.x5t.is_empty() {
        m.insert("x5t".into(), json!(v.x5t));
    }
    let mut attrs = v.attrs.to_json(&c.profile);
    if deleted.is_none() {
        // the list of deleted certificates keeps the recovery fields, the plain list has none
        attrs.remove("recoveryLevel");
        attrs.remove("recoverableDays");
    }
    m.insert("attributes".into(), Value::Object(attrs));
    if !v.tags.is_empty() {
        m.insert("tags".into(), tags_json(&v.tags));
    }
    if deleted.is_none() {
        m.insert("subject".into(), json!(""));
    }
    if let Some(d) = deleted {
        m.insert("recoveryId".into(), json!(format!("{base}/deletedcertificates/{name}")));
        m.insert("deletedDate".into(), json!(d.deleted_date));
        m.insert("scheduledPurgeDate".into(), json!(d.scheduled_purge_date));
    }
    Value::Object(m)
}

pub(crate) fn operation_json(base: &str, name: &str, op: &Operation) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), json!(format!("{base}/certificates/{name}/pending")));
    m.insert("issuer".into(), json!({"name": op.issuer}));
    m.insert("csr".into(), json!(STANDARD.encode(&op.csr)));
    m.insert("cancellation_requested".into(), json!(op.cancellation_requested));
    m.insert("status".into(), json!(op.status));
    match op.status.as_str() {
        "completed" => {
            m.insert("target".into(), json!(format!("{base}/certificates/{name}")));
        }
        "inProgress" => {
            let text = if op.issuer.eq_ignore_ascii_case("Unknown") {
                "Pending certificate created. Please Perform Merge to complete the request."
            } else {
                "Pending certificate created. Certificate request is in progress. This may take some time based on the issuer provider. Please check again later."
            };
            m.insert("status_details".into(), json!(text));
        }
        _ => {}
    }
    if let Some(p) = op.preserve_order {
        m.insert("preserveCertOrder".into(), json!(p));
    }
    m.insert("request_id".into(), json!(op.request_id));
    Value::Object(m)
}

/// An operation response, with the `Retry-After` real Key Vault sends (and `Location` on creation).
pub(crate) fn operation_response(status: u16, req: &Request, name: &str, body: &Value) -> Response {
    let mut r = json_response(status, body);
    let h = r.headers_mut();
    h.insert("retry-after", azemu_core::http::HeaderValue::from_static("10"));
    if status == 202 {
        let version = req.query.get("api-version").unwrap_or("7.6");
        let loc = format!("{}/certificates/{name}/pending?api-version={version}", req.ctx.endpoint.base_url());
        if let Ok(v) = azemu_core::http::HeaderValue::from_str(&loc) {
            h.insert("location", v);
        }
    }
    r
}

pub(crate) fn pending_not_found(name: &str) -> Response {
    json_response(
        404,
        &json!({"error": {"code": "PendingCertificateNotFound", "message": format!("Pending Certificate not found: {name}")}}),
    )
}

/// Everything needed to put a new certificate version (with its key and secret) in a vault.
pub(crate) struct NewVersion {
    pub(crate) version: String,
    pub(crate) der: Vec<u8>,
    pub(crate) material: KeyMaterial,
    pub(crate) secret_value: String,
    pub(crate) secret_content_type: String,
    pub(crate) policy: Value,
    pub(crate) tags: Tags,
    pub(crate) patch: AttrPatch,
    pub(crate) parsed_nbf: i64,
    pub(crate) parsed_exp: i64,
    pub(crate) exportable: bool,
    pub(crate) csr: Vec<u8>,
    pub(crate) issuer: String,
    pub(crate) preserve_order: Option<bool>,
}

pub(crate) fn key_kty_family(m: &KeyMaterial) -> (&'static str, Family) {
    match m {
        KeyMaterial::Rsa(_) => ("RSA", Family::Rsa),
        KeyMaterial::Ec { .. } => ("EC", Family::Ec),
        KeyMaterial::Oct(_) => ("oct", Family::Oct),
    }
}

/// Is `name` taken by a key or secret that does not belong to a certificate?
pub(crate) fn name_taken_by_other(vault: &Vault, name: &str) -> bool {
    let k = vault.keys.items.get(&norm(name)).map(|i| i.versions.iter().any(|v| !v.managed)).unwrap_or(false);
    let s = vault.secrets.items.get(&norm(name)).map(|i| i.versions.iter().any(|v| !v.managed)).unwrap_or(false);
    k || s
}

pub(crate) fn taken_response(name: &str) -> Response {
    conflict(&format!("A key or secret named '{name}' already exists, so it cannot be used for a certificate."))
}

/// Add the certificate version and its key and secret. Caller holds the lock.
pub(crate) fn install(vault: &mut Vault, name: &str, nv: &NewVersion, now: i64) -> CertVersion {
    let mut attrs = Attrs::new(now);
    attrs.nbf = Some(nv.parsed_nbf);
    attrs.exp = Some(nv.parsed_exp);
    attrs.apply(&nv.patch);
    let cert = CertVersion {
        version: nv.version.clone(),
        der: nv.der.clone(),
        x5t: x509::thumbprint(&nv.der),
        attrs: attrs.clone(),
        tags: nv.tags.clone(),
        preserve_order: nv.preserve_order,
    };

    let (kty, family) = key_kty_family(&nv.material);
    let key_size = match &nv.material {
        KeyMaterial::Rsa(k) => {
            use rsa::traits::PublicKeyParts;
            Some(k.n().bit_width())
        }
        _ => None,
    };
    let key = KeyVersion {
        version: nv.version.clone(),
        kty: kty.to_string(),
        family,
        material: nv.material.clone(),
        key_ops: jwk::default_key_ops(family),
        attrs: Attrs { enabled: attrs.enabled, nbf: attrs.nbf, exp: attrs.exp, created: now, updated: now },
        tags: Tags::new(),
        release_policy: None,
        key_size,
        exportable: nv.exportable,
        managed: true,
    };
    let secret = SecretVersion {
        version: nv.version.clone(),
        value: nv.secret_value.clone(),
        content_type: Some(nv.secret_content_type.clone()),
        tags: nv.tags.clone(),
        attrs: Attrs { enabled: attrs.enabled, nbf: attrs.nbf, exp: attrs.exp, created: now, updated: now },
        kid: Some(format!("/keys/{name}/{}", nv.version)),
        managed: true,
    };
    match vault.keys.active_mut(name) {
        Some(i) => i.versions.push(key),
        None => {
            vault
                .keys
                .items
                .insert(norm(name), Item { name: name.to_string(), versions: vec![key], deleted: None, extra: KeyExtra::default() });
        }
    }
    match vault.secrets.active_mut(name) {
        Some(i) => i.versions.push(secret),
        None => {
            vault.secrets.items.insert(norm(name), Item { name: name.to_string(), versions: vec![secret], deleted: None, extra: () });
        }
    }
    let operation = Operation {
        issuer: nv.issuer.clone(),
        csr: nv.csr.clone(),
        status: "completed".into(),
        request_id: new_version(),
        cancellation_requested: false,
        pending_key: None,
        preserve_order: nv.preserve_order,
    };
    match vault.certs.active_mut(name) {
        Some(i) => {
            i.versions.push(cert.clone());
            i.extra.policy = Some(nv.policy.clone());
            i.extra.operation = Some(operation);
        }
        None => {
            vault.certs.items.insert(
                norm(name),
                Item {
                    name: name.to_string(),
                    versions: vec![cert.clone()],
                    deleted: None,
                    extra: CertExtra { policy: Some(nv.policy.clone()), operation: Some(operation), ..Default::default() },
                },
            );
        }
    }
    cert
}

pub(crate) fn secret_for(material: &KeyMaterial, chain: &[Vec<u8>], policy: &Value) -> Result<(String, String), String> {
    let ct = policy.pointer("/secret_props/contentType").and_then(|c| c.as_str()).unwrap_or(PKCS12);
    if ct.eq_ignore_ascii_case(PEM) {
        Ok((pem_bundle(material, chain)?, PEM.to_string()))
    } else {
        Ok((STANDARD.encode(x509::pkcs12_export(material, chain)?), PKCS12.to_string()))
    }
}

fn generate_material(spec: &crate::cert_policy::KeySpec) -> Result<KeyMaterial, String> {
    match spec.kty {
        "RSA" => crypto::generate_rsa(spec.bits, 65537).map_err(|e| format!("{e:?}")),
        _ => Ok(crypto::generate_ec(spec.curve)),
    }
}

impl KeyVaultService {
    fn cert_ctx_check(&self, req: &Request, name: &str) -> Option<Response> {
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        if vault.certs.is_deleted(name) {
            return Some(name_is_deleted(Kind::Certificate, name));
        }
        if name_taken_by_other(vault, name) {
            return Some(taken_response(name));
        }
        None
    }

    pub async fn create_certificate(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let Some(policy_in) = body.get("policy").filter(|p| p.is_object()) else {
            return bad_parameter("The parameter 'policy' is required.");
        };
        let patch = match parse_attr_patch(&body) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let tags = match parse_tags(&body) {
            Ok(t) => t.unwrap_or_default(),
            Err(r) => return r,
        };
        let preserve_order = body.get("preserveCertOrder").and_then(|p| p.as_bool());
        let now = now_secs(req);
        if let Err(r) = check_subject(policy_in) {
            return r;
        }
        let policy = match normalize_policy(policy_in, None, now) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let spec = match key_spec(&policy) {
            Ok(s) => s,
            Err(r) => return r,
        };
        if let Some(r) = self.cert_ctx_check(req, name) {
            return r;
        }
        let issuer = issuer_name(&policy);
        let pending = issuer.eq_ignore_ascii_case("Unknown");
        let when = req.ctx.time;
        let pol = policy.clone();
        let made = tokio::task::spawn_blocking(move || -> Result<(KeyMaterial, x509::Built, String, String), String> {
            let material = generate_material(&spec)?;
            let built = x509::build(&pol, &material, when)?;
            let (value, ct) =
                if pending { (String::new(), String::new()) } else { secret_for(&material, std::slice::from_ref(&built.cert_der), &pol)? };
            Ok((material, built, value, ct))
        })
        .await;
        let (material, built, secret_value, secret_ct) = match made {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => return bad_parameter(&e),
            Err(_) => return json_response(500, &json!({"error": {"code": "InternalError", "message": "certificate creation failed"}})),
        };
        let c = Ctx::of(req);
        let version = new_version();
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now);
        if vault.certs.is_deleted(name) {
            return name_is_deleted(Kind::Certificate, name);
        }
        let display = vault.certs.active(name).map(|i| i.name.clone()).unwrap_or_else(|| name.to_string());
        if pending {
            let stub = version.clone();
            let op = Operation {
                issuer: issuer.clone(),
                csr: built.csr_der.clone(),
                status: "inProgress".into(),
                request_id: new_version(),
                cancellation_requested: false,
                pending_key: Some((version, material)),
                preserve_order,
            };
            let json = operation_json(&c.base, &display, &op);
            match vault.certs.active_mut(name) {
                Some(i) => {
                    i.extra.operation = Some(op);
                    i.extra.policy = Some(policy);
                    i.extra.stub_version = stub;
                    i.extra.stub_cancelled = false;
                }
                None => {
                    vault.certs.items.insert(
                        norm(name),
                        Item {
                            name: display.clone(),
                            versions: vec![],
                            deleted: None,
                            extra: CertExtra { policy: Some(policy), operation: Some(op), stub_version: stub, stub_cancelled: false },
                        },
                    );
                }
            }
            return operation_response(202, req, &display, &json);
        }
        let parsed = match x509::parse_cert(&built.cert_der) {
            Ok(p) => p,
            Err(e) => return bad_parameter(&e),
        };
        let exportable = spec_exportable(&policy);
        let nv = NewVersion {
            version,
            der: built.cert_der,
            material,
            secret_value,
            secret_content_type: secret_ct,
            policy,
            tags,
            patch,
            parsed_nbf: parsed.not_before,
            parsed_exp: parsed.not_after,
            exportable,
            csr: built.csr_der,
            issuer,
            preserve_order,
        };
        install(vault, &display, &nv, now);
        let op = vault.certs.active(name).and_then(|i| i.extra.operation.clone());
        match op {
            Some(mut op) => {
                // The SDK pollers only poll when the 202 says the request is in progress, so
                // the response to the create call says that; the stored operation (what
                // `GET .../pending` returns from now on) is already completed.
                op.status = "inProgress".into();
                operation_response(202, req, &display, &operation_json(&c.base, &display, &op))
            }
            None => bad_parameter("certificate creation failed"),
        }
    }

    pub async fn import_certificate(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let value = match str_field(&body, "value") {
            Ok(v) => v.to_string(),
            Err(r) => return r,
        };
        let pwd = body.get("pwd").and_then(|p| p.as_str()).unwrap_or("").to_string();
        let patch = match parse_attr_patch(&body) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let tags = match parse_tags(&body) {
            Ok(t) => t.unwrap_or_default(),
            Err(r) => return r,
        };
        let now = now_secs(req);
        let preserve_order = body.get("preserveCertOrder").and_then(|p| p.as_bool());
        let newer = Profile::of(req).hsm_platform;
        let policy_in = body.get("policy").filter(|p| p.is_object()).cloned();
        if let Some(r) = self.cert_ctx_check(req, name) {
            return r;
        }
        let policy_for_task = policy_in.clone();
        let task = tokio::task::spawn_blocking(move || -> Result<(NewParts, Value), String> {
            // `value` is base64 of a PKCS#12 file or of a PEM bundle, or the PEM bundle itself as text
            // (the JavaScript SDK sends it that way for `application/x-pem-file`).
            let raw = if value.contains("-----BEGIN") {
                value.clone().into_bytes()
            } else {
                x509::decode_value(&value).ok_or("The 'value' is not valid base64.")?
            };
            let text = String::from_utf8_lossy(&raw).to_string();
            let (material, mut certs, is_pem) = if text.contains("-----BEGIN") {
                let blocks = x509::pem_blocks(&text);
                let mut key = None;
                let mut certs = Vec::new();
                for (label, der) in blocks {
                    if label == "CERTIFICATE" {
                        certs.push(der);
                    } else if label.ends_with("PRIVATE KEY") {
                        key = Some(x509::material_from_pem_block(&label, &der)?);
                    }
                }
                (key.ok_or("The PEM does not contain a private key.")?, certs, true)
            } else {
                let (k, c) = x509::pkcs12_import(&raw, &pwd)?;
                (k, c, false)
            };
            if certs.is_empty() {
                return Err("The imported file contains no certificate.".into());
            }
            // the leaf is the certificate whose public key belongs to the private key; the
            // secret lists the leaf first unless the request asks to keep the file's order
            let original = certs.clone();
            let want = x509::spki_of_material(&material).ok();
            if let Some(w) = want {
                if let Some(i) = certs.iter().position(|c| x509::parse_cert(c).map(|p| p.spki == w).unwrap_or(false)) {
                    let leaf = certs.remove(i);
                    certs.insert(0, leaf);
                }
            }
            let parsed = x509::parse_cert(&certs[0])?;
            let had_policy = policy_for_task.is_some();
            let mut policy = json!({});
            if let Some(p) = &policy_for_task {
                policy = p.clone();
            }
            if !had_policy {
                // an import without a policy gets the policy the service makes up: issuer
                // Unknown (set below) and an e-mail to the contacts at 80 % of the lifetime
                policy["lifetime_actions"] = json!([{"trigger": {"lifetime_percentage": 80}, "action": {"action_type": "EmailContacts"}}]);
            }
            let default_ct = if is_pem { PEM } else { PKCS12 };
            let mut kp = policy.get("key_props").cloned().unwrap_or(json!({}));
            kp["kty"] = json!(parsed.kty);
            match parsed.kty {
                "RSA" => kp["key_size"] = json!(parsed.key_size.unwrap_or(2048)),
                _ => {
                    if let Some(c) = parsed.curve {
                        kp["crv"] = json!(c.name());
                        kp["key_size"] = json!(match c {
                            crypto::Curve::P384 => 384,
                            crypto::Curve::P521 => 521,
                            _ => 256,
                        });
                    }
                }
            }
            policy["key_props"] = kp;
            let mut sp = policy.get("secret_props").cloned().unwrap_or(json!({}));
            if sp.get("contentType").is_none() {
                sp["contentType"] = json!(default_ct);
            }
            policy["secret_props"] = sp;
            let mut xp = policy.get("x509_props").cloned().unwrap_or(json!({}));
            xp["subject"] = json!(parsed.subject);
            match parsed.sans.as_object() {
                Some(o) if !o.is_empty() => xp["sans"] = parsed.sans.clone(),
                _ => {
                    if let Some(o) = xp.as_object_mut() {
                        o.remove("sans");
                    }
                }
            }
            xp["validity_months"] = json!(validity_months(parsed.not_before, parsed.not_after, newer));
            xp["ekus"] = json!(parsed.ekus);
            xp["key_usage"] = json!(parsed.key_usage);
            xp["basic_constraints"] = json!({"ca": parsed.is_ca});
            policy["x509_props"] = xp;
            if policy.get("issuer").is_none() {
                policy["issuer"] = json!({"name": "Unknown"});
            }
            let (secret_value, secret_ct) = {
                let n = normalize_policy(&policy, None, 0).map_err(|_| "invalid policy".to_string())?;
                secret_for(&material, if preserve_order == Some(true) { &original } else { &certs }, &n)?
            };
            Ok((
                NewParts {
                    material,
                    der: certs.remove(0),
                    secret_value,
                    secret_ct,
                    parsed_nbf: parsed.not_before,
                    parsed_exp: parsed.not_after,
                },
                policy,
            ))
        })
        .await;
        let (parts, policy_raw) = match task {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => return bad_parameter(&e),
            Err(_) => return json_response(500, &json!({"error": {"code": "InternalError", "message": "certificate import failed"}})),
        };
        let policy = match normalize_policy(&policy_raw, None, now) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let c = Ctx::of(req);
        let exportable = spec_exportable(&policy);
        let nv = NewVersion {
            version: new_version(),
            der: parts.der,
            material: parts.material,
            secret_value: parts.secret_value,
            secret_content_type: parts.secret_ct,
            policy: policy.clone(),
            tags,
            patch,
            parsed_nbf: parts.parsed_nbf,
            parsed_exp: parts.parsed_exp,
            exportable,
            csr: vec![],
            issuer: issuer_name(&policy),
            preserve_order,
        };
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now);
        if vault.certs.is_deleted(name) {
            return name_is_deleted(Kind::Certificate, name);
        }
        let display = vault.certs.active(name).map(|i| i.name.clone()).unwrap_or_else(|| name.to_string());
        let cert = install(vault, &display, &nv, now);
        // an import has no request to show
        if let Some(i) = vault.certs.active_mut(name) {
            i.extra.operation = None;
        }
        json_response(200, &cert_json(&c, &display, &cert, Some(&policy), false, None))
    }

    pub fn get_certificate(&self, req: &Request, name: &str, version: Option<&str>) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        let what = match version {
            Some(v) if !v.is_empty() => format!("{name}/versions/{v}"),
            _ => name.to_string(),
        };
        let Some(item) = vault.certs.active(name) else {
            return not_found(Kind::Certificate, &what);
        };
        let latest = matches!(version, None | Some(""));
        // A certificate whose request is still pending (issuer `Unknown`, not merged yet) has no
        // certificate version. It can be read all the same, as a bundle without `cer`, `kid`, `sid`
        // and `x5t` that says where the pending request is; the JavaScript SDK reads it before it
        // reads the operation.
        let stub;
        let v = match item.pick(version) {
            Some(v) => v,
            None if item.latest().is_none() && item.extra.operation.is_some() => {
                stub = stub_version(item);
                if latest || version == Some(stub.version.as_str()) {
                    &stub
                } else {
                    return not_found(Kind::Certificate, &what);
                }
            }
            None => return not_found(Kind::Certificate, &what),
        };
        let policy = if latest { item.extra.policy.as_ref() } else { None };
        json_response(200, &cert_json(&c, &item.name, v, policy, latest && item.extra.operation.is_some(), None))
    }

    pub fn update_certificate(&self, req: &Request, name: &str, version: Option<&str>) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let tags = match parse_tags(&body) {
            Ok(t) => t,
            Err(r) => return r,
        };
        let patch = match parse_attr_patch(&body) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let now = now_secs(req);
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now);
        let what = match version {
            Some(v) if !v.is_empty() => format!("{name}/versions/{v}"),
            _ => name.to_string(),
        };
        let Some(item) = vault.certs.active_mut(name) else {
            return not_found(Kind::Certificate, &what);
        };
        let display = item.name.clone();
        let latest = matches!(version, None | Some(""));
        let target = if latest { item.versions.last_mut() } else { item.find_mut(version.unwrap_or("")) };
        let Some(v) = target else {
            return not_found(Kind::Certificate, &what);
        };
        if let Some(t) = tags {
            v.tags = t;
        }
        v.attrs.apply(&patch);
        v.attrs.updated = now;
        let out = v.clone();
        let ver = out.version.clone();
        // keep the linked key and secret in step
        if let Some(i) = vault.secrets.active_mut(name) {
            if let Some(s) = i.find_mut(&ver) {
                s.tags = out.tags.clone();
                s.attrs.enabled = out.attrs.enabled;
                s.attrs.updated = now;
            }
        }
        if let Some(i) = vault.keys.active_mut(name) {
            if let Some(k) = i.find_mut(&ver) {
                k.attrs.enabled = out.attrs.enabled;
                k.attrs.updated = now;
            }
        }
        let item = vault.certs.active(name);
        let policy = item.filter(|_| latest).and_then(|i| i.extra.policy.as_ref());
        let pending = latest && item.map(|i| i.extra.operation.is_some()).unwrap_or(false);
        json_response(200, &cert_json(&c, &display, &out, policy, pending, None))
    }

    pub fn get_certificate_policy(&self, req: &Request, name: &str) -> Response {
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        match vault.certs.active(name).and_then(|i| i.extra.policy.as_ref().map(|p| (i.name.clone(), p))) {
            Some((display, p)) => json_response(200, &policy_json(&c.base, &display, p)),
            None => not_found(Kind::Certificate, name),
        }
    }

    pub fn update_certificate_policy(&self, req: &Request, name: &str) -> Response {
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let now = now_secs(req);
        if let Err(r) = check_subject(&Value::Object(body.clone())) {
            return r;
        }
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now);
        let Some(item) = vault.certs.active_mut(name) else {
            return not_found(Kind::Certificate, name);
        };
        let display = item.name.clone();
        let merged = match normalize_policy(&Value::Object(body), item.extra.policy.as_ref(), now) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let json = policy_json(&c.base, &display, &merged);
        item.extra.policy = Some(merged);
        json_response(200, &json)
    }

    pub fn delete_certificate(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let now = now_secs(req);
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now);
        let Some(item) = vault.certs.active_mut(name) else {
            return not_found(Kind::Certificate, name);
        };
        let d = Deleted::at(now);
        item.deleted = Some(d);
        let display = item.name.clone();
        let (latest, policy) = (Some(item.latest().cloned().unwrap_or_else(|| stub_version(item))), item.extra.policy.clone());
        let has_operation = item.extra.operation.is_some();
        // the linked key and secret go to the deleted state with it
        if let Some(k) = vault.keys.active_mut(name).filter(|k| k.versions.iter().all(|v| v.managed)) {
            k.deleted = Some(d);
        }
        if let Some(s) = vault.secrets.active_mut(name).filter(|s| s.versions.iter().all(|v| v.managed)) {
            s.deleted = Some(d);
        }
        match latest {
            Some(v) => json_response(200, &cert_json(&c, &display, &v, policy.as_ref(), has_operation, Some(&d))),
            None => not_found(Kind::Certificate, name),
        }
    }

    pub fn get_deleted_certificate(&self, req: &Request, name: &str) -> Response {
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        let Some(item) = vault.certs.deleted(name) else {
            return if vault.certs.items.contains_key(&norm(name)) {
                deleted_not_found(Kind::Certificate, name)
            } else {
                // a name that is nowhere (never made, or purged) gets the long text of a missing certificate; the same as
                // for secrets and keys (recordings `test_secrets_client.pyTestSecretClienttest_purge[2025-07-01].json` entries 47 to 53)
                not_found(Kind::Certificate, name)
            };
        };
        let v = item.latest().cloned().unwrap_or_else(|| stub_version(item));
        match item.deleted {
            Some(d) => json_response(200, &cert_json(&c, &item.name, &v, item.extra.policy.as_ref(), false, Some(&d))),
            None => deleted_not_found(Kind::Certificate, name),
        }
    }

    pub fn purge_certificate(&self, req: &Request, name: &str) -> Response {
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        if !vault.certs.is_deleted(name) {
            return deleted_not_found(Kind::Certificate, name);
        }
        vault.certs.items.remove(&norm(name));
        if vault.keys.is_deleted(name) {
            vault.keys.items.remove(&norm(name));
        }
        if vault.secrets.is_deleted(name) {
            vault.secrets.items.remove(&norm(name));
        }
        no_content()
    }

    pub fn recover_certificate(&self, req: &Request, name: &str) -> Response {
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        let Some(item) = vault.certs.items.get_mut(&norm(name)).filter(|i| i.deleted.is_some()) else {
            return deleted_not_found(Kind::Certificate, name);
        };
        item.deleted = None;
        let item = vault.certs.items.get(&norm(name));
        if let Some(k) = vault.keys.items.get_mut(&norm(name)).filter(|k| k.deleted.is_some()) {
            k.deleted = None;
        }
        if let Some(s) = vault.secrets.items.get_mut(&norm(name)).filter(|s| s.deleted.is_some()) {
            s.deleted = None;
        }
        match item {
            Some(i) => {
                let v = i.latest().cloned().unwrap_or_else(|| stub_version(i));
                json_response(200, &cert_json(&c, &i.name, &v, i.extra.policy.as_ref(), i.extra.operation.is_some(), None))
            }
            None => deleted_not_found(Kind::Certificate, name),
        }
    }

    fn list_cert_items(&self, req: &Request, deleted: bool, path: &str) -> Response {
        let params = match paging::parse(req) {
            Ok(p) => p,
            Err(r) => return r,
        };
        // certificates whose request is not finished are listed only on request
        let with_pending = deleted || req.query.get("includePending").map(|v| v.eq_ignore_ascii_case("true")).unwrap_or(false);
        let c = Ctx::of(req);
        let rows: Vec<(String, Value)> = {
            let mut st = self.state.lock();
            let vault = st.vault(&req.account, now_secs(req));
            vault
                .certs
                .listing(deleted)
                .into_iter()
                .filter_map(|(k, item)| {
                    let v = match item.latest() {
                        Some(v) => v.clone(),
                        None if with_pending => stub_version(item),
                        None => return None,
                    };
                    Some((k, item_json(&c, &item.name, &v, false, item.deleted.as_ref())))
                })
                .collect()
        };
        let (values, token) = paging::page(rows, &params);
        paging::list_response(req, path, values, token)
    }

    pub fn list_certificates(&self, req: &Request) -> Response {
        self.list_cert_items(req, false, "/certificates")
    }

    pub fn list_deleted_certificates(&self, req: &Request) -> Response {
        self.list_cert_items(req, true, "/deletedcertificates")
    }

    pub fn list_certificate_versions(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let params = match paging::parse(req) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let c = Ctx::of(req);
        let rows: Vec<(String, Value)> = {
            let mut st = self.state.lock();
            let vault = st.vault(&req.account, now_secs(req));
            match vault.certs.active(name) {
                Some(item) => {
                    let mut rows: Vec<(String, Value)> =
                        item.versions.iter().map(|v| (v.version.clone(), item_json(&c, &item.name, v, true, None))).collect();
                    if rows.is_empty() && item.extra.operation.is_some() {
                        // a pending certificate has the one version `GET` shows
                        let v = stub_version(item);
                        rows.push((v.version.clone(), item_json(&c, &item.name, &v, true, None)));
                    }
                    rows.sort_by(|a, b| a.0.cmp(&b.0));
                    rows
                }
                None => vec![],
            }
        };
        let (values, token) = paging::page(rows, &params);
        paging::list_response(req, &format!("/certificates/{name}/versions"), values, token)
    }
}

struct NewParts {
    material: KeyMaterial,
    der: Vec<u8>,
    secret_value: String,
    secret_ct: String,
    parsed_nbf: i64,
    parsed_exp: i64,
}

/// `validity_months` of an imported certificate: its validity expressed in months (months of
/// 30.5 days since 7.5, of a twelfth of 365 days before).
fn validity_months(not_before: i64, not_after: i64, newer: bool) -> i64 {
    let days = (not_after - not_before) as f64 / 86_400.0;
    let months = if newer { days / 30.5 } else { days * 12.0 / 365.0 };
    (months.round() as i64).max(1)
}

pub(crate) fn spec_exportable(policy: &Value) -> bool {
    policy.pointer("/key_props/exportable").and_then(|e| e.as_bool()).unwrap_or(true)
}
