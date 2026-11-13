//! Certificate operations (`/pending`, merge) and backup/restore.

use azemu_core::{Request, Response};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::{json, Value};

use crate::backup;
use crate::cert_x509 as x509;
use crate::certificates::*;
use crate::common::*;
use crate::crypto::KeyMaterial;
use crate::errors::*;
use crate::jwk;
use crate::keys::{KeyExtra, KeyVersion};
use crate::secrets::SecretVersion;
use crate::state::{norm, Item};
use crate::KeyVaultService;

impl KeyVaultService {
    // -----------------------------------------------------------------------
    // Operations
    // -----------------------------------------------------------------------

    pub fn get_certificate_operation(&self, req: &Request, name: &str) -> Response {
        let base = req.ctx.endpoint.base_url();
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        match vault.certs.active(name).and_then(|i| i.extra.operation.as_ref().map(|o| (i.name.clone(), o))) {
            Some((display, op)) => operation_response(200, req, &display, &operation_json(&base, &display, op)),
            None => pending_not_found(name),
        }
    }

    pub fn update_certificate_operation(&self, req: &Request, name: &str) -> Response {
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let cancel = body.get("cancellation_requested").and_then(|c| c.as_bool()).unwrap_or(false);
        let base = req.ctx.endpoint.base_url();
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        let Some(item) = vault.certs.active_mut(name) else {
            return pending_not_found(name);
        };
        let display = item.name.clone();
        let Some(op) = item.extra.operation.as_mut() else {
            return pending_not_found(name);
        };
        if cancel {
            op.cancellation_requested = true;
            if op.status == "inProgress" {
                op.status = "cancelled".into();
            }
        }
        let cancelled = op.cancellation_requested;
        let json = operation_json(&base, &display, op);
        if cancelled && item.versions.is_empty() {
            item.extra.stub_cancelled = true;
        }
        json_response(200, &json)
    }

    pub fn delete_certificate_operation(&self, req: &Request, name: &str) -> Response {
        let base = req.ctx.endpoint.base_url();
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        let Some(item) = vault.certs.active_mut(name) else {
            return pending_not_found(name);
        };
        let display = item.name.clone();
        let Some(op) = item.extra.operation.take() else {
            return pending_not_found(name);
        };
        let json = operation_json(&base, &display, &op);
        // a request that never produced a certificate stays as a certificate without a
        // version (it can still be deleted, as the real service allows)
        if item.versions.is_empty() && op.cancellation_requested {
            item.extra.stub_cancelled = true;
        }
        json_response(200, &json)
    }

    /// `POST /certificates/{name}/pending/merge`: finish a pending request with a
    /// certificate signed elsewhere.
    pub async fn merge_certificate(&self, req: &Request, name: &str) -> Response {
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let Some(Value::Array(x5c)) = body.get("x5c") else {
            return bad_parameter("The parameter 'x5c' is required.");
        };
        let mut chain: Vec<Vec<u8>> = Vec::new();
        for c in x5c {
            match c.as_str().and_then(x509::decode_value).and_then(x509::certificate_der) {
                Some(d) => chain.push(d),
                None => return bad_parameter("Every 'x5c' entry must be a base64 certificate."),
            }
        }
        if chain.is_empty() {
            return bad_parameter("The parameter 'x5c' is required.");
        }
        let patch = match parse_attr_patch(&body) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let tags = match parse_tags(&body) {
            Ok(t) => t.unwrap_or_default(),
            Err(r) => return r,
        };
        let now = now_secs(req);
        let pending = {
            let mut st = self.state.lock();
            let vault = st.vault(&req.account, now);
            vault.certs.active(name).and_then(|i| i.extra.operation.clone().map(|o| (o, i.extra.policy.clone())))
        };
        let Some((op, policy)) = pending else {
            return pending_not_found(name);
        };
        let Some((version, material)) = op.pending_key.clone() else {
            return bad_parameter("There is no pending certificate request to merge into.");
        };
        let Some(policy) = policy else {
            return pending_not_found(name);
        };
        let pol = policy.clone();
        let task = tokio::task::spawn_blocking(move || -> Result<Merged, String> {
            let leaf = chain.first().cloned().ok_or("empty chain")?;
            let parsed = x509::parse_cert(&leaf)?;
            if parsed.spki != x509::spki_of_material(&material)? {
                return Err("The public key of the certificate does not match the pending request.".into());
            }
            let (value, ct) = secret_for(&material, &chain, &pol)?;
            Ok((leaf, parsed.not_before, parsed.not_after, value, ct, material))
        })
        .await;
        let (der, nbf, exp, secret_value, secret_ct, material) = match task {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => return bad_parameter(&e),
            Err(_) => return json_response(500, &json!({"error": {"code": "InternalError", "message": "merge failed"}})),
        };
        let c = Ctx::of(req);
        let nv = NewVersion {
            version,
            der,
            material,
            secret_value,
            secret_content_type: secret_ct,
            exportable: spec_exportable(&policy),
            policy: policy.clone(),
            tags,
            patch,
            parsed_nbf: nbf,
            parsed_exp: exp,
            csr: op.csr.clone(),
            issuer: op.issuer.clone(),
            preserve_order: op.preserve_order,
        };
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now);
        let display = vault.certs.active(name).map(|i| i.name.clone()).unwrap_or_else(|| name.to_string());
        let cert = install(vault, &display, &nv, now);
        json_response(201, &cert_json(&c, &display, &cert, Some(&policy), true, None))
    }

    // -----------------------------------------------------------------------
    // Backup / restore
    // -----------------------------------------------------------------------

    pub fn backup_certificate(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        let Some(item) = vault.certs.active(name) else {
            return not_found(Kind::Certificate, name);
        };
        let versions: Vec<Value> = item
            .versions
            .iter()
            .filter_map(|v| {
                let key = vault.keys.items.get(&norm(name)).and_then(|k| k.find(&v.version))?;
                let secret = vault.secrets.items.get(&norm(name)).and_then(|s| s.find(&v.version))?;
                Some(json!({
                    "version": v.version, "cer": STANDARD.encode(&v.der), "tags": tags_json(&v.tags),
                    "enabled": v.attrs.enabled, "nbf": v.attrs.nbf, "exp": v.attrs.exp,
                    "created": v.attrs.created, "updated": v.attrs.updated,
                    "preserveCertOrder": v.preserve_order,
                    "key": jwk::material_to_json(&key.material), "exportable": key.exportable,
                    "secret": secret.value, "secret_content_type": secret.content_type,
                }))
            })
            .collect();
        let payload = json!({"name": item.name, "policy": item.extra.policy, "versions": versions});
        json_response(200, &json!({ "value": backup::encode(Kind::Certificate, &payload) }))
    }

    pub async fn restore_certificate(&self, req: &Request) -> Response {
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let text = match str_field(&body, "value") {
            Ok(t) => t.to_string(),
            Err(r) => return r,
        };
        let payload = match backup::decode(Kind::Certificate, &text) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let parsed = tokio::task::spawn_blocking(move || parse_cert_backup(&payload)).await;
        let Ok(Some((name, policy, versions))) = parsed else {
            return bad_parameter("The backup blob is not valid.");
        };
        let now = now_secs(req);
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now);
        let Some(last) = versions.last() else {
            return bad_parameter("The backup blob is not valid.");
        };
        if vault.certs.exists(&name) || name_taken_by_other(vault, &name) {
            return restore_conflict(Kind::Certificate, &format!("{}/certificates/{name}/{}", c.base, last.0.version));
        }
        let mut keys = Vec::new();
        let mut secrets = Vec::new();
        let mut certs = Vec::new();
        for (cert, material, exportable, sval, sct) in &versions {
            let (kty, family) = key_kty_family(material);
            let key_size = match material {
                KeyMaterial::Rsa(k) => {
                    use rsa::traits::PublicKeyParts;
                    Some(k.n().bit_width())
                }
                _ => None,
            };
            keys.push(KeyVersion {
                version: cert.version.clone(),
                kty: kty.into(),
                family,
                material: material.clone(),
                key_ops: jwk::default_key_ops(family),
                attrs: Attrs {
                    enabled: cert.attrs.enabled,
                    nbf: cert.attrs.nbf,
                    exp: cert.attrs.exp,
                    created: cert.attrs.created,
                    updated: cert.attrs.updated,
                },
                tags: Tags::new(),
                release_policy: None,
                key_size,
                exportable: *exportable,
                managed: true,
            });
            secrets.push(SecretVersion {
                version: cert.version.clone(),
                value: sval.clone(),
                content_type: Some(sct.clone()),
                tags: cert.tags.clone(),
                attrs: Attrs {
                    enabled: cert.attrs.enabled,
                    nbf: cert.attrs.nbf,
                    exp: cert.attrs.exp,
                    created: cert.attrs.created,
                    updated: cert.attrs.updated,
                },
                kid: Some(format!("/keys/{name}/{}", cert.version)),
                managed: true,
            });
            certs.push(cert.clone());
        }
        let json = cert_json(&c, &name, &last.0, Some(&policy), false, None);
        vault.keys.items.insert(norm(&name), Item { name: name.clone(), versions: keys, deleted: None, extra: KeyExtra::default() });
        vault.secrets.items.insert(norm(&name), Item { name: name.clone(), versions: secrets, deleted: None, extra: () });
        vault.certs.items.insert(
            norm(&name),
            Item { name, versions: certs, deleted: None, extra: CertExtra { policy: Some(policy), operation: None, ..Default::default() } },
        );
        json_response(200, &json)
    }
}

/// Leaf certificate, its validity, the secret value and content type, and the key.
type Merged = (Vec<u8>, i64, i64, String, String, KeyMaterial);

type RestoredVersion = (CertVersion, KeyMaterial, bool, String, String);

fn parse_cert_backup(p: &Value) -> Option<(String, Value, Vec<RestoredVersion>)> {
    let name = p.get("name")?.as_str()?.to_string();
    if !valid_name(&name) {
        return None;
    }
    let policy = p.get("policy").filter(|x| x.is_object())?.clone();
    let mut out = Vec::new();
    for v in p.get("versions")?.as_array()? {
        let der = STANDARD.decode(v.get("cer")?.as_str()?.as_bytes()).ok()?;
        let cert = CertVersion {
            version: v.get("version")?.as_str()?.to_string(),
            x5t: x509::thumbprint(&der),
            der,
            attrs: Attrs {
                enabled: v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true),
                nbf: v.get("nbf").and_then(|x| x.as_i64()),
                exp: v.get("exp").and_then(|x| x.as_i64()),
                created: v.get("created")?.as_i64()?,
                updated: v.get("updated")?.as_i64()?,
            },
            tags: v
                .get("tags")
                .and_then(|t| t.as_object())
                .map(|t| t.iter().filter_map(|(k, x)| x.as_str().map(|s| (k.clone(), s.to_string()))).collect())
                .unwrap_or_default(),
            preserve_order: v.get("preserveCertOrder").and_then(|p| p.as_bool()),
        };
        out.push((
            cert,
            jwk::material_from_json(v.get("key")?)?,
            v.get("exportable").and_then(|e| e.as_bool()).unwrap_or(true),
            v.get("secret")?.as_str()?.to_string(),
            v.get("secret_content_type")?.as_str()?.to_string(),
        ));
    }
    if out.is_empty() {
        return None;
    }
    Some((name, policy, out))
}
