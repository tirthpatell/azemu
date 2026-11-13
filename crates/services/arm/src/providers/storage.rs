//! Microsoft.Storage: accounts and their blob containers, queues and tables.
//!
//! Accounts need nothing in the data plane (Blob, Queue and Table create an account on
//! first use). Containers, queues and tables are created and deleted through the
//! services' own HTTP API. Deleting an account removes everything in it.

use azemu_core::storage::StorageAccounts;
use azemu_core::{async_trait, etag, Response};
use serde_json::{json, Value};

use super::{bump_generation, dp_error, generation, stored_child, xml_all};

/// Pages of a list one delete reads at most (5000 names a page for Blob and Queue, 1000 for Table).
const MAX_LIST_PAGES: usize = 10_000;
use crate::backends::{call, Backends, Call};
use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::provider::{ActionTarget, Provider, Statuses};
use crate::state::Stored;
use crate::util;

pub struct Storage {
    pub b: Backends,
}

const TYPES: &[&str] = &[
    "storageAccounts",
    "storageAccounts/blobServices",
    "storageAccounts/blobServices/containers",
    "storageAccounts/queueServices",
    "storageAccounts/queueServices/queues",
    "storageAccounts/tableServices",
    "storageAccounts/tableServices/tables",
];

const PASSTHROUGH: &[&str] = &["storageAccounts/fileServices", "storageAccounts/managementPolicies", "storageAccounts/encryptionScopes"];

fn valid_account_name(n: &str) -> bool {
    (3..=24).contains(&n.len()) && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn tier_of(sku: &str) -> &'static str {
    if sku.starts_with("Premium") {
        "Premium"
    } else {
        "Standard"
    }
}

impl Storage {
    fn account_keys(&self, res: &Stored) -> [String; 2] {
        let name = res.name();
        let base = StorageAccounts::new().keys(name);
        let g1 = generation(res, "key1");
        let g2 = generation(res, "key2");
        [
            if g1 == 0 { base[0].clone() } else { util::derive_key(&format!("storage/{name}/key1"), g1) },
            if g2 == 0 { base[1].clone() } else { util::derive_key(&format!("storage/{name}/key2"), g2) },
        ]
    }

    fn keys_response(&self, res: &Stored) -> Response {
        let keys = self.account_keys(res);
        let created = res.created;
        let t = azemu_core::time::iso8601_7(created);
        util::json_ok(
            200,
            &json!({"keys": [
                {"keyName": "key1", "value": keys[0], "permissions": "FULL", "creationTime": t},
                {"keyName": "key2", "value": keys[1], "permissions": "FULL", "creationTime": t},
            ]}),
        )
    }

    /// Every name a list call gives, following the continuation marker to the last page. A failed call is an error (the caller
    /// must not go on as if the list were complete).
    async fn list_names(
        &self,
        svc: &std::sync::Arc<dyn azemu_core::Service>,
        label: &str,
        account: &str,
        suffix: &str,
    ) -> ArmResult<Vec<String>> {
        // <Container><Name>x</Name>... or <Queue><Name>x</Name>
        let unit = if label == "blob" { "Container" } else { "Queue" };
        let mut names = Vec::new();
        let mut marker = String::new();
        for _ in 0..MAX_LIST_PAGES {
            let query = if marker.is_empty() { "comp=list".to_string() } else { format!("comp=list&marker={}", super::enc(&marker)) };
            let r = call(
                svc,
                Call { method: "GET", path: "/", query: &query, headers: vec![], body: vec![], label, account, port_suffix: suffix },
            )
            .await;
            if r.status().as_u16() >= 400 {
                return Err(dp_error(&r));
            }
            let xml = String::from_utf8_lossy(r.body()).to_string();
            names.extend(xml_all(&xml, unit).iter().filter_map(|u| super::xml_text(u, "Name")));
            match super::xml_text(&xml, "NextMarker").filter(|m| !m.is_empty() && *m != marker) {
                Some(m) => marker = m,
                None => return Ok(names),
            }
        }
        Err(ArmError::new(
            500,
            "InternalServerError",
            format!("The list of {label} resources of '{account}' has more than {MAX_LIST_PAGES} pages."),
        ))
    }

    async fn list_tables(&self, table: &std::sync::Arc<dyn azemu_core::Service>, account: &str, suffix: &str) -> ArmResult<Vec<String>> {
        let mut names = Vec::new();
        let mut next = String::new();
        for _ in 0..MAX_LIST_PAGES {
            let query = if next.is_empty() { String::new() } else { format!("NextTableName={}", super::enc(&next)) };
            let r = call(
                table,
                Call {
                    method: "GET",
                    path: "/Tables",
                    query: &query,
                    headers: vec![("Accept", "application/json;odata=nometadata".into())],
                    body: vec![],
                    label: "table",
                    account,
                    port_suffix: suffix,
                },
            )
            .await;
            if r.status().as_u16() >= 400 {
                return Err(dp_error(&r));
            }
            if let Ok(v) = serde_json::from_slice::<Value>(r.body()) {
                names.extend(
                    v["value"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|t| t["TableName"].as_str().map(String::from)).collect::<Vec<_>>())
                        .unwrap_or_default(),
                );
            }
            match r.headers().get("x-ms-continuation-NextTableName").and_then(|v| v.to_str().ok()).filter(|n| !n.is_empty() && *n != next) {
                Some(n) => next = n.to_string(),
                None => return Ok(names),
            }
        }
        Err(ArmError::new(500, "InternalServerError", format!("The list of tables of '{account}' has more than {MAX_LIST_PAGES} pages.")))
    }

    /// Delete a container. `force` (an account is being deleted: Azure drops everything in it, leases included) breaks a lease first.
    /// A 404 is success (it is gone). Anything else that is not 2xx is the error to give back: Azure's Storage resource provider
    /// refuses to delete a container the data plane refuses to delete (a lease, a legal hold, an immutability policy).
    async fn delete_container(
        &self,
        blob: &std::sync::Arc<dyn azemu_core::Service>,
        account: &str,
        name: &str,
        suffix: &str,
        force: bool,
    ) -> ArmResult<()> {
        let path = format!("/{name}");
        let del = || {
            call(
                blob,
                Call {
                    method: "DELETE",
                    path: &path,
                    query: "restype=container",
                    headers: vec![],
                    body: vec![],
                    label: "blob",
                    account,
                    port_suffix: suffix,
                },
            )
        };
        let mut r = del().await;
        let leased = |r: &Response| matches!(r.headers().get("x-ms-error-code").and_then(|v| v.to_str().ok()), Some("LeaseIdMissing"));
        if force && r.status().as_u16() >= 400 && leased(&r) {
            let headers = vec![("x-ms-lease-action", "break".to_string()), ("x-ms-lease-break-period", "0".to_string())];
            call(
                blob,
                Call {
                    method: "PUT",
                    path: &path,
                    query: "restype=container&comp=lease",
                    headers,
                    body: vec![],
                    label: "blob",
                    account,
                    port_suffix: suffix,
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
            format!("The container '{name}' of the storage account '{account}' cannot be deleted: {}", e.message),
        ))
    }

    /// Remove every container, queue and table of an account, or say why not: every page of every list is read, every failed
    /// delete is reported (the first one's code, the count in the message), and ARM keeps the account then.
    async fn wipe_account(&self, cx: &Cx<'_>, account: &str) -> ArmResult<()> {
        let suffix = cx.port_suffix();
        let mut failures: Vec<ArmError> = Vec::new();
        if let Some(blob) = self.b.blob_service() {
            for c in self.list_names(&blob, "blob", account, &suffix).await? {
                if let Err(e) = self.delete_container(&blob, account, &c, &suffix, true).await {
                    failures.push(e);
                }
            }
        }
        if let Some(queue) = &self.b.queue {
            for q in self.list_names(queue, "queue", account, &suffix).await? {
                let path = format!("/{q}");
                let r = call(
                    queue,
                    Call {
                        method: "DELETE",
                        path: &path,
                        query: "",
                        headers: vec![],
                        body: vec![],
                        label: "queue",
                        account,
                        port_suffix: &suffix,
                    },
                )
                .await;
                if r.status().as_u16() >= 400 && r.status().as_u16() != 404 {
                    failures.push(dp_error(&r));
                }
            }
        }
        if let Some(table) = &self.b.table {
            for t in self.list_tables(table, account, &suffix).await? {
                let path = format!("/Tables('{t}')");
                let r = call(
                    table,
                    Call {
                        method: "DELETE",
                        path: &path,
                        query: "",
                        headers: vec![],
                        body: vec![],
                        label: "table",
                        account,
                        port_suffix: &suffix,
                    },
                )
                .await;
                if r.status().as_u16() >= 400 && r.status().as_u16() != 404 {
                    failures.push(dp_error(&r));
                }
            }
        }
        match failures.into_iter().next() {
            None => Ok(()),
            Some(first) => Err(ArmError::conflict(
                &first.code,
                format!(
                    "The storage account '{account}' cannot be deleted because something in it could not be removed. {}",
                    first.message
                ),
            )),
        }
    }

    async fn put_container(&self, cx: &Cx<'_>, res: &mut Stored, existed: bool) -> ArmResult<()> {
        let props = util::props_mut(&mut res.body);
        util::set_default(props, "publicAccess", json!("None"));
        if props.get("publicAccess").map(Value::is_null).unwrap_or(false) {
            props.insert("publicAccess".into(), json!("None"));
        }
        util::set_default(props, "metadata", json!({}));
        let access = props.get("publicAccess").and_then(Value::as_str).unwrap_or("None").to_string();
        let metadata = props.get("metadata").cloned().unwrap_or(json!({}));
        if !matches!(access.as_str(), "None" | "Blob" | "Container") {
            return Err(ArmError::bad_request("InvalidPublicAccess", format!("The value '{access}' is not valid for publicAccess.")));
        }
        let now = azemu_core::time::iso8601_7(cx.now);
        props.insert("leaseStatus".into(), json!("Unlocked"));
        props.insert("leaseState".into(), json!("Available"));
        props.insert("lastModifiedTime".into(), json!(now));
        util::set_default(props, "hasImmutabilityPolicy", json!(false));
        util::set_default(props, "hasLegalHold", json!(false));
        util::set_default(props, "deleted", json!(false));
        util::set_default(props, "denyEncryptionScopeOverride", json!(false));
        util::set_default(props, "defaultEncryptionScope", json!("$account-encryption-key"));
        util::set_default(props, "remainingRetentionDays", json!(0));
        util::set_default(props, "immutableStorageWithVersioning", json!({"enabled": false}));
        res.body["etag"] = json!(etag::new_etag());
        let Some(blob) = self.b.blob_service() else { return Ok(()) };
        let account = res.name_at(0).to_string();
        let name = res.name().to_string();
        let suffix = cx.port_suffix();
        let path = format!("/{name}");
        let mut headers: Vec<(&str, String)> = Vec::new();
        let meta_headers: Vec<(String, String)> = metadata
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (format!("x-ms-meta-{k}"), v.as_str().unwrap_or("").to_string())).collect())
            .unwrap_or_default();
        for (k, v) in &meta_headers {
            headers.push((k.as_str(), v.clone()));
        }
        if access != "None" {
            headers.push(("x-ms-blob-public-access", access.to_ascii_lowercase()));
        }
        let r = call(
            &blob,
            Call {
                method: "PUT",
                path: &path,
                query: "restype=container",
                headers,
                body: vec![],
                label: "blob",
                account: &account,
                port_suffix: &suffix,
            },
        )
        .await;
        let status = r.status().as_u16();
        if status == 409 {
            // Already there (created through the Blob API, or this is an update): set what ARM carries. A refusal is the answer:
            // "Succeeded" means the Blob service took the metadata and the access level.
            let mut meta_h: Vec<(&str, String)> = Vec::new();
            for (k, v) in &meta_headers {
                meta_h.push((k.as_str(), v.clone()));
            }
            let m = call(
                &blob,
                Call {
                    method: "PUT",
                    path: &path,
                    query: "restype=container&comp=metadata",
                    headers: meta_h,
                    body: vec![],
                    label: "blob",
                    account: &account,
                    port_suffix: &suffix,
                },
            )
            .await;
            if m.status().as_u16() >= 400 {
                return Err(dp_error(&m));
            }
            // no header at all means private ("none" is not a value the Blob service accepts; it answered 400, which used to be ignored)
            let acl_h: Vec<(&str, String)> =
                if access == "None" { vec![] } else { vec![("x-ms-blob-public-access", access.to_ascii_lowercase())] };
            if access != "None" || existed {
                let a = call(
                    &blob,
                    Call {
                        method: "PUT",
                        path: &path,
                        query: "restype=container&comp=acl",
                        headers: acl_h,
                        body: vec![],
                        label: "blob",
                        account: &account,
                        port_suffix: &suffix,
                    },
                )
                .await;
                if a.status().as_u16() >= 400 {
                    return Err(dp_error(&a));
                }
            }
            return Ok(());
        }
        if status >= 400 {
            return Err(dp_error(&r));
        }
        Ok(())
    }

    async fn put_queue(&self, cx: &Cx<'_>, res: &mut Stored) -> ArmResult<()> {
        let props = util::props_mut(&mut res.body);
        util::set_default(props, "metadata", json!({}));
        util::set_default(props, "approximateMessageCount", json!(0));
        let metadata = props.get("metadata").cloned().unwrap_or(json!({}));
        let Some(queue) = &self.b.queue else { return Ok(()) };
        let account = res.name_at(0).to_string();
        let path = format!("/{}", res.name());
        let suffix = cx.port_suffix();
        let meta: Vec<(String, String)> = metadata
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (format!("x-ms-meta-{k}"), v.as_str().unwrap_or("").to_string())).collect())
            .unwrap_or_default();
        let headers: Vec<(&str, String)> = meta.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        let r = call(
            queue,
            Call { method: "PUT", path: &path, query: "", headers, body: vec![], label: "queue", account: &account, port_suffix: &suffix },
        )
        .await;
        let s = r.status().as_u16();
        if s == 409 {
            let headers: Vec<(&str, String)> = meta.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
            let m = call(
                queue,
                Call {
                    method: "PUT",
                    path: &path,
                    query: "comp=metadata",
                    headers,
                    body: vec![],
                    label: "queue",
                    account: &account,
                    port_suffix: &suffix,
                },
            )
            .await;
            if m.status().as_u16() >= 400 {
                return Err(dp_error(&m));
            }
            return Ok(());
        }
        if s >= 400 {
            return Err(dp_error(&r));
        }
        Ok(())
    }

    async fn put_table(&self, cx: &Cx<'_>, res: &mut Stored) -> ArmResult<()> {
        let name = res.name().to_string();
        let props = util::props_mut(&mut res.body);
        props.insert("tableName".into(), json!(name));
        let Some(table) = &self.b.table else { return Ok(()) };
        let account = res.name_at(0).to_string();
        let suffix = cx.port_suffix();
        let r = call(
            table,
            Call {
                method: "POST",
                path: "/Tables",
                query: "",
                headers: vec![
                    ("Content-Type", "application/json".into()),
                    ("Accept", "application/json;odata=nometadata".into()),
                    ("Prefer", "return-no-content".into()),
                ],
                body: json!({"TableName": name}).to_string().into_bytes(),
                label: "table",
                account: &account,
                port_suffix: &suffix,
            },
        )
        .await;
        let s = r.status().as_u16();
        if s >= 400 && s != 409 {
            return Err(dp_error(&r));
        }
        Ok(())
    }

    /// `blobServices/default`: the properties ARM drives. `isVersioningEnabled` switches versioning for the account in
    /// the Blob service, `cors.corsRules`, `deleteRetentionPolicy` and `defaultServiceVersion` become the Blob
    /// service properties (the same as Set Blob Service Properties). The rest (`containerDeleteRetentionPolicy`,
    /// `changeFeed`, `restorePolicy`, `lastAccessTimeTrackingPolicy`, `automaticSnapshotPolicyEnabled`) is stored and
    /// returned; nothing enforces it. A property the request leaves out keeps its value.
    async fn put_blob_service(&self, cx: &Cx<'_>, res: &mut Stored) -> ArmResult<()> {
        if res.name() != "default" {
            return Err(ArmError::bad_request("InvalidResourceName", "The only service name allowed is 'default'."));
        }
        let old = cx
            .inner
            .state
            .lock()
            .resources
            .get(&crate::state::key(&res.id))
            .and_then(|s| s.body.get("properties").and_then(Value::as_object).cloned());
        let props = util::props_mut(&mut res.body);
        for (k, v) in old.unwrap_or_default() {
            if k != "provisioningState" && !props.contains_key(&k) {
                props.insert(k, v);
            }
        }
        util::set_default(props, "isVersioningEnabled", json!(false));
        util::set_default(props, "cors", json!({"corsRules": []}));
        util::set_default(props, "deleteRetentionPolicy", json!({"enabled": false, "allowPermanentDelete": false}));
        let versioning = match props.get("isVersioningEnabled") {
            Some(Value::Bool(b)) => *b,
            _ => return Err(ArmError::bad_request("InvalidRequest", "isVersioningEnabled must be true or false.")),
        };
        let account = res.name_at(0).to_string();
        if let Some(blob) = &self.b.blob {
            blob.set_versioning_enabled(&account, versioning);
        }
        let Some(svc) = self.b.blob_service() else { return Ok(()) };
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><StorageServiceProperties>");
        let props = &res.body["properties"];
        if let Some(rules) = props["cors"]["corsRules"].as_array() {
            if rules.is_empty() {
                xml.push_str("<Cors />");
            } else {
                xml.push_str("<Cors>");
                for r in rules {
                    let list = |k: &str| {
                        r[k].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(",")).unwrap_or_default()
                    };
                    xml.push_str(&format!(
                        "<CorsRule><AllowedOrigins>{}</AllowedOrigins><AllowedMethods>{}</AllowedMethods><AllowedHeaders>{}</AllowedHeaders><ExposedHeaders>{}</ExposedHeaders><MaxAgeInSeconds>{}</MaxAgeInSeconds></CorsRule>",
                        azemu_core::storage::xml_escape(&list("allowedOrigins")),
                        azemu_core::storage::xml_escape(&list("allowedMethods")),
                        azemu_core::storage::xml_escape(&list("allowedHeaders")),
                        azemu_core::storage::xml_escape(&list("exposedHeaders")),
                        r["maxAgeInSeconds"].as_u64().unwrap_or(0),
                    ));
                }
                xml.push_str("</Cors>");
            }
        }
        let d = &props["deleteRetentionPolicy"];
        if d.is_object() {
            let enabled = d["enabled"].as_bool().unwrap_or(false);
            xml.push_str(&format!("<DeleteRetentionPolicy><Enabled>{enabled}</Enabled>"));
            if enabled {
                if let Some(days) = d["days"].as_u64() {
                    xml.push_str(&format!("<Days>{days}</Days>"));
                }
            }
            xml.push_str(&format!(
                "<AllowPermanentDelete>{}</AllowPermanentDelete></DeleteRetentionPolicy>",
                d["allowPermanentDelete"].as_bool().unwrap_or(false)
            ));
        }
        if let Some(v) = props["defaultServiceVersion"].as_str().filter(|v| !v.is_empty()) {
            xml.push_str(&format!("<DefaultServiceVersion>{}</DefaultServiceVersion>", azemu_core::storage::xml_escape(v)));
        }
        xml.push_str("</StorageServiceProperties>");
        let suffix = cx.port_suffix();
        let r = call(
            &svc,
            Call {
                method: "PUT",
                path: "/",
                query: "restype=service&comp=properties",
                headers: vec![("Content-Type", "application/xml".into())],
                body: xml.into_bytes(),
                label: "blob",
                account: &account,
                port_suffix: &suffix,
            },
        )
        .await;
        if r.status().as_u16() >= 400 {
            return Err(dp_error(&r));
        }
        Ok(())
    }

    fn account_defaults(&self, res: &mut Stored, existed: bool, now: &str) {
        let location = res.body.get("location").and_then(Value::as_str).unwrap_or("eastus").to_string();
        if !res.body.get("kind").map(Value::is_string).unwrap_or(false) {
            res.body["kind"] = json!("StorageV2");
        }
        let kind = res.body["kind"].as_str().unwrap_or("StorageV2").to_string();
        let sku_name = res.body.get("sku").and_then(|s| s.get("name")).and_then(Value::as_str).unwrap_or("Standard_LRS").to_string();
        res.body["sku"] = json!({"name": sku_name, "tier": tier_of(&sku_name)});
        let props = util::props_mut(&mut res.body);
        if !existed {
            props.insert("creationTime".into(), json!(now));
            props.insert("keyCreationTime".into(), json!({"key1": now, "key2": now}));
        }
        util::set_default(props, "primaryLocation", json!(location));
        util::set_default(props, "statusOfPrimary", json!("available"));
        util::set_default(props, "supportsHttpsTrafficOnly", json!(true));
        util::set_default(props, "minimumTlsVersion", json!("TLS1_2"));
        util::set_default(props, "allowBlobPublicAccess", json!(false));
        util::set_default(props, "allowSharedKeyAccess", json!(true));
        util::set_default(props, "defaultToOAuthAuthentication", json!(false));
        util::set_default(props, "isHnsEnabled", json!(false));
        util::set_default(props, "publicNetworkAccess", json!("Enabled"));
        if matches!(kind.as_str(), "StorageV2" | "BlobStorage") {
            util::set_default(props, "accessTier", json!("Hot"));
        }
        util::set_default(
            props,
            "networkAcls",
            json!({"bypass": "AzureServices", "virtualNetworkRules": [], "ipRules": [], "defaultAction": "Allow"}),
        );
        util::set_default(
            props,
            "encryption",
            json!({
                "services": {
                    "file": {"keyType": "Account", "enabled": true, "lastEnabledTime": now},
                    "blob": {"keyType": "Account", "enabled": true, "lastEnabledTime": now}
                },
                "keySource": "Microsoft.Storage",
                "requireInfrastructureEncryption": false
            }),
        );
    }
}

#[async_trait]
impl Provider for Storage {
    fn namespace(&self) -> &'static str {
        "Microsoft.Storage"
    }

    fn types(&self) -> &'static [&'static str] {
        TYPES
    }

    fn passthrough(&self) -> &'static [&'static str] {
        PASSTHROUGH
    }

    fn statuses(&self, type_path: &str) -> Statuses {
        match type_path {
            "storageAccounts" => Statuses { create: 200, update: 200, delete: 200, delete_missing: 204 },
            "storageAccounts/blobServices/containers" => Statuses { create: 201, update: 200, delete: 200, delete_missing: 204 },
            "storageAccounts/queueServices/queues" | "storageAccounts/tableServices/tables" => {
                Statuses { create: 200, update: 200, delete: 204, delete_missing: 204 }
            }
            _ => Statuses { create: 200, update: 200, delete: 200, delete_missing: 204 },
        }
    }

    fn validate_name(&self, type_path: &str, name: &str) -> ArmResult<()> {
        if type_path == "storageAccounts" && !valid_account_name(name) {
            return Err(ArmError::bad_request(
                "AccountNameInvalid",
                format!("{name} is not a valid storage account name. Storage account name must be between 3 and 24 characters in length and use numbers and lower-case letters only."),
            ));
        }
        Ok(())
    }

    async fn put(&self, cx: &Cx<'_>, res: &mut Stored, existed: bool, children: &mut Vec<Stored>) -> ArmResult<()> {
        match res.type_path().as_str() {
            "storageAccounts" => {
                let now = cx.now_iso();
                self.account_defaults(res, existed, &now);
                // `allowBlobPublicAccess` (false unless the request says true, as on Azure) decides whether the Blob service
                // lets containers of this account be public.
                if let Some(blob) = &self.b.blob {
                    let allowed = res.body["properties"]["allowBlobPublicAccess"].as_bool().unwrap_or(false);
                    blob.set_public_access_allowed(res.name(), allowed);
                }
                if !existed {
                    // On Azure versioning is off until it is turned on. An account made through ARM starts that way
                    // (the Blob service's own default, on, stays for accounts ARM never made).
                    if let Some(blob) = &self.b.blob {
                        blob.set_versioning_enabled(res.name(), false);
                    }
                    let sku = res.body["sku"].clone();
                    for (ty, props) in [
                        (
                            "blobServices",
                            json!({
                                "cors": {"corsRules": []},
                                "deleteRetentionPolicy": {"allowPermanentDelete": false, "enabled": false},
                                "containerDeleteRetentionPolicy": {"allowPermanentDelete": false, "enabled": true, "days": 7},
                                "isVersioningEnabled": false,
                            }),
                        ),
                        ("queueServices", json!({"cors": {"corsRules": []}})),
                        ("tableServices", json!({"cors": {"corsRules": []}})),
                        (
                            "fileServices",
                            json!({"protocolSettings": {"smb": {}}, "cors": {"corsRules": []}, "shareDeleteRetentionPolicy": {"enabled": true, "days": 7}}),
                        ),
                    ] {
                        children.push(stored_child(res, ty, "default", json!({"sku": sku, "properties": props})));
                    }
                }
                Ok(())
            }
            "storageAccounts/blobServices/containers" => self.put_container(cx, res, existed).await,
            "storageAccounts/queueServices/queues" => self.put_queue(cx, res).await,
            "storageAccounts/tableServices/tables" => self.put_table(cx, res).await,
            "storageAccounts/blobServices" => self.put_blob_service(cx, res).await,
            "storageAccounts/queueServices" | "storageAccounts/tableServices" => {
                if res.name() != "default" {
                    return Err(ArmError::bad_request("InvalidResourceName", "The only service name allowed is 'default'."));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn delete(&self, cx: &Cx<'_>, res: &Stored, force: bool) -> ArmResult<()> {
        let suffix = cx.port_suffix();
        let failed = |r: &Response| {
            let s = r.status().as_u16();
            s >= 400 && s != 404
        };
        match res.type_path().as_str() {
            "storageAccounts" => self.wipe_account(cx, res.name()).await?,
            "storageAccounts/blobServices/containers" => {
                if let Some(blob) = self.b.blob_service() {
                    self.delete_container(&blob, res.name_at(0), res.name(), &suffix, force).await?;
                }
            }
            "storageAccounts/queueServices/queues" => {
                if let Some(q) = &self.b.queue {
                    let path = format!("/{}", res.name());
                    let r = call(
                        q,
                        Call {
                            method: "DELETE",
                            path: &path,
                            query: "",
                            headers: vec![],
                            body: vec![],
                            label: "queue",
                            account: res.name_at(0),
                            port_suffix: &suffix,
                        },
                    )
                    .await;
                    if failed(&r) {
                        return Err(dp_error(&r));
                    }
                }
            }
            "storageAccounts/tableServices/tables" => {
                if let Some(t) = &self.b.table {
                    let path = format!("/Tables('{}')", res.name());
                    let r = call(
                        t,
                        Call {
                            method: "DELETE",
                            path: &path,
                            query: "",
                            headers: vec![],
                            body: vec![],
                            label: "table",
                            account: res.name_at(0),
                            port_suffix: &suffix,
                        },
                    )
                    .await;
                    if failed(&r) {
                        return Err(dp_error(&r));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn render(&self, cx: &Cx<'_>, res: &Stored) -> Value {
        let mut v = res.body.clone();
        if res.type_path() == "storageAccounts/blobServices" {
            if let Some(blob) = &self.b.blob {
                util::props_mut(&mut v).insert("isVersioningEnabled".into(), json!(blob.versioning_enabled(res.name_at(0))));
            }
        }
        if res.type_path() == "storageAccounts" {
            let n = res.name();
            let p = util::props_mut(&mut v);
            p.insert(
                "primaryEndpoints".into(),
                json!({
                    "blob": format!("{}/", cx.storage_url("http", n, "blob")),
                    "queue": format!("{}/", cx.storage_url("http", n, "queue")),
                    "table": format!("{}/", cx.storage_url("http", n, "table")),
                    "dfs": format!("{}/", cx.storage_url("http", n, "dfs")),
                }),
            );
        }
        v
    }

    async fn action(
        &self,
        cx: &Cx<'_>,
        target: ActionTarget<'_>,
        type_path: &str,
        action: &str,
        body: &Value,
    ) -> ArmResult<Option<Response>> {
        let a = action.to_ascii_lowercase();
        match (target, type_path, a.as_str()) {
            (ActionTarget::Provider, _, "checknameavailability") => {
                let name = body.get("name").and_then(Value::as_str).unwrap_or("");
                let ty = body.get("type").and_then(Value::as_str).unwrap_or("Microsoft.Storage/storageAccounts");
                if !ty.eq_ignore_ascii_case("Microsoft.Storage/storageAccounts") {
                    return Err(ArmError::bad_request("InvalidResourceType", format!("The type '{ty}' is not valid.")));
                }
                let v = if !valid_account_name(name) {
                    util::name_availability(
                        false,
                        "AccountNameInvalid",
                        &format!("{name} is not a valid storage account name. Storage account name must be between 3 and 24 characters in length and use numbers and lower-case letters only."),
                    )
                } else if cx
                    .inner
                    .state
                    .lock()
                    .resources
                    .values()
                    .any(|r| r.ns == "Microsoft.Storage" && r.types.len() == 1 && r.name().eq_ignore_ascii_case(name))
                {
                    util::name_availability(
                        false,
                        "AlreadyExists",
                        "The storage account named {name} is already taken.".replace("{name}", name).as_str(),
                    )
                } else {
                    util::name_availability(true, "", "")
                };
                Ok(Some(util::json_ok(200, &v)))
            }
            (ActionTarget::Resource(res), "storageAccounts", "listkeys") => Ok(Some(self.keys_response(res))),
            (ActionTarget::Resource(res), "storageAccounts", "regeneratekey") => {
                let which = body.get("keyName").and_then(Value::as_str).unwrap_or("");
                let which = which.to_ascii_lowercase();
                if which != "key1" && which != "key2" && which != "kerb1" && which != "kerb2" {
                    return Err(ArmError::bad_request("InvalidKeyName", "The key name must be key1 or key2."));
                }
                bump_generation(cx, &res.id, &which);
                let fresh = cx.inner.state.lock().resources.get(&crate::state::key(&res.id)).cloned().unwrap_or_else(|| res.clone());
                Ok(Some(self.keys_response(&fresh)))
            }
            _ => Ok(None),
        }
    }
}
