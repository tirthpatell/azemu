//! Service-level operations: properties, stats, account information, user
//! delegation key and CORS.

use azemu_core::resp::Resp;
use azemu_core::time::{self, parse_iso8601};

use crate::cx::Cx;
use crate::errors::{err, ApiError, ApiResult};
use crate::state::{CorsRule, DeleteRetention, Logging, Metrics, Retention, ServiceProps, StaticWebsite};
use crate::xml::{self, Node, Out};

fn write_retention(o: &mut Out, r: &Retention) {
    o.open("RetentionPolicy").el("Enabled", &r.enabled.to_string());
    if let Some(d) = r.days {
        o.el("Days", &d.to_string());
    }
    o.close("RetentionPolicy");
}

fn write_metrics(o: &mut Out, tag: &str, m: &Metrics) {
    o.open(tag).el("Version", &m.version).el("Enabled", &m.enabled.to_string());
    if let Some(i) = m.include_apis {
        o.el("IncludeAPIs", &i.to_string());
    }
    write_retention(o, &m.retention);
    o.close(tag);
}

pub fn props_xml(p: &ServiceProps) -> String {
    let mut o = Out::new();
    o.open("StorageServiceProperties");
    o.open("Logging")
        .el("Version", &p.logging.version)
        .el("Read", &p.logging.read.to_string())
        .el("Write", &p.logging.write.to_string())
        .el("Delete", &p.logging.delete.to_string());
    write_retention(&mut o, &p.logging.retention);
    o.close("Logging");
    write_metrics(&mut o, "HourMetrics", &p.hour_metrics);
    write_metrics(&mut o, "MinuteMetrics", &p.minute_metrics);
    if p.cors.is_empty() {
        o.empty("Cors");
    } else {
        o.open("Cors");
        for r in &p.cors {
            o.open("CorsRule")
                .el("AllowedMethods", &r.allowed_methods)
                .el("AllowedOrigins", &r.allowed_origins)
                .el("AllowedHeaders", &azemu_core::storage::cors::header_list_in_azure_order(&r.allowed_headers))
                .el("ExposedHeaders", &azemu_core::storage::cors::header_list_in_azure_order(&r.exposed_headers))
                .el("MaxAgeInSeconds", &r.max_age.to_string())
                .close("CorsRule");
        }
        o.close("Cors");
    }
    o.open("DeleteRetentionPolicy").el("Enabled", &p.delete_retention.enabled.to_string());
    if let Some(d) = p.delete_retention.days {
        o.el("Days", &d.to_string());
    }
    o.el("AllowPermanentDelete", &p.delete_retention.allow_permanent_delete.to_string());
    o.close("DeleteRetentionPolicy");
    o.open("StaticWebsite").el("Enabled", &p.static_website.enabled.to_string());
    o.opt("IndexDocument", p.static_website.index_document.as_deref());
    o.opt("ErrorDocument404Path", p.static_website.error_document_404.as_deref());
    o.close("StaticWebsite");
    o.opt("DefaultServiceVersion", p.default_service_version.as_deref());
    o.close("StorageServiceProperties");
    o.finish()
}

pub fn get_properties(cx: &mut Cx) -> ApiResult {
    let xml = props_xml(&cx.account().props);
    Ok(Resp::new(200u16).xml(xml).build())
}

fn bool_of(n: &Node, name: &str) -> Result<Option<bool>, ApiError> {
    match n.child_text(name) {
        None => Ok(None),
        Some(t) => match t.trim() {
            "true" => Ok(Some(true)),
            "false" => Ok(Some(false)),
            _ => Err(ApiError::invalid_xml(&format!("The value of {name} must be true or false."))),
        },
    }
}

fn retention_of(n: &Node, max_days: Option<u32>) -> Result<Retention, ApiError> {
    let enabled = bool_of(n, "Enabled")?.ok_or_else(|| err("MissingRequiredXmlNode").detail("XmlNodeName", "Enabled"))?;
    let days = match n.child_text("Days") {
        None => None,
        Some(t) => Some(t.trim().parse::<i64>().map_err(|_| ApiError::invalid_xml("Days must be an integer."))?),
    };
    if enabled {
        match days {
            None => return Err(err("MissingRequiredXmlNode").detail("XmlNodeName", "Days")),
            Some(d) if d < 1 || max_days.map(|m| d > m as i64).unwrap_or(false) => {
                return Err(ApiError::invalid_xml("Retention days must be greater than 0 and less than or equal to 365 days."));
            }
            _ => {}
        }
    }
    Ok(Retention { enabled, days: days.map(|d| d as u32) })
}

fn metrics_of(n: &Node) -> Result<Metrics, ApiError> {
    let enabled = bool_of(n, "Enabled")?.ok_or_else(|| err("MissingRequiredXmlNode").detail("XmlNodeName", "Enabled"))?;
    let retention = match n.child("RetentionPolicy") {
        Some(r) => retention_of(r, Some(365))?,
        None => Retention::default(),
    };
    Ok(Metrics {
        version: n.child_text("Version").unwrap_or("1.0").to_string(),
        enabled,
        include_apis: bool_of(n, "IncludeAPIs")?,
        retention,
    })
}

const CORS_METHODS: &[&str] = &["DELETE", "GET", "HEAD", "MERGE", "POST", "OPTIONS", "PUT", "PATCH"];

fn cors_of(n: &Node) -> Result<Vec<CorsRule>, ApiError> {
    let mut rules = vec![];
    for r in n.children_named("CorsRule") {
        let txt = |name: &str| r.child_text(name).unwrap_or("").to_string();
        let methods = txt("AllowedMethods");
        if methods.is_empty() || txt("AllowedOrigins").is_empty() {
            return Err(err("MissingRequiredXmlNode").detail("XmlNodeName", "AllowedMethods"));
        }
        for m in methods.split(',') {
            if !CORS_METHODS.contains(&m.trim().to_ascii_uppercase().as_str()) {
                return Err(ApiError::invalid_xml("Invalid CORS method."));
            }
        }
        let max_age = r
            .child_text("MaxAgeInSeconds")
            .map(|t| t.trim().parse::<u32>().map_err(|_| ApiError::invalid_xml("MaxAgeInSeconds must be an integer.")))
            .transpose()?
            .unwrap_or(0);
        rules.push(CorsRule {
            allowed_origins: txt("AllowedOrigins"),
            allowed_methods: methods,
            allowed_headers: txt("AllowedHeaders"),
            exposed_headers: txt("ExposedHeaders"),
            max_age,
        });
    }
    if rules.len() > 5 {
        return Err(ApiError::invalid_xml("A maximum of 5 CORS rules can be specified."));
    }
    Ok(rules)
}

pub fn set_properties(cx: &mut Cx) -> ApiResult {
    let req = cx.req;
    if req.body.is_empty() {
        return Err(err("MissingRequiredXmlNode").msg("An XML node that's mandatory for this request is not specified."));
    }
    let text = std::str::from_utf8(&req.body).map_err(|_| ApiError::invalid_xml("The body is not valid UTF-8."))?;
    let root = xml::parse(text).map_err(|e| ApiError::invalid_xml(&e))?;
    if root.name != "StorageServiceProperties" {
        return Err(ApiError::invalid_xml("The root element must be StorageServiceProperties."));
    }
    // Build the new values first so a bad section changes nothing.
    let mut p = cx.account().props.clone();
    if let Some(n) = root.child("Logging") {
        p.logging = Logging {
            version: n.child_text("Version").unwrap_or("1.0").to_string(),
            read: bool_of(n, "Read")?.unwrap_or(false),
            write: bool_of(n, "Write")?.unwrap_or(false),
            delete: bool_of(n, "Delete")?.unwrap_or(false),
            retention: match n.child("RetentionPolicy") {
                Some(r) => retention_of(r, Some(365))?,
                None => Retention::default(),
            },
        };
    }
    if let Some(n) = root.child("HourMetrics") {
        p.hour_metrics = metrics_of(n)?;
    }
    if let Some(n) = root.child("MinuteMetrics") {
        p.minute_metrics = metrics_of(n)?;
    }
    if let Some(n) = root.child("Cors") {
        p.cors = cors_of(n)?;
    }
    if let Some(n) = root.child("DefaultServiceVersion") {
        let v = n.text.trim();
        p.default_service_version = if v.is_empty() { None } else { Some(v.to_string()) };
    }
    if let Some(n) = root.child("DeleteRetentionPolicy") {
        let r = retention_of(n, Some(365))?;
        p.delete_retention = DeleteRetention {
            enabled: r.enabled,
            days: r.days,
            allow_permanent_delete: bool_of(n, "AllowPermanentDelete")?.unwrap_or(false),
        };
    }
    if let Some(n) = root.child("StaticWebsite") {
        let enabled = bool_of(n, "Enabled")?.unwrap_or(false);
        let get = |name: &str| n.child_text(name).filter(|s| !s.is_empty()).map(str::to_string);
        p.static_website = StaticWebsite {
            enabled,
            index_document: if enabled { get("IndexDocument").or_else(|| get("DefaultIndexDocumentPath")) } else { None },
            error_document_404: if enabled { get("ErrorDocument404Path") } else { None },
        };
    }
    // `$logs` appears when logging is turned on, and also with an update of CORS, static website, the default service
    // version or soft delete (Microsoft Learn, "Azure Storage Analytics logging"). `$web` comes with the static website.
    let logging_on = p.logging.read || p.logging.write || p.logging.delete;
    let other = ["Cors", "StaticWebsite", "DefaultServiceVersion", "DeleteRetentionPolicy"].iter().any(|n| root.child(n).is_some());
    let web = p.static_website.enabled;
    let (now, config) = (cx.now, cx.st.config);
    let acct = cx.account();
    acct.props = p;
    if logging_on || other {
        crate::analytics::ensure_logs(acct, now, logging_on);
    }
    if web {
        crate::analytics::ensure_web(acct, now);
    }
    // a shorter retention period takes effect now
    crate::analytics::prune(acct, now, &config);
    Ok(Resp::new(202u16).build())
}

pub fn get_stats(cx: &mut Cx) -> ApiResult {
    let mut o = Out::new();
    o.open("StorageServiceStats")
        .open("GeoReplication")
        .el("Status", "live")
        .el("LastSyncTime", &time::rfc1123(cx.now))
        .close("GeoReplication")
        .close("StorageServiceStats");
    Ok(Resp::new(200u16).xml(o.finish()).build())
}

pub fn account_info(cx: &mut Cx) -> ApiResult {
    // true once any container of the account is hierarchical (see dfs/model.rs)
    let hns = cx.account().containers.values().any(|c| c.hns);
    Ok(Resp::new(200u16)
        .header("x-ms-sku-name", "Standard_RAGRS")
        .header("x-ms-account-kind", "StorageV2")
        .header("x-ms-is-hns-enabled", hns.to_string())
        .build())
}

pub fn user_delegation_key(cx: &mut Cx) -> ApiResult {
    let req = cx.req;
    let text = std::str::from_utf8(&req.body).map_err(|_| ApiError::invalid_xml("The body is not valid UTF-8."))?;
    let root = xml::parse(text).map_err(|e| ApiError::invalid_xml(&e))?;
    let expiry = root.child_text("Expiry").ok_or_else(|| err("MissingRequiredXmlNode").detail("XmlNodeName", "Expiry"))?;
    let exp = parse_iso8601(expiry).ok_or_else(|| ApiError::invalid_xml("Expiry is not a valid date."))?;
    let start = root.child_text("Start").and_then(parse_iso8601).unwrap_or(cx.now);
    let version = req.header("x-ms-version").unwrap_or("2020-02-10");
    let oid = uuid::Uuid::new_v4().to_string();
    let tid = uuid::Uuid::new_v4().to_string();
    let (start_s, exp_s) = (time::iso8601_secs(start), time::iso8601_secs(exp));
    // Derived from the fields it returns, so a SAS signed with it can be verified
    // later without remembering the key.
    let key = azemu_core::storage::sig::user_delegation_key_b64(
        &azemu_core::storage::StorageAccounts::new(),
        &cx.acct,
        &oid,
        &tid,
        &start_s,
        &exp_s,
        "b",
        version,
    );
    let mut o = Out::new();
    o.open("UserDelegationKey")
        .el("SignedOid", &oid)
        .el("SignedTid", &tid)
        .el("SignedStart", &start_s)
        .el("SignedExpiry", &exp_s)
        .el("SignedService", "b")
        .el("SignedVersion", version)
        .el("Value", &key)
        .close("UserDelegationKey");
    Ok(Resp::new(200u16).xml(o.finish()).build())
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

/// The account's CORS rules in the form `azemu_core::storage::cors` takes (the same code answers a preflight and adds
/// the headers to an actual request for Blob, Queue and Table).
pub fn cors_rules(props: &ServiceProps) -> Vec<azemu_core::storage::cors::CorsRule> {
    props
        .cors
        .iter()
        .map(|r| azemu_core::storage::cors::CorsRule {
            allowed_origins: r.allowed_origins.clone(),
            allowed_methods: r.allowed_methods.clone(),
            allowed_headers: r.allowed_headers.clone(),
            exposed_headers: r.exposed_headers.clone(),
            max_age: r.max_age,
        })
        .collect()
}
