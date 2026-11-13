//! Microsoft.DocumentDB: database accounts, SQL databases and SQL containers.
//! Databases and containers are created through the Cosmos emulator's own HTTP API.

use azemu_core::{async_trait, Response};
use serde_json::{json, Value};

use super::{bump_generation, dp_error, generation, stored_child};
use crate::backends::{call, Backends, Call};
use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::provider::{ActionTarget, Provider, Statuses};
use crate::state::Stored;
use crate::util;

pub struct Cosmos {
    pub b: Backends,
}

const TYPES: &[&str] = &[
    "databaseAccounts",
    "databaseAccounts/sqlDatabases",
    "databaseAccounts/sqlDatabases/containers",
    "databaseAccounts/sqlDatabases/throughputSettings",
    "databaseAccounts/sqlDatabases/containers/throughputSettings",
];

fn valid_account_name(n: &str) -> bool {
    let b = n.as_bytes();
    (3..=44).contains(&b.len())
        && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

const KEY_KINDS: &[(&str, &str)] = &[
    ("primaryMasterKey", "primary"),
    ("secondaryMasterKey", "secondary"),
    ("primaryReadonlyMasterKey", "primaryReadonly"),
    ("secondaryReadonlyMasterKey", "secondaryReadonly"),
];

impl Cosmos {
    fn key_value(&self, res: &Stored, kind: &str) -> String {
        util::derive_key64(&format!("cosmos/{}/{kind}", res.id.to_ascii_lowercase()), generation(res, kind))
    }

    fn account_defaults(&self, res: &mut Stored, existed: bool, now: &str, endpoint_hint: &str) {
        let location = res.body.get("location").and_then(Value::as_str).unwrap_or("eastus").to_string();
        if !res.body.get("kind").map(Value::is_string).unwrap_or(false) {
            res.body["kind"] = json!("GlobalDocumentDB");
        }
        let name = res.name().to_string();
        let p = util::props_mut(&mut res.body);
        util::set_default(p, "databaseAccountOfferType", json!("Standard"));
        util::set_default(
            p,
            "consistencyPolicy",
            json!({"defaultConsistencyLevel": "Session", "maxIntervalInSeconds": 5, "maxStalenessPrefix": 100}),
        );
        let locs = p.get("locations").and_then(Value::as_array).cloned().unwrap_or_default();
        let loc_name = locs.first().and_then(|l| l.get("locationName")).and_then(Value::as_str).unwrap_or(&location).to_string();
        let loc = json!({
            "id": format!("{name}-{}", loc_name.replace(' ', "").to_ascii_lowercase()),
            "locationName": loc_name,
            "documentEndpoint": endpoint_hint,
            "provisioningState": "Succeeded",
            "failoverPriority": 0,
            "isZoneRedundant": false
        });
        // The azurerm provider builds `geo_location` from the failover policies.
        p.insert("failoverPolicies".into(), json!([{"id": loc["id"], "locationName": loc_name, "failoverPriority": 0}]));
        p.insert("locations".into(), json!([loc.clone()]));
        p.insert("writeLocations".into(), json!([loc.clone()]));
        p.insert("readLocations".into(), json!([loc]));
        util::set_default(p, "enableAutomaticFailover", json!(false));
        util::set_default(p, "enableMultipleWriteLocations", json!(false));
        util::set_default(p, "capabilities", json!([]));
        util::set_default(p, "ipRules", json!([]));
        util::set_default(p, "isVirtualNetworkFilterEnabled", json!(false));
        util::set_default(p, "disableKeyBasedMetadataWriteAccess", json!(false));
        util::set_default(p, "publicNetworkAccess", json!("Enabled"));
        util::set_default(p, "enableFreeTier", json!(false));
        util::set_default(p, "createMode", json!("Default"));
        // What Azure returns for an account that does not set them. Nothing enforces these here.
        util::set_default(p, "enableAnalyticalStorage", json!(false));
        util::set_default(p, "enablePartitionMerge", json!(false));
        util::set_default(p, "enableBurstCapacity", json!(false));
        util::set_default(p, "minimalTlsVersion", json!("Tls12"));
        util::set_default(p, "networkAclBypass", json!("None"));
        util::set_default(p, "networkAclBypassResourceIds", json!([]));
        util::set_default(p, "virtualNetworkRules", json!([]));
        util::set_default(p, "disableLocalAuth", json!(false));
        util::set_default(p, "defaultIdentity", json!("FirstPartyIdentity"));
        util::set_default(p, "cors", json!([]));
        util::set_default(
            p,
            "backupPolicy",
            json!({"type": "Periodic", "periodicModeProperties": {"backupIntervalInMinutes": 240, "backupRetentionIntervalInHours": 8, "backupStorageRedundancy": "Geo"}}),
        );
        if !existed {
            p.insert("instanceId".into(), json!(uuid::Uuid::new_v4().to_string()));
            p.insert("createdAt".into(), json!(now));
        }
    }

    async fn dp(
        &self,
        cx: &Cx<'_>,
        method: &str,
        account: &str,
        path: &str,
        headers: Vec<(&str, String)>,
        body: Vec<u8>,
    ) -> Option<Response> {
        let svc = self.b.cosmos.as_ref()?;
        let suffix = cx.port_suffix();
        let mut h = vec![("x-ms-version", "2018-12-31".to_string()), ("Content-Type", "application/json".to_string())];
        h.extend(headers);
        Some(call(svc, Call { method, path, query: "", headers: h, body, label: "documents", account, port_suffix: &suffix }).await)
    }

    fn throughput_headers(options: &Value) -> Vec<(&'static str, String)> {
        let mut h = Vec::new();
        if let Some(t) = options.get("throughput").and_then(Value::as_u64) {
            h.push(("x-ms-offer-throughput", t.to_string()));
        }
        if let Some(m) = options.get("autoscaleSettings").and_then(|a| a.get("maxThroughput")).and_then(Value::as_u64) {
            h.push(("x-ms-cosmos-offer-autopilot-settings", json!({"maxThroughput": m}).to_string()));
        }
        h
    }

    fn throughput_child(&self, parent: &Stored, options: &Value) -> Option<Stored> {
        let resource = if let Some(t) = options.get("throughput").and_then(Value::as_u64) {
            json!({"throughput": t, "minimumThroughput": "400", "offerReplacePending": "false"})
        } else {
            let m = options.get("autoscaleSettings").and_then(|a| a.get("maxThroughput")).and_then(Value::as_u64)?;
            json!({"autoscaleSettings": {"maxThroughput": m}, "minimumThroughput": "1000", "offerReplacePending": "false"})
        };
        Some(stored_child(parent, "throughputSettings", "default", json!({"properties": {"resource": resource}})))
    }

    async fn put_database(&self, cx: &Cx<'_>, res: &mut Stored, children: &mut Vec<Stored>) -> ArmResult<()> {
        let account = res.name_at(0).to_string();
        let name = res.name().to_string();
        let options = res.body.get("properties").and_then(|p| p.get("options")).cloned().unwrap_or(json!({}));
        let mut resource = json!({"id": name});
        if self.b.cosmos.is_some() {
            let r = self
                .dp(cx, "POST", &account, "/dbs", Self::throughput_headers(&options), json!({"id": name}).to_string().into_bytes())
                .await;
            if let Some(r) = r {
                let s = r.status().as_u16();
                if s == 409 {
                    if let Some(g) = self.dp(cx, "GET", &account, &format!("/dbs/{name}"), vec![], vec![]).await {
                        if let Ok(v) = serde_json::from_slice::<Value>(g.body()) {
                            resource = pick(&v, &["id", "_rid", "_self", "_etag", "_colls", "_users", "_ts"]);
                        }
                    }
                } else if s >= 400 {
                    return Err(dp_error(&r));
                } else if let Ok(v) = serde_json::from_slice::<Value>(r.body()) {
                    resource = pick(&v, &["id", "_rid", "_self", "_etag", "_colls", "_users", "_ts"]);
                }
            }
        }
        let p = util::props_mut(&mut res.body);
        p.insert("resource".into(), resource);
        if let Some(c) = self.throughput_child(res, &options) {
            children.push(c);
        }
        let p = util::props_mut(&mut res.body);
        p.remove("options");
        Ok(())
    }

    async fn put_container(&self, cx: &Cx<'_>, res: &mut Stored, children: &mut Vec<Stored>) -> ArmResult<()> {
        let account = res.name_at(0).to_string();
        let db = res.name_at(1).to_string();
        let name = res.name().to_string();
        let props = res.body.get("properties").cloned().unwrap_or(json!({}));
        let options = props.get("options").cloned().unwrap_or(json!({}));
        let mut doc = props.get("resource").cloned().unwrap_or(json!({}));
        doc["id"] = json!(name);
        if doc.get("partitionKey").is_none() {
            doc["partitionKey"] = json!({"paths": ["/id"], "kind": "Hash"});
        }
        let mut resource = doc.clone();
        if self.b.cosmos.is_some() {
            let path = format!("/dbs/{db}/colls");
            let r = self.dp(cx, "POST", &account, &path, Self::throughput_headers(&options), doc.to_string().into_bytes()).await;
            if let Some(r) = r {
                let s = r.status().as_u16();
                if s == 409 {
                    let one = format!("{path}/{name}");
                    // Replace what can change, then read back.
                    let rep = self.dp(cx, "PUT", &account, &one, vec![], doc.to_string().into_bytes()).await;
                    if let Some(g) = rep {
                        // the container is only "Succeeded" when the data plane took the settings (a changed partition key is refused)
                        if g.status().as_u16() >= 400 {
                            return Err(dp_error(&g));
                        }
                        if let Ok(v) = serde_json::from_slice::<Value>(g.body()) {
                            resource = strip_internal(&v);
                        }
                    }
                } else if s >= 400 {
                    return Err(dp_error(&r));
                } else if let Ok(v) = serde_json::from_slice::<Value>(r.body()) {
                    resource = strip_internal(&v);
                }
            }
        }
        let p = util::props_mut(&mut res.body);
        p.insert("resource".into(), resource);
        p.remove("options");
        if let Some(c) = self.throughput_child(res, &options) {
            children.push(c);
        }
        Ok(())
    }
}

fn pick(v: &Value, keys: &[&str]) -> Value {
    let mut o = serde_json::Map::new();
    for k in keys {
        if let Some(x) = v.get(*k) {
            o.insert((*k).to_string(), x.clone());
        }
    }
    Value::Object(o)
}

/// Container documents from the data plane carry paths to other resources; ARM shows
/// the settings plus `_rid`, `_ts` and `_etag`.
fn strip_internal(v: &Value) -> Value {
    let mut o = v.as_object().cloned().unwrap_or_default();
    for k in ["_self", "_docs", "_sprocs", "_triggers", "_udfs", "_conflicts"] {
        o.remove(k);
    }
    Value::Object(o)
}

#[async_trait]
impl Provider for Cosmos {
    fn namespace(&self) -> &'static str {
        "Microsoft.DocumentDB"
    }

    fn types(&self) -> &'static [&'static str] {
        TYPES
    }

    fn get_actions(&self) -> &'static [&'static str] {
        &["readonlykeys"]
    }

    fn statuses(&self, type_path: &str) -> Statuses {
        match type_path {
            "databaseAccounts" => Statuses { create: 200, update: 200, delete: 204, delete_missing: 204 },
            "databaseAccounts/sqlDatabases" | "databaseAccounts/sqlDatabases/containers" => {
                Statuses { create: 200, update: 200, delete: 204, delete_missing: 204 }
            }
            _ => Statuses { create: 200, update: 200, delete: 204, delete_missing: 204 },
        }
    }

    fn global_name(&self, type_path: &str) -> bool {
        type_path == "databaseAccounts"
    }

    fn requires_location(&self, type_path: &str) -> bool {
        type_path == "databaseAccounts"
    }

    fn validate_name(&self, type_path: &str, name: &str) -> ArmResult<()> {
        if type_path == "databaseAccounts" && !valid_account_name(name) {
            return Err(ArmError::bad_request(
                "BadRequest",
                format!("Database account name '{name}' is invalid. Use 3 to 44 lowercase letters, numbers and hyphens."),
            ));
        }
        Ok(())
    }

    async fn put(&self, cx: &Cx<'_>, res: &mut Stored, existed: bool, children: &mut Vec<Stored>) -> ArmResult<()> {
        match res.type_path().as_str() {
            "databaseAccounts" => {
                let now = cx.now_iso();
                // the endpoint is not stored (it holds the port): `render` fills it in
                self.account_defaults(res, existed, &now, "");
                Ok(())
            }
            "databaseAccounts/sqlDatabases" => self.put_database(cx, res, children).await,
            "databaseAccounts/sqlDatabases/containers" => self.put_container(cx, res, children).await,
            "databaseAccounts/sqlDatabases/throughputSettings" | "databaseAccounts/sqlDatabases/containers/throughputSettings" => {
                if res.name() != "default" {
                    return Err(ArmError::bad_request("BadRequest", "The only throughput setting name is 'default'."));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn delete(&self, cx: &Cx<'_>, res: &Stored, _force: bool) -> ArmResult<()> {
        let account = res.name_at(0).to_string();
        // a 404 is success (already gone); any other failure is the answer
        let check = |r: Option<Response>| -> ArmResult<()> {
            match r {
                Some(r) if r.status().as_u16() >= 400 && r.status().as_u16() != 404 => Err(dp_error(&r)),
                _ => Ok(()),
            }
        };
        match res.type_path().as_str() {
            "databaseAccounts" => {
                // every page of the list of databases, then each database
                let mut ids: Vec<String> = Vec::new();
                let mut continuation = String::new();
                for _ in 0..10_000 {
                    let headers = if continuation.is_empty() { vec![] } else { vec![("x-ms-continuation", continuation.clone())] };
                    let Some(r) = self.dp(cx, "GET", &account, "/dbs", headers, vec![]).await else { return Ok(()) };
                    if r.status().as_u16() >= 400 {
                        return Err(dp_error(&r));
                    }
                    ids.extend(
                        serde_json::from_slice::<Value>(r.body())
                            .ok()
                            .and_then(|v| {
                                v["Databases"]
                                    .as_array()
                                    .map(|a| a.iter().filter_map(|d| d["id"].as_str().map(String::from)).collect::<Vec<_>>())
                            })
                            .unwrap_or_default(),
                    );
                    match r.headers().get("x-ms-continuation").and_then(|v| v.to_str().ok()).filter(|c| !c.is_empty() && *c != continuation)
                    {
                        Some(c) => continuation = c.to_string(),
                        None => break,
                    }
                }
                let mut first: Option<ArmError> = None;
                for id in ids {
                    if let Err(e) = check(self.dp(cx, "DELETE", &account, &format!("/dbs/{id}"), vec![], vec![]).await) {
                        first.get_or_insert(e);
                    }
                }
                if let Some(e) = first {
                    return Err(e);
                }
            }
            "databaseAccounts/sqlDatabases" => {
                check(self.dp(cx, "DELETE", &account, &format!("/dbs/{}", res.name()), vec![], vec![]).await)?
            }
            "databaseAccounts/sqlDatabases/containers" => {
                check(self.dp(cx, "DELETE", &account, &format!("/dbs/{}/colls/{}", res.name_at(1), res.name()), vec![], vec![]).await)?
            }
            _ => {}
        }
        Ok(())
    }

    fn render(&self, cx: &Cx<'_>, res: &Stored) -> Value {
        let mut v = res.body.clone();
        if res.type_path() == "databaseAccounts" {
            let endpoint = format!("{}/", cx.data_url("https", res.name(), "documents"));
            let p = util::props_mut(&mut v);
            for list in ["locations", "writeLocations", "readLocations"] {
                if let Some(Value::Array(a)) = p.get_mut(list) {
                    for l in a.iter_mut().filter_map(Value::as_object_mut) {
                        l.insert("documentEndpoint".into(), json!(endpoint));
                    }
                }
            }
            p.insert("documentEndpoint".into(), json!(endpoint));
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
        let ActionTarget::Resource(res) = target else { return Ok(None) };
        if type_path != "databaseAccounts" {
            return Ok(None);
        }
        match action.to_ascii_lowercase().as_str() {
            "listkeys" => {
                let mut o = serde_json::Map::new();
                for (field, kind) in KEY_KINDS {
                    o.insert((*field).to_string(), json!(self.key_value(res, kind)));
                }
                Ok(Some(util::json_ok(200, &Value::Object(o))))
            }
            "readonlykeys" => {
                let mut o = serde_json::Map::new();
                for (field, kind) in KEY_KINDS.iter().filter(|(f, _)| f.contains("Readonly")) {
                    o.insert((*field).to_string(), json!(self.key_value(res, kind)));
                }
                Ok(Some(util::json_ok(200, &Value::Object(o))))
            }
            "listconnectionstrings" => {
                let endpoint = cx.data_url("https", res.name(), "documents");
                let cs = |kind: &str, desc: &str, key_kind: &str| {
                    json!({
                        "connectionString": format!("AccountEndpoint={endpoint}/;AccountKey={};", self.key_value(res, kind)),
                        "description": desc,
                        "keyKind": key_kind,
                        "type": if kind.contains("Readonly") { "SqlReadOnly" } else { "Sql" },
                    })
                };
                Ok(Some(util::json_ok(
                    200,
                    &json!({"connectionStrings": [
                        cs("primary", "Primary SQL Connection String", "Primary"),
                        cs("secondary", "Secondary SQL Connection String", "Secondary"),
                        cs("primaryReadonly", "Primary Read-Only SQL Connection String", "PrimaryReadonly"),
                        cs("secondaryReadonly", "Secondary Read-Only SQL Connection String", "SecondaryReadonly"),
                    ]}),
                )))
            }
            "regeneratekey" => {
                let kind = body.get("keyKind").and_then(Value::as_str).unwrap_or("");
                let Some((_, k)) = KEY_KINDS.iter().find(|(_, k)| k.eq_ignore_ascii_case(kind)) else {
                    return Err(ArmError::bad_request(
                        "BadRequest",
                        "keyKind must be primary, secondary, primaryReadonly or secondaryReadonly.",
                    ));
                };
                bump_generation(cx, &res.id, k);
                Ok(Some(util::empty(200)))
            }
            _ => Ok(None),
        }
    }
}
