//! Certificate policies, issuers and contacts.

use azemu_core::{Request, Response};
use serde_json::{json, Map, Value};

use crate::common::*;
use crate::errors::*;
use crate::KeyVaultService;

/// What the policy says about the key to generate.
pub struct KeySpec {
    /// `RSA` or `EC`
    pub kty: &'static str,
    pub bits: usize,
    pub curve: crate::crypto::Curve,
}

fn merge_default(target: &mut Map<String, Value>, key: &str, default: Value) {
    if target.get(key).map(|v| v.is_null()).unwrap_or(true) {
        target.insert(key.to_string(), default);
    }
}

/// Fill in what the service fills in: defaults for the parts the request left out.
/// `input` is the policy of a request; `base` the policy to start from, if any.
pub fn normalize_policy(input: &Value, base: Option<&Value>, now: i64) -> Result<Value, Response> {
    let mut out = base.and_then(|b| b.as_object().cloned()).unwrap_or_default();
    let Some(i) = input.as_object() else {
        return Err(bad_parameter("The 'policy' field must be an object."));
    };
    for section in ["key_props", "secret_props", "x509_props", "issuer"] {
        if let Some(Value::Object(new)) = i.get(section) {
            out.insert(section.to_string(), Value::Object(new.clone()));
        }
    }
    if let Some(la) = i.get("lifetime_actions").filter(|v| v.is_array()) {
        out.insert("lifetime_actions".into(), la.clone());
    }
    // key_props
    let mut kp = out.get("key_props").and_then(|v| v.as_object().cloned()).unwrap_or_default();
    merge_default(&mut kp, "exportable", json!(true));
    merge_default(&mut kp, "kty", json!("RSA"));
    merge_default(&mut kp, "reuse_key", json!(false));
    let kty = kp.get("kty").and_then(|k| k.as_str()).unwrap_or("RSA").to_ascii_lowercase();
    if kty.starts_with("ec") {
        merge_default(&mut kp, "crv", json!("P-256"));
        let size = match kp.get("crv").and_then(|c| c.as_str()) {
            Some("P-384") => 384,
            Some("P-521") => 521,
            _ => 256,
        };
        merge_default(&mut kp, "key_size", json!(size));
    } else {
        merge_default(&mut kp, "key_size", json!(2048));
    }
    out.insert("key_props".into(), Value::Object(kp));
    // secret_props
    let mut sp = out.get("secret_props").and_then(|v| v.as_object().cloned()).unwrap_or_default();
    merge_default(&mut sp, "contentType", json!("application/x-pkcs12"));
    out.insert("secret_props".into(), Value::Object(sp));
    // x509_props
    let mut xp = out.get("x509_props").and_then(|v| v.as_object().cloned()).unwrap_or_default();
    merge_default(&mut xp, "ekus", json!(["1.3.6.1.5.5.7.3.1", "1.3.6.1.5.5.7.3.2"]));
    merge_default(&mut xp, "key_usage", json!(["digitalSignature", "keyEncipherment"]));
    merge_default(&mut xp, "validity_months", json!(12));
    merge_default(&mut xp, "basic_constraints", json!({"ca": false}));
    out.insert("x509_props".into(), Value::Object(xp));
    merge_default(&mut out, "lifetime_actions", json!([{"trigger": {"lifetime_percentage": 80}, "action": {"action_type": "AutoRenew"}}]));
    merge_default(&mut out, "issuer", json!({"name": "Self"}));
    // attributes
    let mut attrs = out.get("attributes").and_then(|v| v.as_object().cloned()).unwrap_or_default();
    attrs.entry("enabled").or_insert(json!(true));
    attrs.entry("created").or_insert(json!(now));
    attrs.insert("updated".into(), json!(now));
    if let Some(Value::Object(a)) = i.get("attributes") {
        if let Some(e) = a.get("enabled") {
            attrs.insert("enabled".into(), e.clone());
        }
    }
    out.insert("attributes".into(), Value::Object(attrs));
    out.remove("id");
    Ok(Value::Object(out))
}

/// Reject a subject that is not an X.500 distinguished name (`400 BadParameter`).
pub fn check_subject(policy: &Value) -> Result<(), Response> {
    match policy.pointer("/x509_props/subject").and_then(|s| s.as_str()) {
        Some(s) => crate::cert_x509::validate_subject(s).map_err(|e| bad_parameter(&e)),
        None => Ok(()),
    }
}

pub fn policy_json(base: &str, name: &str, policy: &Value) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), json!(format!("{base}/certificates/{name}/policy")));
    if let Some(o) = policy.as_object() {
        for (k, v) in o {
            m.insert(k.clone(), v.clone());
        }
    }
    Value::Object(m)
}

pub fn issuer_name(policy: &Value) -> String {
    policy.pointer("/issuer/name").and_then(|n| n.as_str()).unwrap_or("Self").to_string()
}

/// The key the policy asks for.
pub fn key_spec(policy: &Value) -> Result<KeySpec, Response> {
    let kp = policy.get("key_props").cloned().unwrap_or(json!({}));
    let kty = kp.get("kty").and_then(|k| k.as_str()).unwrap_or("RSA").to_ascii_lowercase();
    if kty.starts_with("rsa") {
        let bits = kp.get("key_size").and_then(|s| s.as_u64()).unwrap_or(2048) as usize;
        if !matches!(bits, 2048 | 3072 | 4096) {
            return Err(bad_parameter(&format!("The RSA key size {bits} is not supported. Supported sizes are 2048, 3072 and 4096.")));
        }
        Ok(KeySpec { kty: "RSA", bits, curve: crate::crypto::Curve::P256 })
    } else if kty.starts_with("ec") {
        let c = kp.get("crv").and_then(|c| c.as_str()).unwrap_or("P-256");
        let curve = crate::crypto::Curve::parse(c).ok_or_else(|| bad_parameter(&format!("The curve '{c}' is not supported.")))?;
        Ok(KeySpec { kty: "EC", bits: 0, curve })
    } else {
        Err(bad_parameter(&format!("The key type '{kty}' is not supported for certificates.")))
    }
}

// ---------------------------------------------------------------------------
// Issuers
// ---------------------------------------------------------------------------

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Issuer {
    pub name: String,
    pub provider: Option<String>,
    pub account_id: Option<String>,
    pub password: Option<String>,
    pub org_details: Option<Value>,
    pub enabled: bool,
    pub created: i64,
    pub updated: i64,
}

fn issuer_json(base: &str, i: &Issuer) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), json!(format!("{base}/certificates/issuers/{}", i.name)));
    if let Some(p) = &i.provider {
        m.insert("provider".into(), json!(p));
    }
    if let Some(a) = &i.account_id {
        m.insert("credentials".into(), json!({"account_id": a}));
    }
    if let Some(o) = &i.org_details {
        m.insert("org_details".into(), o.clone());
    }
    m.insert("attributes".into(), json!({"enabled": i.enabled, "created": i.created, "updated": i.updated}));
    Value::Object(m)
}

fn org_details(v: &Value) -> Value {
    let mut o = v.as_object().cloned().unwrap_or_default();
    o.entry("zip").or_insert(json!(0));
    Value::Object(o)
}

fn issuer_not_found() -> Response {
    json_response(404, &json!({"error": {"code": "CertificateIssuerNotFound", "message": "Issuer not found"}}))
}

fn issuer_name_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= 63 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl KeyVaultService {
    fn apply_issuer_body(i: &mut Issuer, body: &Map<String, Value>) -> Result<(), Response> {
        if let Some(p) = opt_str_field(body, "provider")? {
            i.provider = Some(p.to_string());
        }
        if let Some(Value::Object(c)) = body.get("credentials") {
            if let Some(a) = c.get("account_id").and_then(|a| a.as_str()) {
                i.account_id = Some(a.to_string());
            }
            if let Some(p) = c.get("password").and_then(|a| a.as_str()) {
                i.password = Some(p.to_string());
            }
        }
        if let Some(o) = body.get("org_details").filter(|o| o.is_object()) {
            i.org_details = Some(org_details(o));
        }
        if let Some(e) = body.get("attributes").and_then(|a| a.get("enabled")).and_then(|e| e.as_bool()) {
            i.enabled = e;
        }
        Ok(())
    }

    pub fn set_issuer(&self, req: &Request, name: &str) -> Response {
        if !issuer_name_ok(name) {
            return invalid_name(name);
        }
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        if body.get("provider").and_then(|p| p.as_str()).is_none() {
            return bad_parameter("The parameter 'provider' is required.");
        }
        let now = now_secs(req);
        let base = req.ctx.endpoint.base_url();
        let mut issuer = Issuer {
            name: name.to_string(),
            provider: None,
            account_id: None,
            password: None,
            org_details: None,
            enabled: true,
            created: now,
            updated: now,
        };
        if let Err(r) = Self::apply_issuer_body(&mut issuer, &body) {
            return r;
        }
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now);
        if let Some(old) = vault.issuers.get(&name.to_ascii_lowercase()) {
            issuer.created = old.created;
        }
        let json = issuer_json(&base, &issuer);
        vault.issuers.insert(name.to_ascii_lowercase(), issuer);
        json_response(200, &json)
    }

    pub fn update_issuer(&self, req: &Request, name: &str) -> Response {
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let now = now_secs(req);
        let base = req.ctx.endpoint.base_url();
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now);
        let Some(issuer) = vault.issuers.get_mut(&name.to_ascii_lowercase()) else {
            return issuer_not_found();
        };
        if let Err(r) = Self::apply_issuer_body(issuer, &body) {
            return r;
        }
        issuer.updated = now;
        json_response(200, &issuer_json(&base, issuer))
    }

    pub fn get_issuer(&self, req: &Request, name: &str) -> Response {
        let base = req.ctx.endpoint.base_url();
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        match vault.issuers.get(&name.to_ascii_lowercase()) {
            Some(i) => json_response(200, &issuer_json(&base, i)),
            None => issuer_not_found(),
        }
    }

    pub fn delete_issuer(&self, req: &Request, name: &str) -> Response {
        let base = req.ctx.endpoint.base_url();
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        match vault.issuers.remove(&name.to_ascii_lowercase()) {
            Some(i) => json_response(200, &issuer_json(&base, &i)),
            None => issuer_not_found(),
        }
    }

    pub fn list_issuers(&self, req: &Request) -> Response {
        let params = match crate::paging::parse(req) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let base = req.ctx.endpoint.base_url();
        let rows: Vec<(String, Value)> = {
            let mut st = self.state.lock();
            let vault = st.vault(&req.account, now_secs(req));
            vault
                .issuers
                .iter()
                .map(|(k, i)| {
                    let mut m = Map::new();
                    m.insert("id".into(), json!(format!("{base}/certificates/issuers/{}", i.name)));
                    if let Some(p) = &i.provider {
                        m.insert("provider".into(), json!(p));
                    }
                    (k.clone(), Value::Object(m))
                })
                .collect()
        };
        let (values, token) = crate::paging::page(rows, &params);
        crate::paging::list_response(req, "/certificates/issuers", values, token)
    }

    // -----------------------------------------------------------------------
    // Contacts
    // -----------------------------------------------------------------------

    pub fn set_contacts(&self, req: &Request) -> Response {
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let Some(Value::Array(contacts)) = body.get("contacts") else {
            return bad_parameter("The parameter 'contacts' is required.");
        };
        let base = req.ctx.endpoint.base_url();
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        vault.contacts = Some(contacts.clone());
        json_response(200, &json!({"id": format!("{base}/certificates/contacts"), "contacts": contacts}))
    }

    fn contacts_not_found() -> Response {
        json_response(404, &json!({"error": {"code": "ContactsNotFound", "message": "Contacts not found"}}))
    }

    pub fn get_contacts(&self, req: &Request) -> Response {
        let base = req.ctx.endpoint.base_url();
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        match &vault.contacts {
            Some(c) => json_response(200, &json!({"id": format!("{base}/certificates/contacts"), "contacts": c})),
            None => Self::contacts_not_found(),
        }
    }

    pub fn delete_contacts(&self, req: &Request) -> Response {
        let base = req.ctx.endpoint.base_url();
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        match vault.contacts.take() {
            Some(c) => json_response(200, &json!({"id": format!("{base}/certificates/contacts"), "contacts": c})),
            None => Self::contacts_not_found(),
        }
    }
}
