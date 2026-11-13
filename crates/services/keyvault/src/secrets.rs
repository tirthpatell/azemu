//! Secrets: set, get, update, delete (soft), deleted list/get/recover/purge, list,
//! versions, backup and restore.

use azemu_core::{Request, Response};
use serde_json::{json, Map, Value};

use crate::auth::no_content;
use crate::backup;
use crate::common::*;
use crate::errors::*;
use crate::paging;
use crate::state::{norm, Deleted, Item, Versioned};
use crate::KeyVaultService;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SecretVersion {
    pub version: String,
    pub value: String,
    pub content_type: Option<String>,
    pub tags: Tags,
    pub attrs: Attrs,
    /// Set for the secret behind a certificate: the id of the linked key.
    pub kid: Option<String>,
    /// True for the secret behind a certificate.
    pub managed: bool,
}

impl Versioned for SecretVersion {
    fn version(&self) -> &str {
        &self.version
    }
}

/// JSON for one secret. `list` items leave out the value (and the id's version when
/// `with_version` is false).
fn secret_json(c: &Ctx, name: &str, v: &SecretVersion, with_version: bool, with_value: bool, deleted: Option<&Deleted>) -> Value {
    let base = c.base.as_str();
    let mut m = Map::new();
    if with_value {
        m.insert("value".into(), json!(v.value));
    }
    let id = if with_version { format!("{base}/secrets/{name}/{}", v.version) } else { format!("{base}/secrets/{name}") };
    m.insert("id".into(), json!(id));
    if let Some(ct) = &v.content_type {
        m.insert("contentType".into(), json!(ct));
    }
    m.insert("attributes".into(), Value::Object(v.attrs.to_json(&c.profile)));
    if !v.tags.is_empty() {
        m.insert("tags".into(), tags_json(&v.tags));
    }
    if let Some(k) = &v.kid {
        // stored as a path (`/keys/name/version`) so that it is right on whatever address the vault is reached
        // by, also after a restart; an absolute address from an old backup is returned as it is
        let kid = if k.starts_with('/') { format!("{}{k}", c.base) } else { k.clone() };
        m.insert("kid".into(), json!(kid));
    }
    if v.managed {
        m.insert("managed".into(), json!(true));
    }
    if let Some(d) = deleted {
        m.insert("recoveryId".into(), json!(format!("{base}/deletedsecrets/{name}")));
        m.insert("deletedDate".into(), json!(d.deleted_date));
        m.insert("scheduledPurgeDate".into(), json!(d.scheduled_purge_date));
    }
    Value::Object(m)
}

fn version_to_backup(v: &SecretVersion) -> Value {
    json!({
        "version": v.version,
        "value": v.value,
        "contentType": v.content_type,
        "tags": tags_json(&v.tags),
        "enabled": v.attrs.enabled,
        "nbf": v.attrs.nbf,
        "exp": v.attrs.exp,
        "created": v.attrs.created,
        "updated": v.attrs.updated,
        "kid": v.kid,
        "managed": v.managed,
    })
}

fn version_from_backup(v: &Value) -> Option<SecretVersion> {
    let o = v.as_object()?;
    Some(SecretVersion {
        version: o.get("version")?.as_str()?.to_string(),
        value: o.get("value")?.as_str()?.to_string(),
        content_type: o.get("contentType").and_then(|x| x.as_str()).map(String::from),
        tags: o
            .get("tags")
            .and_then(|t| t.as_object())
            .map(|t| t.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
            .unwrap_or_default(),
        attrs: Attrs {
            enabled: o.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true),
            nbf: o.get("nbf").and_then(|x| x.as_i64()),
            exp: o.get("exp").and_then(|x| x.as_i64()),
            created: o.get("created")?.as_i64()?,
            updated: o.get("updated")?.as_i64()?,
        },
        kid: o.get("kid").and_then(|x| x.as_str()).map(String::from),
        managed: o.get("managed").and_then(|x| x.as_bool()).unwrap_or(false),
    })
}

fn disabled_secret(op: &str) -> Response {
    forbidden(&format!("Operation {op} is not allowed on a disabled secret."), "SecretDisabled")
}

impl KeyVaultService {
    pub fn set_secret(&self, req: &Request, name: &str) -> Response {
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
        if value.len() > MAX_SECRET_BYTES {
            return bad_parameter(&format!(
                "The secret value is too large: {} bytes. The maximum is {MAX_SECRET_BYTES} bytes (25 KiB).",
                value.len()
            ));
        }
        let content_type = match opt_str_field(&body, "contentType") {
            Ok(c) => c.map(String::from),
            Err(r) => return r,
        };
        let tags = match parse_tags(&body) {
            Ok(t) => t.unwrap_or_default(),
            Err(r) => return r,
        };
        let patch = match parse_attr_patch(&body) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let now = now_secs(req);
        let mut attrs = Attrs::new(now);
        attrs.apply(&patch);
        let version = SecretVersion { version: new_version(), value, content_type, tags, attrs, kid: None, managed: false };

        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now);
        if vault.secrets.is_deleted(name) {
            return name_is_deleted(Kind::Secret, name);
        }
        let json = secret_json(&c, name, &version, true, true, None);
        match vault.secrets.active_mut(name) {
            Some(item) => item.versions.push(version),
            None => {
                vault.secrets.items.insert(norm(name), Item { name: name.to_string(), versions: vec![version], deleted: None, extra: () });
            }
        }
        json_response(200, &json)
    }

    pub fn get_secret(&self, req: &Request, name: &str, version: Option<&str>) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        let what = || match version {
            Some(v) if !v.is_empty() => format!("{name}/{v}"),
            _ => name.to_string(),
        };
        let Some(item) = vault.secrets.active(name) else {
            return not_found(Kind::Secret, &what());
        };
        let Some(v) = item.pick(version) else {
            return not_found(Kind::Secret, &what());
        };
        if !v.attrs.enabled {
            return disabled_secret("get");
        }
        if let Some(out) = req.query.get("outContentType") {
            return match crate::certificates::convert_secret(v, out) {
                Ok(converted) => {
                    let mut v2 = v.clone();
                    v2.value = converted;
                    v2.content_type = Some(out.to_string());
                    json_response(200, &secret_json(&c, &item.name, &v2, true, true, None))
                }
                Err(r) => r,
            };
        }
        json_response(200, &secret_json(&c, &item.name, v, true, true, None))
    }

    pub fn update_secret(&self, req: &Request, name: &str, version: Option<&str>) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let content_type = match opt_str_field(&body, "contentType") {
            Ok(c) => c.map(String::from),
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
        let what = || match version {
            Some(v) if !v.is_empty() => format!("{name}/{v}"),
            _ => name.to_string(),
        };
        let Some(item) = vault.secrets.active_mut(name) else {
            return not_found(Kind::Secret, &what());
        };
        let display = item.name.clone();
        let target = match version {
            None | Some("") => item.versions.last_mut(),
            Some(v) => item.find_mut(v),
        };
        let Some(v) = target else {
            return not_found(Kind::Secret, &what());
        };
        if let Some(ct) = content_type {
            v.content_type = Some(ct);
        }
        if let Some(t) = tags {
            v.tags = t;
        }
        v.attrs.apply(&patch);
        v.attrs.updated = now;
        json_response(200, &secret_json(&c, &display, v, true, false, None))
    }

    pub fn delete_secret(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let now = now_secs(req);
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now);
        let Some(item) = vault.secrets.active_mut(name) else {
            return not_found(Kind::Secret, name);
        };
        let d = Deleted::at(now);
        item.deleted = Some(d);
        let Some(latest) = item.latest() else {
            return not_found(Kind::Secret, name);
        };
        json_response(200, &secret_json(&c, &item.name, latest, true, false, Some(&d)))
    }

    pub fn get_deleted_secret(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        let Some(item) = vault.secrets.deleted(name) else {
            return if vault.secrets.items.contains_key(&norm(name)) {
                deleted_not_found(Kind::Secret, name)
            } else {
                // a name that is nowhere (never made, or purged) gets the long text of a missing secret: recording
                // `test_secrets_client.pyTestSecretClienttest_purge[2025-07-01].json`, entries 47 to 53
                not_found(Kind::Secret, name)
            };
        };
        let (Some(d), Some(latest)) = (item.deleted, item.latest()) else {
            return deleted_not_found(Kind::Secret, name);
        };
        // Real Key Vault refuses to read a disabled secret, deleted or not.
        if !latest.attrs.enabled {
            return disabled_secret("get");
        }
        json_response(200, &secret_json(&c, &item.name, latest, true, false, Some(&d)))
    }

    pub fn purge_secret(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        if vault.secrets.is_deleted(name) {
            vault.secrets.items.remove(&norm(name));
            no_content()
        } else {
            deleted_not_found(Kind::Secret, name)
        }
    }

    pub fn recover_secret(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        let Some(item) = vault.secrets.items.get_mut(&norm(name)).filter(|i| i.deleted.is_some()) else {
            return deleted_not_found(Kind::Secret, name);
        };
        item.deleted = None;
        let Some(latest) = item.latest() else {
            return deleted_not_found(Kind::Secret, name);
        };
        json_response(200, &secret_json(&c, &item.name, latest, true, false, None))
    }

    fn list_secret_items(&self, req: &Request, deleted: bool, path: &str) -> Response {
        let params = match paging::parse(req) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let c = Ctx::of(req);
        let rows: Vec<(String, Value)> = {
            let mut st = self.state.lock();
            let vault = st.vault(&req.account, now_secs(req));
            vault
                .secrets
                .listing(deleted)
                .into_iter()
                .filter_map(|(k, item)| {
                    let latest = item.latest()?;
                    Some((k, secret_json(&c, &item.name, latest, false, false, item.deleted.as_ref())))
                })
                .collect()
        };
        let (values, token) = paging::page(rows, &params);
        paging::list_response(req, path, values, token)
    }

    pub fn list_secrets(&self, req: &Request) -> Response {
        self.list_secret_items(req, false, "/secrets")
    }

    pub fn list_deleted_secrets(&self, req: &Request) -> Response {
        self.list_secret_items(req, true, "/deletedsecrets")
    }

    pub fn list_secret_versions(&self, req: &Request, name: &str) -> Response {
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
            match vault.secrets.active(name) {
                Some(item) => {
                    let mut rows: Vec<(String, Value)> =
                        item.versions.iter().map(|v| (v.version.clone(), secret_json(&c, &item.name, v, true, false, None))).collect();
                    rows.sort_by(|a, b| a.0.cmp(&b.0));
                    rows
                }
                None => vec![],
            }
        };
        let (values, token) = paging::page(rows, &params);
        paging::list_response(req, &format!("/secrets/{name}/versions"), values, token)
    }

    pub fn backup_secret(&self, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return invalid_name(name);
        }
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        let Some(item) = vault.secrets.active(name) else {
            return not_found(Kind::Secret, name);
        };
        let payload = json!({
            "name": item.name,
            "versions": item.versions.iter().map(version_to_backup).collect::<Vec<_>>(),
        });
        json_response(200, &json!({ "value": backup::encode(Kind::Secret, &payload) }))
    }

    pub fn restore_secret(&self, req: &Request) -> Response {
        let body = match body_object(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let text = match str_field(&body, "value") {
            Ok(t) => t,
            Err(r) => return r,
        };
        let payload = match backup::decode(Kind::Secret, text) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let invalid = || bad_parameter("The backup blob is not valid.");
        let Some(name) = payload.get("name").and_then(|n| n.as_str()).map(String::from) else {
            return invalid();
        };
        let versions: Option<Vec<SecretVersion>> =
            payload.get("versions").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(version_from_backup).collect());
        let Some(versions) = versions.filter(|v| !v.is_empty()) else {
            return invalid();
        };
        if !valid_name(&name) {
            return invalid();
        }
        let c = Ctx::of(req);
        let mut st = self.state.lock();
        let vault = st.vault(&req.account, now_secs(req));
        let last = versions.last().cloned();
        let Some(last) = last else { return invalid() };
        if vault.secrets.exists(&name) {
            return restore_conflict(Kind::Secret, &format!("{}/secrets/{name}/{}", c.base, last.version));
        }
        let json = secret_json(&c, &name, &last, true, false, None);
        vault.secrets.items.insert(norm(&name), Item { name, versions, deleted: None, extra: () });
        json_response(200, &json)
    }
}
