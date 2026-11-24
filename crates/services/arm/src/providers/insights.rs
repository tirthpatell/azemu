//! `Microsoft.Insights/components` (Application Insights) and `Microsoft.OperationalInsights/workspaces` (Log Analytics).
//!
//! A component has the keys a program needs to send telemetry: an instrumentation key, an application id and a connection string
//! whose endpoints are the emulator's own monitor service (`docs/services/monitor.md`). Telemetry sent with that string can be read back
//! through `/_azemu/monitor/telemetry?ikey=...`. A workspace is stored with the fields Terraform and the SDKs read, and hands out
//! shared keys; nothing is ingested into it.
//!
//! The shapes come from recorded answers of a real component and workspace (November 2026): the property names of a component start
//! with capitals, and `type` is written in lower case (`microsoft.insights/components`).

use azemu_core::{async_trait, Response};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::generation;
use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::provider::{ActionTarget, Provider, Statuses};
use crate::state::Stored;
use crate::util;

/// A GUID that is the same for the same seed (a version 4 GUID: the version and variant bits are set, the rest comes from a hash).
fn guid_from(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(b).to_string()
}

/// Azure writes the creation date of a component with six digits and a numeric zone (`2026-11-24T15:31:12.730897+00:00`).
fn creation_date(t: azemu_core::time::OffsetDateTime) -> String {
    let s = azemu_core::time::iso8601_7(t);
    // `...:12.7308970Z`: drop the seventh digit and the `Z`
    format!("{}+00:00", &s[..s.len().saturating_sub(2)])
}

/// Fill in a field of a JSON object that is missing or null.
fn set_field(obj: &mut Value, key: &str, default: Value) {
    if obj.get(key).map(Value::is_null).unwrap_or(true) {
        obj[key] = default;
    }
}

/// The next 22:00 UTC after `now`: when the daily quota of a workspace starts again (as in the recorded answer).
fn next_quota_reset(now: azemu_core::time::OffsetDateTime) -> String {
    let at = azemu_core::time::Time::from_hms(22, 0, 0).unwrap_or(azemu_core::time::Time::MIDNIGHT);
    let mut t = now.to_offset(azemu_core::time::UtcOffset::UTC).replace_time(at);
    if t <= now {
        t += azemu_core::time::Duration::days(1);
    }
    azemu_core::time::iso8601_secs(t)
}

fn new_etag() -> String {
    format!("\"{}\"", uuid::Uuid::new_v4())
}

/// The instrumentation key of a component: the same for the same resource id, also after a restart.
fn instrumentation_key(res: &Stored) -> String {
    guid_from(&format!("azemu-arm-key/appinsights/{}/ikey", res.id.to_ascii_lowercase()))
}

pub struct Insights;

impl Insights {
    fn put_component(&self, res: &mut Stored) -> ArmResult<()> {
        let ikey = instrumentation_key(res);
        let name = res.name().to_string();
        let created = creation_date(res.created);
        let tenant = res.sub.clone();
        if !res.body.get("kind").map(Value::is_string).unwrap_or(false) {
            res.body["kind"] = json!("web");
        }
        if !res.body.get("tags").map(Value::is_object).unwrap_or(false) {
            res.body["tags"] = json!({});
        }
        res.body["etag"] = json!(new_etag());
        let p = util::props_mut(&mut res.body);
        let has_workspace = p.get("WorkspaceResourceId").and_then(Value::as_str).is_some_and(|w| !w.is_empty());
        for (k, v) in [("Flow_Type", Value::Null), ("Request_Source", Value::Null)] {
            util::set_default(p, k, v);
        }
        util::set_default(p, "Application_Type", json!("web"));
        util::set_default(p, "DisableIpMasking", json!(false));
        util::set_default(p, "DisableLocalAuth", json!(false));
        util::set_default(p, "ForceCustomerStorageForProfiler", json!(false));
        util::set_default(p, "IngestionMode", json!(if has_workspace { "LogAnalytics" } else { "ApplicationInsights" }));
        util::set_default(p, "RetentionInDays", json!(90));
        util::set_default(p, "SamplingPercentage", json!(100.0));
        util::set_default(p, "publicNetworkAccessForIngestion", json!("Enabled"));
        util::set_default(p, "publicNetworkAccessForQuery", json!("Enabled"));
        let days = p.get("RetentionInDays").and_then(Value::as_u64).unwrap_or(90);
        if !(30..=730).contains(&days) {
            return Err(ArmError::bad_request(
                "InvalidRetentionInDays",
                format!("RetentionInDays {days} is not valid: it must be between 30 and 730."),
            ));
        }
        p.insert("Retention".into(), json!(format!("P{days}D")));
        p.insert("Name".into(), json!(name));
        p.insert("ApplicationId".into(), json!(name));
        p.insert("InstrumentationKey".into(), json!(ikey));
        p.insert("AppId".into(), json!(azemu_monitor::app_id(&ikey)));
        p.insert("CreationDate".into(), json!(created));
        p.insert("TenantId".into(), json!(tenant));
        p.insert("Ver".into(), json!("v2"));
        Ok(())
    }

    /// What the program sends telemetry to: the emulator's monitor service, written the way the other providers write the address of
    /// a service (`serviceBusEndpoint`, `primaryEndpoints`). The endpoints take plain `http`: no token goes with an instrumentation key.
    fn connection_string(cx: &Cx<'_>, res: &Stored) -> String {
        let p = &res.body["properties"];
        let location = res.body.get("location").and_then(Value::as_str).filter(|l| !l.is_empty()).unwrap_or("eastus");
        let ingestion = cx.data_url("http", location, "in");
        let live = cx.data_url("http", location, "livediagnostics");
        format!(
            "InstrumentationKey={};IngestionEndpoint={ingestion}/;LiveEndpoint={live}/;ApplicationId={}",
            p["InstrumentationKey"].as_str().unwrap_or(""),
            p["AppId"].as_str().unwrap_or("")
        )
    }
}

#[async_trait]
impl Provider for Insights {
    fn namespace(&self) -> &'static str {
        "Microsoft.Insights"
    }

    fn types(&self) -> &'static [&'static str] {
        &["components"]
    }

    /// Alert rules, action groups, diagnostic settings, web tests and the children of a component are kept as JSON, as before.
    fn other_types_stored(&self) -> bool {
        true
    }

    /// The billing features of a component (the daily data cap): read and written on `components/{c}/currentbillingfeatures`.
    fn get_actions(&self) -> &'static [&'static str] {
        &["currentbillingfeatures"]
    }

    fn put_actions(&self) -> &'static [&'static str] {
        &["currentbillingfeatures"]
    }

    fn statuses(&self, _type_path: &str) -> Statuses {
        Statuses { create: 200, update: 200, delete: 200, delete_missing: 204 }
    }

    /// A component's name is unique in its resource group only.
    fn global_name(&self, _type_path: &str) -> bool {
        false
    }

    fn validate_name(&self, _type_path: &str, name: &str) -> ArmResult<()> {
        if name.is_empty()
            || name.len() > 260
            || name.ends_with('.')
            || name.ends_with(' ')
            || name.contains(['<', '>', '%', '&', ':', '\\', '?', '/'])
        {
            return Err(ArmError::bad_request(
                "InvalidResourceName",
                format!("The resource name '{name}' is invalid. Use 1 to 260 characters, none of < > % & : \\ ? /, and no dot or space at the end."),
            ));
        }
        Ok(())
    }

    async fn put(&self, _cx: &Cx<'_>, res: &mut Stored, _existed: bool, _children: &mut Vec<Stored>) -> ArmResult<()> {
        self.put_component(res)
    }

    async fn delete(&self, _cx: &Cx<'_>, _res: &Stored, _force: bool) -> ArmResult<()> {
        Ok(())
    }

    fn render(&self, cx: &Cx<'_>, res: &Stored) -> Value {
        let mut v = res.body.clone();
        // Azure writes the namespace of this type in lower case
        v["type"] = json!("microsoft.insights/components");
        if let Some(id) = v["id"].as_str() {
            v["id"] = json!(id.replacen("/providers/Microsoft.Insights/", "/providers/microsoft.insights/", 1));
        }
        let cs = Self::connection_string(cx, res);
        util::props_mut(&mut v).insert("ConnectionString".into(), json!(cs));
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
        if type_path != "components" || !action.eq_ignore_ascii_case("currentbillingfeatures") {
            return Ok(None);
        }
        // The daily data cap. Written by `azurerm_application_insights` on every create, read back on every refresh. Kept as it was
        // written; nothing is limited. The shape is that of the 2015-05-01 API (`ComponentBillingFeatures`), not a recorded answer.
        let mut st = cx.inner.state.lock();
        let Some(stored) = st.resources.get_mut(&crate::state::key(&res.id)) else { return Ok(None) };
        if !stored.extra.is_object() {
            stored.extra = json!({});
        }
        if cx.req.method.as_str() == "PUT" {
            let mut features = body.clone();
            set_field(&mut features, "CurrentBillingFeatures", json!(["Basic"]));
            let mut cap = features.get("DataVolumeCap").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
            for (k, v) in [
                ("Cap", json!(100.0)),
                ("ResetTime", json!(0)),
                ("WarningThreshold", json!(90)),
                ("StopSendNotificationWhenHitThreshold", json!(false)),
                ("StopSendNotificationWhenHitCap", json!(false)),
                ("MaxHistoryCap", json!(500.0)),
            ] {
                set_field(&mut cap, k, v);
            }
            features["DataVolumeCap"] = cap;
            stored.extra["billing"] = features;
        }
        let features = stored.extra.get("billing").cloned().unwrap_or_else(|| {
            json!({
                "CurrentBillingFeatures": ["Basic"],
                "DataVolumeCap": {"Cap": 100.0, "ResetTime": 0, "WarningThreshold": 90, "StopSendNotificationWhenHitThreshold": false,
                                  "StopSendNotificationWhenHitCap": false, "MaxHistoryCap": 500.0}
            })
        });
        Ok(Some(util::json_ok(200, &features)))
    }
}

pub struct OperationalInsights;

/// Azure writes the SKU names with these capitals; a request may spell them in any case.
fn sku_name(given: &str) -> Option<&'static str> {
    const NAMES: &[&str] = &["Free", "Standard", "Premium", "PerNode", "PerGB2018", "Standalone", "CapacityReservation", "LACluster"];
    NAMES.iter().find(|n| n.eq_ignore_ascii_case(given)).copied()
}

/// 4 to 63 characters: letters, digits and hyphens, not starting or ending with a hyphen (Microsoft Learn, "Naming rules and
/// restrictions for Azure resources").
fn valid_workspace_name(n: &str) -> bool {
    (4..=63).contains(&n.len()) && !n.starts_with('-') && !n.ends_with('-') && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

impl OperationalInsights {
    fn shared_keys(res: &Stored) -> Response {
        let seed = format!("loganalytics/{}", res.id.to_ascii_lowercase());
        util::json_ok(
            200,
            &json!({
                "primarySharedKey": util::derive_key64(&format!("{seed}/primary"), generation(res, "primary")),
                "secondarySharedKey": util::derive_key64(&format!("{seed}/secondary"), generation(res, "secondary")),
            }),
        )
    }
}

#[async_trait]
impl Provider for OperationalInsights {
    fn namespace(&self) -> &'static str {
        "Microsoft.OperationalInsights"
    }

    fn types(&self) -> &'static [&'static str] {
        &["workspaces"]
    }

    /// Saved searches, tables, data exports and linked services stay plain JSON, as before.
    fn other_types_stored(&self) -> bool {
        true
    }

    fn statuses(&self, _type_path: &str) -> Statuses {
        Statuses { create: 200, update: 200, delete: 200, delete_missing: 204 }
    }

    fn global_name(&self, _type_path: &str) -> bool {
        false
    }

    fn validate_name(&self, type_path: &str, name: &str) -> ArmResult<()> {
        if type_path == "workspaces" && !valid_workspace_name(name) {
            return Err(ArmError::bad_request(
                "InvalidParameter",
                format!("The workspace name '{name}' is invalid. Use 4 to 63 letters, digits and hyphens, not starting or ending with a hyphen."),
            ));
        }
        Ok(())
    }

    async fn put(&self, cx: &Cx<'_>, res: &mut Stored, existed: bool, _children: &mut Vec<Stored>) -> ArmResult<()> {
        let now = cx.now_iso();
        let customer = guid_from(&format!("azemu-arm-key/loganalytics/{}/customer", res.id.to_ascii_lowercase()));
        res.body["etag"] = json!(new_etag());
        if !res.body.get("tags").map(Value::is_object).unwrap_or(false) {
            res.body["tags"] = json!({});
        }
        let p = util::props_mut(&mut res.body);
        let given = p.get("sku").and_then(|s| s.get("name")).and_then(Value::as_str).unwrap_or("PerGB2018").to_string();
        let Some(sku) = sku_name(&given) else {
            return Err(ArmError::bad_request("InvalidParameter", format!("The workspace sku '{given}' is not valid.")));
        };
        let mut sku_obj = p.get("sku").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
        sku_obj["name"] = json!(sku);
        set_field(&mut sku_obj, "lastSkuUpdate", json!(now));
        p.insert("sku".into(), sku_obj);
        if !existed {
            p.insert("createdDate".into(), json!(now));
        }
        p.insert("modifiedDate".into(), json!(now));
        p.insert("customerId".into(), json!(customer));
        util::set_default(p, "retentionInDays", json!(30));
        util::set_default(p, "publicNetworkAccessForIngestion", json!("Enabled"));
        util::set_default(p, "publicNetworkAccessForQuery", json!("Enabled"));
        let days = p.get("retentionInDays").and_then(Value::as_u64).unwrap_or(30);
        if !(7..=730).contains(&days) {
            return Err(ArmError::bad_request(
                "InvalidParameter",
                format!("retentionInDays {days} is not valid: it must be between 7 and 730."),
            ));
        }
        let mut features = p.get("features").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
        for (k, v) in [
            ("disableLocalAuth", json!(false)),
            ("enableLogAccessUsingOnlyResourcePermissions", json!(true)),
            ("legacy", json!(0)),
            ("searchVersion", json!(1)),
        ] {
            set_field(&mut features, k, v);
        }
        p.insert("features".into(), features);
        let mut capping = p.get("workspaceCapping").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
        set_field(&mut capping, "dailyQuotaGb", json!(-1.0));
        set_field(&mut capping, "dataIngestionStatus", json!("RespectQuota"));
        set_field(&mut capping, "quotaNextResetTime", json!(next_quota_reset(cx.now)));
        p.insert("workspaceCapping".into(), capping);
        Ok(())
    }

    async fn delete(&self, _cx: &Cx<'_>, _res: &Stored, _force: bool) -> ArmResult<()> {
        Ok(())
    }

    async fn action(
        &self,
        _cx: &Cx<'_>,
        target: ActionTarget<'_>,
        type_path: &str,
        action: &str,
        _body: &Value,
    ) -> ArmResult<Option<Response>> {
        match target {
            ActionTarget::Resource(res) if type_path == "workspaces" && action.eq_ignore_ascii_case("sharedKeys") => {
                Ok(Some(Self::shared_keys(res)))
            }
            _ => Ok(None),
        }
    }
}
