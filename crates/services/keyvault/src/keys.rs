//! Keys: create, import, get, update, delete (soft), deleted list/get/recover/purge,
//! list, versions, backup, restore, rotate, random bytes. The crypto operations
//! (sign, verify, encrypt, ...) are in `keys_crypto.rs`.

use azemu_core::{Request, Response};
use serde_json::{json, Map, Value};

use crate::auth::no_content;
use crate::backup;
use crate::common::*;
use crate::crypto::{self, Curve, KeyMaterial};
use crate::errors::*;
use crate::jwk::{self, Family};
use crate::paging;
use crate::rotation::RotationPolicy;
use crate::state::{norm, Deleted, Item, Versioned};
use crate::KeyVaultService;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct KeyVersion {
    pub version: String,
    /// As the client asked for it: RSA, RSA-HSM, EC, EC-HSM, oct, oct-HSM.
    pub kty: String,
    pub family: Family,
    #[serde(with = "crate::persist::material")]
    pub material: KeyMaterial,
    pub key_ops: Vec<String>,
    pub attrs: Attrs,
    pub tags: Tags,
    pub release_policy: Option<Value>,
    /// RSA modulus bits or symmetric key bits.
    pub key_size: Option<u32>,
    pub exportable: bool,
    /// True for the key behind a certificate.
    pub managed: bool,
}

impl Versioned for KeyVersion {
    fn version(&self) -> &str {
        &self.version
    }
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct KeyExtra {
    pub policy: Option<RotationPolicy>,
}

impl KeyVersion {
    fn is_hsm(&self) -> bool {
        self.kty.ends_with("-HSM")
    }
}

fn key_attrs_json(c: &Ctx, v: &KeyVersion) -> Value {
    let mut m = v.attrs.to_json(&c.profile);
    if c.profile.exportable {
        m.insert("exportable".into(), json!(v.exportable));
    }
    if let (true, Some(bits)) = (c.profile.key_size, v.key_size) {
        m.insert("key_size".into(), json!(bits));
    }
    if c.profile.hsm_platform {
        m.insert("hsmPlatform".into(), json!(if v.is_hsm() { "2" } else { "0" }));
    }
    Value::Object(m)
}

/// A key bundle: the JWK plus attributes and tags. With `deleted` it is a deleted key.
pub fn key_json(c: &Ctx, name: &str, v: &KeyVersion, deleted: Option<&Deleted>) -> Value {
    let base = c.base.as_str();
    let kid = format!("{base}/keys/{name}/{}", v.version);
    let mut m = Map::new();
    m.insert("key".into(), jwk::public_jwk(&kid, &v.kty, &v.key_ops, &v.material));
    m.insert("attributes".into(), key_attrs_json(c, v));
    if !v.tags.is_empty() {
        m.insert("tags".into(), tags_json(&v.tags));
    }
    if let Some(rp) = &v.release_policy {
        m.insert("release_policy".into(), rp.clone());
    }
    if v.managed {
        m.insert("managed".into(), json!(true));
    }
    if let Some(d) = deleted {
        m.insert("recoveryId".into(), json!(format!("{base}/deletedkeys/{name}")));
        m.insert("deletedDate".into(), json!(d.deleted_date));
        m.insert("scheduledPurgeDate".into(), json!(d.scheduled_purge_date));
    }
    Value::Object(m)
}

/// A list entry: `kid` (with the version only for version lists), attributes, tags.
fn item_json(c: &Ctx, name: &str, v: &KeyVersion, with_version: bool, deleted: Option<&Deleted>) -> Value {
    let base = c.base.as_str();
    let kid = if with_version { format!("{base}/keys/{name}/{}", v.version) } else { format!("{base}/keys/{name}") };
    let mut m = Map::new();
    m.insert("kid".into(), json!(kid));
    m.insert("attributes".into(), key_attrs_json(c, v));
    if !v.tags.is_empty() {
        m.insert("tags".into(), tags_json(&v.tags));
    }
    if v.managed {
        m.insert("managed".into(), json!(true));
    }
    if let Some(d) = deleted {
        m.insert("recoveryId".into(), json!(format!("{base}/deletedkeys/{name}")));
        m.insert("deletedDate".into(), json!(d.deleted_date));
        m.insert("scheduledPurgeDate".into(), json!(d.scheduled_purge_date));
    }
    Value::Object(m)
}

/// The release policy as the service stores it: content type and immutability filled in.
fn normalize_release_policy(v: &Value) -> Value {
    let mut m = v.as_object().cloned().unwrap_or_default();
    m.entry("contentType").or_insert_with(|| json!("application/json; charset=utf-8"));
    m.entry("immutable").or_insert_with(|| json!(false));
    Value::Object(m)
}

fn what(name: &str, version: Option<&str>) -> String {
    match version {
        Some(v) if !v.is_empty() => format!("{name}/{v}"),
        _ => name.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Create / import
// ---------------------------------------------------------------------------

struct Spec {
    kty: &'static str,
    family: Family,
    bits: u32,
    exponent: u64,
    curve: Curve,
    key_ops: Vec<String>,
    patch: AttrPatch,
    tags: Tags,
    release_policy: Option<Value>,
    exportable: bool,
}

/// A missing or empty `key_ops` list on create and import means the default operations of the key type (the
/// Java SDK sends `"key_ops": []` for a key it was given without operations). Update keeps its own rules.
fn ops_or_default(ops: Option<Vec<String>>, family: Family) -> Vec<String> {
    match ops {
        Some(o) if !o.is_empty() => o,
        _ => jwk::default_key_ops(family),
    }
}

fn parse_exportable(body: &Map<String, Value>) -> bool {
    body.get("attributes").and_then(|a| a.get("exportable")).and_then(|e| e.as_bool()).unwrap_or(false)
}

fn parse_create(body: &Map<String, Value>) -> Result<Spec, Response> {
    let kty_text = str_field(body, "kty")?;
    let (family, kty) = jwk::parse_kty(kty_text).ok_or_else(|| bad_parameter(&format!("The key type '{kty_text}' is not supported.")))?;
    let size = match body.get("key_size") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or_else(|| bad_parameter("'key_size' must be a number."))?),
    };
    let mut spec = Spec {
        kty,
        family,
        bits: 0,
        exponent: 65537,
        curve: Curve::P256,
        key_ops: ops_or_default(jwk::parse_key_ops(body, "key_ops")?, family),
        patch: parse_attr_patch(body)?,
        tags: parse_tags(body)?.unwrap_or_default(),
        release_policy: body.get("release_policy").filter(|v| !v.is_null()).map(normalize_release_policy),
        exportable: parse_exportable(body),
    };
    match family {
        Family::Rsa => {
            spec.bits = size.unwrap_or(2048);
            if !matches!(spec.bits, 2048 | 3072 | 4096) {
                return Err(bad_parameter(&format!(
                    "The RSA key size {} is not supported. Supported sizes are 2048, 3072 and 4096.",
                    spec.bits
                )));
            }
            // 0 means "not given": the Java SDK sends `public_exponent: 0` when the option is not set.
            if let Some(e) = body.get("public_exponent").and_then(|e| e.as_u64()).filter(|e| *e != 0) {
                if e < 3 || e % 2 == 0 || e >= (1 << 32) {
                    return Err(bad_parameter("The RSA public exponent is not valid."));
                }
                spec.exponent = e;
            }
        }
        Family::Ec => {
            if let Some(c) = body.get("crv").and_then(|c| c.as_str()) {
                spec.curve = Curve::parse(c).ok_or_else(|| bad_parameter(&format!("The curve '{c}' is not supported.")))?;
            }
        }
        Family::Oct => {
            spec.bits = size.unwrap_or(256);
            if !matches!(spec.bits, 128 | 192 | 256) {
                return Err(bad_parameter(&format!("The symmetric key size {} is not supported.", spec.bits)));
            }
        }
    }
    Ok(spec)
}

/// Blocking: generate the key material for a spec.
fn generate(spec: &Spec) -> Result<KeyMaterial, Response> {
    match spec.family {
        Family::Rsa => crypto::generate_rsa(spec.bits as usize, spec.exponent).map_err(|e| bad_parameter(&format!("{e:?}"))),
        Family::Ec => Ok(crypto::generate_ec(spec.curve)),
        Family::Oct => Ok(crypto::generate_oct((spec.bits / 8) as usize)),
    }
}

impl KeyVaultService {
    /// Put a new version under `name`; 409 if the name is in the deleted state.
    fn add_key_version(&self, req: &Request, name: &str, v: KeyVersion) -> Response {
        let c = Ctx::of(req);
        let now = now_secs(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now);
        if vault.keys.is_deleted(name) {
            return name_is_deleted(Kind::Key, name);
        }
        let display = vault.keys.active(name).map(|i| i.name.clone()).unwrap_or_else(|| name.to_string());
        let json = key_json(&c, &display, &v, None);
        match vault.keys.active_mut(name) {
            Some(item) => item.versions.push(v),
            None => {
                vault
                    .keys
                    .items
                    .insert(norm(name), Item { name: name.to_string(), versions: vec![v], deleted: None, extra: KeyExtra::default() });
            }
        }
        json_response(200, &json)
    }

    fn name_conflict(&self, req: &Request, name: &str) -> Option<Response> {
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        vault.keys.is_deleted(name).then(|| name_is_deleted(Kind::Key, name))
    }

    pub async fn create_key(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let spec = match parse_create(&body) {
            Ok(s) => s,
            Err(r) => return r,
        };
        if let Some(r) = self.name_conflict(req, name) {
            return r;
        }
        let now = now_secs(req);
        let spec = std::sync::Arc::new(spec);
        let s2 = spec.clone();
        let material = match tokio::task::spawn_blocking(move || generate(&s2)).await {
            Ok(Ok(m)) => m,
            Ok(Err(r)) => return r,
            Err(_) => return json_response(500, &json!({"error": {"code": "InternalError", "message": "key generation failed"}})),
        };
        let mut attrs = Attrs::new(now);
        attrs.apply(&spec.patch);
        let v = KeyVersion {
            version: new_version(),
            kty: spec.kty.to_string(),
            family: spec.family,
            material,
            key_ops: spec.key_ops.clone(),
            attrs,
            tags: spec.tags.clone(),
            release_policy: spec.release_policy.clone(),
            key_size: (spec.family != Family::Ec).then_some(spec.bits),
            exportable: spec.exportable,
            managed: false,
        };
        self.add_key_version(req, name, v)
    }

    pub async fn import_key(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let Some(Value::Object(key)) = body.get("key").cloned() else {
            return bad_parameter("The parameter 'key' is required.");
        };
        let mut kty = match str_field(&key, "kty") {
            Ok(k) => k.to_string(),
            Err(r) => return r,
        };
        if body.get("hsm").and_then(|h| h.as_bool()) == Some(true) && !kty.to_ascii_lowercase().ends_with("-hsm") {
            kty.push_str("-HSM");
        }
        let Some((family, canonical)) = jwk::parse_kty(&kty) else {
            return bad_parameter(&format!("The key type '{kty}' is not supported."));
        };
        let patch = match parse_attr_patch(&body) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let tags = match parse_tags(&body) {
            Ok(t) => t.unwrap_or_default(),
            Err(r) => return r,
        };
        if let Some(r) = self.name_conflict(req, name) {
            return r;
        }
        let kty2 = kty.clone();
        let imported = match tokio::task::spawn_blocking(move || jwk::import_jwk(&key, &kty2)).await {
            Ok(Ok(i)) => i,
            Ok(Err(r)) => return r,
            Err(_) => return json_response(500, &json!({"error": {"code": "InternalError", "message": "key import failed"}})),
        };
        let now = now_secs(req);
        let mut attrs = Attrs::new(now);
        attrs.apply(&patch);
        let v = KeyVersion {
            version: new_version(),
            kty: canonical.to_string(),
            family,
            material: imported.material,
            key_ops: ops_or_default(imported.key_ops, family),
            attrs,
            tags,
            release_policy: body.get("release_policy").filter(|v| !v.is_null()).map(normalize_release_policy),
            key_size: imported.key_size,
            exportable: parse_exportable(&body),
            managed: false,
        };
        self.add_key_version(req, name, v)
    }

    pub async fn rotate_key(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let now = now_secs(req);
        let template = {
            let mut st = self.state.lock();
            let vault = st.vault(&req.account, now);
            match vault.keys.active(name).and_then(|i| i.latest().cloned()) {
                Some(v) => v,
                None => return not_found(Kind::Key, name),
            }
        };
        let spec = Spec {
            kty: jwk::parse_kty(&template.kty).map(|k| k.1).unwrap_or("RSA"),
            family: template.family,
            bits: template.key_size.unwrap_or(2048),
            exponent: 65537,
            curve: match &template.material {
                KeyMaterial::Ec { curve, .. } => *curve,
                _ => Curve::P256,
            },
            key_ops: vec![],
            patch: AttrPatch::default(),
            tags: Tags::new(),
            release_policy: None,
            exportable: false,
        };
        let material = match tokio::task::spawn_blocking(move || generate(&spec)).await {
            Ok(Ok(m)) => m,
            Ok(Err(r)) => return r,
            Err(_) => return json_response(500, &json!({"error": {"code": "InternalError", "message": "key generation failed"}})),
        };
        let mut attrs = Attrs::new(now);
        attrs.enabled = template.attrs.enabled;
        let v = KeyVersion { version: new_version(), material, attrs, ..template };
        self.add_key_version(req, name, v)
    }

    // -----------------------------------------------------------------------
    // Read / update / delete
    // -----------------------------------------------------------------------

    pub fn get_key(&self, req: &Request, name: &str, version: Option<&str>) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        let Some(item) = vault.keys.active(name) else {
            return not_found(Kind::Key, &what(name, version));
        };
        match item.pick(version) {
            Some(v) => json_response(200, &key_json(&c, &item.name, v, None)),
            None => not_found(Kind::Key, &what(name, version)),
        }
    }

    pub fn update_key(&self, req: &Request, name: &str, version: Option<&str>) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let key_ops = match jwk::parse_key_ops(&body, "key_ops") {
            Ok(k) => k,
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
        let Some(item) = vault.keys.active_mut(name) else {
            return not_found(Kind::Key, &what(name, version));
        };
        let display = item.name.clone();
        let target = match version {
            None | Some("") => item.versions.last_mut(),
            Some(v) => item.find_mut(v),
        };
        let Some(v) = target else {
            return not_found(Kind::Key, &what(name, version));
        };
        if let Some(ops) = key_ops {
            v.key_ops = ops;
        }
        if let Some(t) = tags {
            v.tags = t;
        }
        if let Some(rp) = body.get("release_policy").filter(|r| !r.is_null()) {
            if v.release_policy.as_ref().and_then(|p| p.get("immutable")).and_then(|i| i.as_bool()) == Some(true) {
                return bad_parameter("AKV.SKR.1020: Immutable Key Release Policy cannot be modified.");
            }
            v.release_policy = Some(normalize_release_policy(rp));
        }
        v.attrs.apply(&patch);
        v.attrs.updated = now;
        json_response(200, &key_json(&c, &display, v, None))
    }

    pub fn delete_key(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let now = now_secs(req);
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now);
        let Some(item) = vault.keys.active_mut(name) else {
            return not_found(Kind::Key, name);
        };
        let d = Deleted::at(now);
        item.deleted = Some(d);
        match item.latest() {
            Some(latest) => json_response(200, &key_json(&c, &item.name, latest, Some(&d))),
            None => not_found(Kind::Key, name),
        }
    }

    pub fn get_deleted_key(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        match vault.keys.deleted(name) {
            Some(item) => match (item.deleted, item.latest()) {
                (Some(d), Some(latest)) => json_response(200, &key_json(&c, &item.name, latest, Some(&d))),
                _ => deleted_not_found(Kind::Key, name),
            },
            // a name that is nowhere (never made, or purged) gets the long text of a missing key: recording
            // `test_key_client.pyTestKeyClienttest_purge[2025-07-01_vault].json`, entries 46 to 50
            None if vault.keys.items.contains_key(&norm(name)) => deleted_not_found(Kind::Key, name),
            None => not_found(Kind::Key, name),
        }
    }

    pub fn purge_key(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        if vault.keys.is_deleted(name) {
            vault.keys.items.remove(&norm(name));
            no_content()
        } else {
            deleted_not_found(Kind::Key, name)
        }
    }

    pub fn recover_key(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        let Some(item) = vault.keys.items.get_mut(&norm(name)).filter(|i| i.deleted.is_some()) else {
            return deleted_not_found(Kind::Key, name);
        };
        item.deleted = None;
        match item.latest() {
            Some(latest) => json_response(200, &key_json(&c, &item.name, latest, None)),
            None => deleted_not_found(Kind::Key, name),
        }
    }

    // -----------------------------------------------------------------------
    // Lists
    // -----------------------------------------------------------------------

    fn list_key_items(&self, req: &Request, deleted: bool, path: &str) -> Response {
        let params = match paging::parse(req) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let c = Ctx::of(req);
        let rows: Vec<(String, Value)> = {
            let mut st = self.state.lock();
            let vault = st.vault(&req.account, now_secs(req));
            vault
                .keys
                .listing(deleted)
                .into_iter()
                .filter_map(|(k, item)| {
                    let latest = item.latest()?;
                    Some((k, item_json(&c, &item.name, latest, false, item.deleted.as_ref())))
                })
                .collect()
        };
        let (values, token) = paging::page(rows, &params);
        paging::list_response(req, path, values, token)
    }

    pub fn list_keys(&self, req: &Request) -> Response {
        self.list_key_items(req, false, "/keys")
    }

    pub fn list_deleted_keys(&self, req: &Request) -> Response {
        self.list_key_items(req, true, "/deletedkeys")
    }

    pub fn list_key_versions(&self, req: &Request, name: &str) -> Response {
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
            match vault.keys.active(name) {
                Some(item) => {
                    let mut rows: Vec<(String, Value)> =
                        item.versions.iter().map(|v| (v.version.clone(), item_json(&c, &item.name, v, true, None))).collect();
                    rows.sort_by(|a, b| a.0.cmp(&b.0));
                    rows
                }
                None => vec![],
            }
        };
        let (values, token) = paging::page(rows, &params);
        paging::list_response(req, &format!("/keys/{name}/versions"), values, token)
    }

    // -----------------------------------------------------------------------
    // Backup / restore
    // -----------------------------------------------------------------------

    pub fn backup_key(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        let Some(item) = vault.keys.active(name) else {
            return not_found(Kind::Key, name);
        };
        let versions: Vec<Value> = item
            .versions
            .iter()
            .map(|v| {
                json!({
                    "version": v.version,
                    "kty": v.kty,
                    "key_ops": v.key_ops,
                    "key_size": v.key_size,
                    "material": jwk::material_to_json(&v.material),
                    "enabled": v.attrs.enabled,
                    "nbf": v.attrs.nbf,
                    "exp": v.attrs.exp,
                    "created": v.attrs.created,
                    "updated": v.attrs.updated,
                    "tags": tags_json(&v.tags),
                    "release_policy": v.release_policy,
                    "exportable": v.exportable,
                    "managed": v.managed,
                })
            })
            .collect();
        let policy = item.extra.policy.as_ref().map(|p| {
            json!({
                "actions": p.lifetime_actions.iter().map(|a| json!([a.action, a.trigger_kind, a.trigger_value])).collect::<Vec<_>>(),
                "expiry": p.expiry_time, "created": p.created, "updated": p.updated,
            })
        });
        let payload = json!({ "name": item.name, "versions": versions, "policy": policy });
        json_response(200, &json!({ "value": backup::encode(Kind::Key, &payload) }))
    }

    pub async fn restore_key(&self, req: &Request) -> Response {
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let text = match str_field(&body, "value") {
            Ok(t) => t.to_string(),
            Err(r) => return r,
        };
        let payload = match backup::decode(Kind::Key, &text) {
            Ok(p) => p,
            Err(r) => return r,
        };
        // Rebuilding RSA keys does big-number work: keep it off the executor.
        let parsed = tokio::task::spawn_blocking(move || parse_backup(&payload)).await;
        let (name, versions, policy) = match parsed {
            Ok(Some(p)) => p,
            _ => return bad_parameter("The backup blob is not valid."),
        };
        let c = Ctx::of(req);
        let now = now_secs(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now);
        let Some(last) = versions.last() else {
            return bad_parameter("The backup blob is not valid.");
        };
        if vault.keys.exists(&name) {
            return restore_conflict(Kind::Key, &format!("{}/keys/{name}/{}", c.base, last.version));
        }
        let json = key_json(&c, &name, last, None);
        vault.keys.items.insert(norm(&name), Item { name, versions, deleted: None, extra: KeyExtra { policy } });
        json_response(200, &json)
    }

    // -----------------------------------------------------------------------
    // Random bytes
    // -----------------------------------------------------------------------

    pub fn random_bytes(&self, req: &Request) -> Response {
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let count = match body.get("count").and_then(|c| c.as_u64()) {
            Some(c) if (1..=128).contains(&c) => c as usize,
            _ => return bad_parameter("The 'count' parameter must be between 1 and 128."),
        };
        json_response(200, &json!({ "value": b64u(&crypto::random_bytes(count)) }))
    }
}

type ParsedBackup = (String, Vec<KeyVersion>, Option<RotationPolicy>);

fn parse_backup(payload: &Value) -> Option<ParsedBackup> {
    let name = payload.get("name")?.as_str()?.to_string();
    if !valid_name(&name) {
        return None;
    }
    let mut versions = Vec::new();
    for v in payload.get("versions")?.as_array()? {
        let kty = v.get("kty")?.as_str()?;
        let (family, canonical) = jwk::parse_kty(kty)?;
        versions.push(KeyVersion {
            version: v.get("version")?.as_str()?.to_string(),
            kty: canonical.to_string(),
            family,
            material: jwk::material_from_json(v.get("material")?)?,
            key_ops: v.get("key_ops")?.as_array()?.iter().filter_map(|o| o.as_str().map(String::from)).collect(),
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
            release_policy: v.get("release_policy").filter(|r| !r.is_null()).cloned(),
            key_size: v.get("key_size").and_then(|s| s.as_u64()).map(|s| s as u32),
            exportable: v.get("exportable").and_then(|x| x.as_bool()).unwrap_or(false),
            managed: v.get("managed").and_then(|x| x.as_bool()).unwrap_or(false),
        });
    }
    if versions.is_empty() {
        return None;
    }
    let policy = payload.get("policy").filter(|p| !p.is_null()).and_then(|p| {
        let actions = p
            .get("actions")?
            .as_array()?
            .iter()
            .filter_map(|a| {
                let a = a.as_array()?;
                Some(crate::rotation::LifetimeAction {
                    action: a.first()?.as_str()?.to_string(),
                    trigger_kind: a.get(1)?.as_str()?.to_string(),
                    trigger_value: a.get(2)?.as_str()?.to_string(),
                })
            })
            .collect();
        Some(RotationPolicy {
            lifetime_actions: actions,
            expiry_time: p.get("expiry").and_then(|e| e.as_str()).map(String::from),
            created: p.get("created")?.as_i64()?,
            updated: p.get("updated")?.as_i64()?,
        })
    });
    Some((name, versions, policy))
}
