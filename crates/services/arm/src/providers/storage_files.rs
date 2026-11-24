//! `Microsoft.Storage/storageAccounts/fileServices/shares`: joined to the Azure Files service (`crates/services/file`).
//!
//! A share made through ARM is created in the file service with its quota, access tier and metadata; a share made through the file
//! REST API is listed and read by ARM (`sync` reads the service before every GET, PUT, PATCH, DELETE and list of a share); deleting
//! the ARM resource deletes the share. The file service is the source of truth for what a share has: ARM keeps a record only to
//! have an id to answer for, and the access policies it was given.
//!
//! The shapes come from recorded answers of a real account (November 2026): a PUT answers with the properties it was given, a GET
//! with all of them, and a list without the metadata and the access policies.

use std::sync::Arc;

use azemu_core::{storage::xml_escape, Service};
use serde_json::{json, Map, Value};

use super::dp_error;
use super::storage::Storage;
use crate::backends::{call, Call};
use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::state::{key, Stored};
use crate::util;

/// Pages of a share list one sync reads at most.
const MAX_PAGES: usize = 1_000;

/// 3 to 63 characters: lower-case letters, digits and hyphens, starting with a letter or digit, no two hyphens in a row (Microsoft
/// Learn, "Naming and referencing shares, directories, files, and metadata").
pub fn valid_share_name(n: &str) -> bool {
    (3..=63).contains(&n.len())
        && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !n.starts_with('-')
        && !n.ends_with('-')
        && !n.contains("--")
}

pub fn name_error() -> ArmError {
    // Azure's own words, with its spelling
    ArmError::bad_request("InvalidResourceName", "The specifed resource name contains invalid characters.")
}

/// What the file service says about one share.
struct ShareInfo {
    name: String,
    etag: String,
    last_modified: String,
    quota: u64,
    tier: String,
    tier_changed: String,
    lease_state: String,
    lease_status: String,
    protocols: String,
    metadata: Map<String, Value>,
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// The time of a list answer (RFC 1123) in the seven-digit form Azure Resource Manager writes.
fn iso(rfc1123: &str) -> String {
    azemu_core::time::parse_rfc1123(rfc1123).map(azemu_core::time::iso8601_7).unwrap_or_else(|| rfc1123.to_string())
}

fn parse_share(block: &str) -> Option<ShareInfo> {
    let text = |tag: &str| super::xml_text(block, tag).map(|v| unescape(&v));
    let mut metadata = Map::new();
    if let Some(m) = super::xml_text(block, "Metadata") {
        let mut rest = m.as_str();
        while let Some(start) = rest.find('<') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('>') else { break };
            let tag = &after[..end];
            if let Some(name) = tag.strip_suffix('/') {
                metadata.insert(name.trim().to_string(), json!(""));
                rest = &after[end + 1..];
                continue;
            }
            let close = format!("</{tag}>");
            let body = &after[end + 1..];
            let Some(at) = body.find(&close) else { break };
            metadata.insert(tag.to_string(), json!(unescape(&body[..at])));
            rest = &body[at + close.len()..];
        }
    }
    Some(ShareInfo {
        name: text("Name")?,
        etag: text("Etag").unwrap_or_default(),
        last_modified: iso(&text("Last-Modified").unwrap_or_default()),
        quota: text("Quota").and_then(|q| q.parse().ok()).unwrap_or(0),
        tier: text("AccessTier").unwrap_or_else(|| "TransactionOptimized".into()),
        tier_changed: iso(&text("AccessTierChangeTime").or_else(|| text("Last-Modified")).unwrap_or_default()),
        lease_state: text("LeaseState").unwrap_or_else(|| "available".into()),
        lease_status: text("LeaseStatus").unwrap_or_else(|| "unlocked".into()),
        protocols: text("EnabledProtocols").unwrap_or_else(|| "SMB".into()),
        metadata,
    })
}

impl ShareInfo {
    /// The properties of a stored share: what a GET shows (`share-get.json`).
    fn properties(&self, signed: Value) -> Map<String, Value> {
        let mut p = Map::new();
        p.insert("accessTier".into(), json!(self.tier));
        p.insert("accessTierChangeTime".into(), json!(self.tier_changed));
        p.insert("enabledProtocols".into(), json!(self.protocols));
        p.insert("lastModifiedTime".into(), json!(self.last_modified));
        p.insert("leaseState".into(), json!(self.lease_state));
        p.insert("leaseStatus".into(), json!(self.lease_status));
        p.insert("metadata".into(), Value::Object(self.metadata.clone()));
        p.insert("shareQuota".into(), json!(self.quota));
        p.insert("signedIdentifiers".into(), signed);
        p
    }
}

fn meta_headers(metadata: &Value) -> Vec<(String, String)> {
    metadata
        .as_object()
        .map(|m| m.iter().map(|(k, v)| (format!("x-ms-meta-{k}"), v.as_str().unwrap_or("").to_string())).collect())
        .unwrap_or_default()
}

/// The access policies of ARM (`signedIdentifiers`) as the body of Set Share ACL.
fn acl_xml(list: &[Value]) -> String {
    let mut x = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><SignedIdentifiers>");
    for i in list {
        x.push_str(&format!("<SignedIdentifier><Id>{}</Id>", xml_escape(i["id"].as_str().unwrap_or(""))));
        if let Some(p) = i.get("accessPolicy").filter(|p| p.is_object()) {
            x.push_str("<AccessPolicy>");
            for (tag, field) in [("Start", "startTime"), ("Expiry", "expiryTime"), ("Permission", "permission")] {
                if let Some(v) = p[field].as_str() {
                    x.push_str(&format!("<{tag}>{}</{tag}>", xml_escape(v)));
                }
            }
            x.push_str("</AccessPolicy>");
        }
        x.push_str("</SignedIdentifier>");
    }
    x.push_str("</SignedIdentifiers>");
    x
}

impl Storage {
    /// Every share of an account, with its metadata (the service's list, page after page).
    async fn list_file_shares(&self, file: &Arc<dyn Service>, account: &str, prefix: &str, suffix: &str) -> ArmResult<Vec<ShareInfo>> {
        let mut out = Vec::new();
        let mut marker = String::new();
        for _ in 0..MAX_PAGES {
            let mut query = String::from("comp=list&include=metadata");
            if !prefix.is_empty() {
                query.push_str(&format!("&prefix={}", super::enc(prefix)));
            }
            if !marker.is_empty() {
                query.push_str(&format!("&marker={}", super::enc(&marker)));
            }
            let r = call(
                file,
                Call {
                    method: "GET",
                    path: "/",
                    query: &query,
                    headers: vec![],
                    body: vec![],
                    label: "file",
                    account,
                    port_suffix: suffix,
                },
            )
            .await;
            if r.status().as_u16() >= 400 {
                return Err(dp_error(&r));
            }
            let xml = String::from_utf8_lossy(r.body()).to_string();
            out.extend(super::xml_all(&xml, "Share").iter().filter_map(|b| parse_share(b)));
            match super::xml_text(&xml, "NextMarker").filter(|m| !m.is_empty() && *m != marker) {
                Some(m) => marker = unescape(&m),
                None => return Ok(out),
            }
        }
        Err(ArmError::new(500, "InternalServerError", format!("The list of shares of '{account}' has more than {MAX_PAGES} pages.")))
    }

    /// Bring ARM's records of the shares of one account in line with the file service: a share made through the file API gets a
    /// record, a record whose share is gone (deleted through the file API) is dropped. Does nothing when the account or its
    /// `fileServices/default` is not an ARM resource.
    pub(super) async fn sync_shares(&self, cx: &Cx<'_>, sub: &str, rg: &str, names: &[String]) -> ArmResult<()> {
        let Some(file) = self.b.file.clone() else { return Ok(()) };
        if names.len() < 2 || !names[1].eq_ignore_ascii_case("default") || rg.is_empty() {
            return Ok(());
        }
        let account = names[0].clone();
        let parent_types = ["storageAccounts".to_string(), "fileServices".to_string()];
        let parent_id = crate::ids::resource_id(sub, rg, "Microsoft.Storage", &parent_types, &[account.clone(), "default".to_string()]);
        if !cx.inner.state.lock().resources.contains_key(&key(&parent_id)) {
            return Ok(());
        }
        let suffix = cx.port_suffix();
        let shares = self.list_file_shares(&file, &account, "", &suffix).await?;
        let types = ["storageAccounts".to_string(), "fileServices".to_string(), "shares".to_string()];
        let prefix = format!("{}/shares/", key(&parent_id));
        let mut st = cx.inner.state.lock();
        let known: Vec<String> = st
            .resources
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .filter(|(_, s)| s.types.len() == 3)
            .map(|(k, _)| k.clone())
            .collect();
        for info in &shares {
            let id =
                crate::ids::resource_id(sub, rg, "Microsoft.Storage", &types, &[account.clone(), "default".to_string(), info.name.clone()]);
            let k = key(&id);
            let existing = st.resources.get(&k).cloned();
            let signed =
                existing.as_ref().map(|e| e.body["properties"]["signedIdentifiers"].clone()).filter(Value::is_array).unwrap_or(json!([]));
            let body = json!({
                "id": id,
                "name": info.name,
                "type": crate::ids::type_string("Microsoft.Storage", &types),
                "etag": info.etag,
                "properties": Value::Object(info.properties(signed)),
            });
            let stored = Stored {
                id,
                sub: sub.to_string(),
                rg: rg.to_string(),
                ns: "Microsoft.Storage".to_string(),
                types: types.to_vec(),
                names: vec![account.clone(), "default".to_string(), info.name.clone()],
                body,
                extra: existing.as_ref().map(|e| e.extra.clone()).unwrap_or(Value::Object(Map::new())),
                created: existing.as_ref().map(|e| e.created).unwrap_or(cx.now),
            };
            st.resources.insert(k, stored);
        }
        for k in known {
            let name = st.resources.get(&k).map(|s| s.name().to_string()).unwrap_or_default();
            if !shares.iter().any(|s| s.name.eq_ignore_ascii_case(&name)) {
                st.resources.remove(&k);
            }
        }
        Ok(())
    }

    /// PUT of a share: create it in the file service, or update what ARM carries when it is there (the same as for containers).
    pub(super) async fn put_share(&self, cx: &Cx<'_>, res: &mut Stored) -> ArmResult<()> {
        let props = util::props_mut(&mut res.body);
        props.remove("provisioningState");
        let quota = props.get("shareQuota").filter(|v| !v.is_null()).cloned();
        let tier = props.get("accessTier").and_then(Value::as_str).map(str::to_string);
        let protocols = props.get("enabledProtocols").and_then(Value::as_str).map(str::to_string);
        let metadata = props.get("metadata").filter(|m| m.is_object()).cloned().unwrap_or(json!({}));
        let signed = props.get("signedIdentifiers").and_then(Value::as_array).cloned();
        if let Some(q) = &quota {
            if q.as_u64().is_none() {
                return Err(ArmError::bad_request("InvalidInput", "shareQuota must be a whole number of GiB."));
            }
        }
        let account = res.name_at(0).to_string();
        let name = res.name().to_string();
        let signed_value = json!(signed.clone().unwrap_or_default());
        let Some(file) = self.b.file.clone() else {
            // no file service behind: keep what the request said, with the defaults of Azure
            let info = ShareInfo {
                name: name.clone(),
                etag: azemu_core::etag::new_etag(),
                last_modified: cx.now_iso(),
                quota: quota.as_ref().and_then(Value::as_u64).unwrap_or(5120),
                tier: tier.unwrap_or_else(|| "TransactionOptimized".into()),
                tier_changed: cx.now_iso(),
                lease_state: "available".into(),
                lease_status: "unlocked".into(),
                protocols: protocols.unwrap_or_else(|| "SMB".into()),
                metadata: metadata.as_object().cloned().unwrap_or_default(),
            };
            res.body["etag"] = json!(info.etag);
            res.body["properties"] = Value::Object(info.properties(signed_value));
            return Ok(());
        };
        let suffix = cx.port_suffix();
        let path = format!("/{name}");
        let mut headers: Vec<(String, String)> = meta_headers(&metadata);
        if let Some(q) = quota.as_ref().and_then(Value::as_u64) {
            headers.push(("x-ms-share-quota".into(), q.to_string()));
        }
        if let Some(t) = &tier {
            headers.push(("x-ms-access-tier".into(), t.clone()));
        }
        if let Some(p) = &protocols {
            headers.push(("x-ms-enabled-protocols".into(), p.clone()));
        }
        let send = |query: &'static str, h: Vec<(String, String)>| {
            let file = file.clone();
            let account = account.clone();
            let path = path.clone();
            let suffix = suffix.clone();
            async move {
                let hv: Vec<(&str, String)> = h.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                call(
                    &file,
                    Call {
                        method: "PUT",
                        path: &path,
                        query,
                        headers: hv,
                        body: vec![],
                        label: "file",
                        account: &account,
                        port_suffix: &suffix,
                    },
                )
                .await
            }
        };
        let r = send("restype=share", headers.clone()).await;
        let status = r.status().as_u16();
        if status == 409 {
            // there already (made through the file API, or this is an update): set what ARM carries
            let prop_h: Vec<(String, String)> =
                headers.iter().filter(|(k, _)| k == "x-ms-share-quota" || k == "x-ms-access-tier").cloned().collect();
            if !prop_h.is_empty() {
                let p = send("restype=share&comp=properties", prop_h).await;
                if p.status().as_u16() >= 400 {
                    return Err(dp_error(&p));
                }
            }
            let meta_h: Vec<(String, String)> = headers.iter().filter(|(k, _)| k.starts_with("x-ms-meta-")).cloned().collect();
            let m = send("restype=share&comp=metadata", meta_h).await;
            if m.status().as_u16() >= 400 {
                return Err(dp_error(&m));
            }
        } else if status >= 400 {
            return Err(dp_error(&r));
        }
        if let Some(list) = &signed {
            let body = acl_xml(list).into_bytes();
            let hv = vec![("Content-Type", "application/xml".to_string())];
            let a = call(
                &file,
                Call {
                    method: "PUT",
                    path: &path,
                    query: "restype=share&comp=acl",
                    headers: hv,
                    body,
                    label: "file",
                    account: &account,
                    port_suffix: &suffix,
                },
            )
            .await;
            if a.status().as_u16() >= 400 {
                return Err(dp_error(&a));
            }
        }
        // what the service made of it: the default quota, the tier, the etag
        let info = self.list_file_shares(&file, &account, &name, &suffix).await?.into_iter().find(|s| s.name == name).ok_or_else(|| {
            ArmError::new(500, "InternalServerError", format!("The share '{name}' is not in the file service after it was written."))
        })?;
        res.body["etag"] = json!(info.etag);
        res.body["properties"] = Value::Object(info.properties(signed_value));
        Ok(())
    }

    /// DELETE of a share. A share that is already gone is a success. `force` (the account or the group goes) drops its snapshots and
    /// breaks a lease; otherwise `x-ms-snapshot: include` asks for the snapshots to go too, as on Azure.
    pub(super) async fn delete_share(&self, cx: &Cx<'_>, res: &Stored, force: bool) -> ArmResult<()> {
        let Some(file) = self.b.file.clone() else { return Ok(()) };
        let account = res.name_at(0).to_string();
        let path = format!("/{}", res.name());
        let suffix = cx.port_suffix();
        let include = force || cx.req.header("x-ms-snapshot").is_some_and(|v| v.eq_ignore_ascii_case("include"));
        let del = || {
            let mut headers: Vec<(&str, String)> = Vec::new();
            if include {
                headers.push(("x-ms-delete-snapshots", if force { "include-leased" } else { "include" }.to_string()));
            }
            if let Some(l) = cx.req.header("x-ms-leaseid") {
                headers.push(("x-ms-lease-id", l.to_string()));
            }
            call(
                &file,
                Call {
                    method: "DELETE",
                    path: &path,
                    query: "restype=share",
                    headers,
                    body: vec![],
                    label: "file",
                    account: &account,
                    port_suffix: &suffix,
                },
            )
        };
        let mut r = del().await;
        let leased = |r: &azemu_core::Response| {
            r.headers().get("x-ms-error-code").and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("Lease"))
        };
        if force && r.status().as_u16() >= 400 && leased(&r) {
            let headers = vec![("x-ms-lease-action", "break".to_string()), ("x-ms-lease-break-period", "0".to_string())];
            call(
                &file,
                Call {
                    method: "PUT",
                    path: &path,
                    query: "restype=share&comp=lease",
                    headers,
                    body: vec![],
                    label: "file",
                    account: &account,
                    port_suffix: &suffix,
                },
            )
            .await;
            r = del().await;
        }
        let s = r.status().as_u16();
        if s < 300 || s == 404 {
            return Ok(());
        }
        let e = dp_error(&r);
        Err(ArmError::conflict(
            &e.code,
            format!("The share '{}' of the storage account '{account}' cannot be deleted: {}", res.name(), e.message),
        ))
    }

    /// What a client sees of a share: the answer of a PUT or a PATCH holds the properties that were sent, a list leaves out the
    /// metadata and the access policies, a GET shows everything (the recorded answers).
    pub(super) fn render_share(&self, cx: &Cx<'_>, res: &Stored) -> Value {
        let mut v = res.body.clone();
        let path = cx.req.decoded_path().trim_end_matches('/').to_ascii_lowercase();
        let method = cx.req.method.as_str();
        if matches!(method, "PUT" | "PATCH") && path == res.id.to_ascii_lowercase() {
            let sent: Vec<String> = serde_json::from_slice::<Value>(&cx.req.body)
                .ok()
                .and_then(|b| b["properties"].as_object().map(|p| p.keys().cloned().collect()))
                .unwrap_or_default();
            let all = util::props_mut(&mut v);
            all.retain(|k, _| sent.iter().any(|s| s.eq_ignore_ascii_case(k)));
            if let Some(o) = v.as_object_mut() {
                o.remove("etag");
            }
        } else if method == "GET" && path.ends_with("/shares") {
            let all = util::props_mut(&mut v);
            all.remove("metadata");
            all.remove("signedIdentifiers");
        }
        v
    }

    /// `fileServices/default`: only that name; a property the request leaves out keeps its value. The retention of deleted shares
    /// (`shareDeleteRetentionPolicy`) is stored and returned; the file service has one global setting for it
    /// (`AZEMU_FILE_SOFT_DELETE_DAYS`), which this does not change.
    pub(super) fn put_file_service(&self, cx: &Cx<'_>, res: &mut Stored) -> ArmResult<()> {
        if res.name() != "default" {
            return Err(ArmError::bad_request("InvalidResourceName", "The only service name allowed is 'default'."));
        }
        let old =
            cx.inner.state.lock().resources.get(&key(&res.id)).and_then(|s| s.body.get("properties").and_then(Value::as_object).cloned());
        let props = util::props_mut(&mut res.body);
        for (k, v) in old.unwrap_or_default() {
            if k != "provisioningState" && !props.contains_key(&k) {
                props.insert(k, v);
            }
        }
        util::set_default(props, "cors", json!({"corsRules": []}));
        util::set_default(props, "protocolSettings", json!({"smb": {}}));
        util::set_default(props, "shareDeleteRetentionPolicy", json!({"enabled": true, "days": 7}));
        props.remove("provisioningState");
        Ok(())
    }
}
