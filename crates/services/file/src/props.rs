//! File service properties (metrics, CORS, SMB protocol settings) and the user delegation key.
//!
//! Soft delete of shares is not here: on Azure it is a setting of the storage account (Resource Manager), not of this API.

use azemu_core::resp::Resp;
use azemu_core::storage;
use azemu_core::time::{iso8601_secs, parse_iso8601, Duration};
use azemu_core::{Request, Response};
use serde::{Deserialize, Serialize};

use crate::errors;
use crate::state::State;
use crate::xml::{self, Node, Pos};

const MAX_CORS_RULES: usize = 5;
const CORS_METHODS: [&str; 8] = ["DELETE", "GET", "HEAD", "MERGE", "POST", "OPTIONS", "PUT", "PATCH"];
const RETENTION_REASON: &str = "Retention days must be greater than 0 and less than or equal to 365 days.";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    pub enabled: bool,
    pub days: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metrics {
    pub enabled: bool,
    pub include_apis: Option<bool>,
    pub retention: Retention,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorsRule {
    pub allowed_methods: String,
    pub allowed_origins: String,
    pub allowed_headers: String,
    pub exposed_headers: String,
    pub max_age: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceProps {
    pub hour: Metrics,
    pub minute: Metrics,
    pub cors: Vec<CorsRule>,
    /// `ProtocolSettings/SMB/Multichannel/Enabled`
    pub multichannel: bool,
}

fn off() -> Metrics {
    Metrics { enabled: false, include_apis: None, retention: Retention { enabled: false, days: None } }
}

/// What a storage account made in November 2026 reported.
impl Default for ServiceProps {
    fn default() -> Self {
        ServiceProps { hour: off(), minute: off(), cors: vec![], multichannel: false }
    }
}

/// The account's CORS rules in the form `azemu_core::storage::cors` takes.
pub fn cors_rules(state: &State, account: &str) -> Vec<storage::cors::CorsRule> {
    let Some(acct) = state.accounts.get(&account.to_ascii_lowercase()) else { return Vec::new() };
    acct.props
        .cors
        .iter()
        .map(|r| storage::cors::CorsRule {
            allowed_origins: r.allowed_origins.clone(),
            allowed_methods: r.allowed_methods.clone(),
            allowed_headers: r.allowed_headers.clone(),
            exposed_headers: r.exposed_headers.clone(),
            max_age: r.max_age,
        })
        .collect()
}

fn retention_xml(r: &Retention) -> String {
    let mut s = format!("<RetentionPolicy><Enabled>{}</Enabled>", r.enabled);
    if let Some(d) = r.days {
        s.push_str(&format!("<Days>{d}</Days>"));
    }
    s.push_str("</RetentionPolicy>");
    s
}

fn metrics_xml(tag: &str, m: &Metrics) -> String {
    let mut s = format!("<{tag}><Version>1.0</Version><Enabled>{}</Enabled>", m.enabled);
    if let Some(i) = m.include_apis {
        s.push_str(&format!("<IncludeAPIs>{i}</IncludeAPIs>"));
    }
    s.push_str(&retention_xml(&m.retention));
    s.push_str(&format!("</{tag}>"));
    s
}

impl ServiceProps {
    pub fn to_xml(&self) -> String {
        let mut s = String::from("<StorageServiceProperties>");
        s.push_str(&metrics_xml("HourMetrics", &self.hour));
        s.push_str(&metrics_xml("MinuteMetrics", &self.minute));
        if self.cors.is_empty() {
            s.push_str("<Cors />");
        } else {
            s.push_str("<Cors>");
            for r in &self.cors {
                s.push_str("<CorsRule>");
                s.push_str(&xml::element("AllowedMethods", &r.allowed_methods));
                s.push_str(&xml::element("AllowedOrigins", &r.allowed_origins));
                s.push_str(&xml::element("AllowedHeaders", &storage::cors::header_list_in_azure_order(&r.allowed_headers)));
                s.push_str(&xml::element("ExposedHeaders", &storage::cors::header_list_in_azure_order(&r.exposed_headers)));
                s.push_str(&format!("<MaxAgeInSeconds>{}</MaxAgeInSeconds></CorsRule>", r.max_age));
            }
            s.push_str("</Cors>");
        }
        s.push_str(&format!(
            "<ProtocolSettings><SMB><Multichannel><Enabled>{}</Enabled></Multichannel></SMB></ProtocolSettings></StorageServiceProperties>",
            self.multichannel
        ));
        s
    }
}

/// A validation failure: where it was found (or `None` for "no position") and why.
struct Invalid {
    pos: Option<Pos>,
    reason: String,
}

fn bad(pos: Option<Pos>, reason: &str) -> Invalid {
    Invalid { pos, reason: reason.to_string() }
}

const PARSE: &str = "Error parsing Xml content";

fn parse_retention(n: &Node) -> Result<Retention, Invalid> {
    let enabled = match n.child_text("Enabled") {
        Some(t) => xml::parse_bool(t).ok_or_else(|| bad(Some(n.end), PARSE))?,
        None => return Err(bad(Some(n.end), PARSE)),
    };
    let days = match n.child_text("Days") {
        Some(t) => match t.trim().parse::<i64>() {
            Ok(d) if (1..=365).contains(&d) => Some(d as u32),
            _ => return Err(bad(Some(n.end), RETENTION_REASON)),
        },
        None => None,
    };
    if enabled && days.is_none() {
        return Err(bad(Some(n.end), RETENTION_REASON));
    }
    Ok(Retention { enabled, days })
}

fn parse_metrics(n: &Node) -> Result<Metrics, Invalid> {
    let enabled = match n.child_text("Enabled") {
        Some(t) => xml::parse_bool(t).ok_or_else(|| bad(Some(n.end), PARSE))?,
        None => return Err(bad(Some(n.end), PARSE)),
    };
    let include_apis = match n.child_text("IncludeAPIs") {
        Some(t) => Some(xml::parse_bool(t).ok_or_else(|| bad(Some(n.end), PARSE))?),
        None => None,
    };
    let retention = match n.child("RetentionPolicy") {
        Some(r) => parse_retention(r)?,
        None => Retention { enabled: false, days: None },
    };
    Ok(Metrics { enabled, include_apis, retention })
}

fn split_list(s: &str) -> Vec<&str> {
    s.split(',').map(str::trim).filter(|x| !x.is_empty()).collect()
}

fn parse_cors(n: &Node) -> Result<Vec<CorsRule>, Invalid> {
    let rules: Vec<&Node> = n.children_named("CorsRule").collect();
    if rules.len() > MAX_CORS_RULES {
        return Err(bad(None, ""));
    }
    let mut out = Vec::new();
    for r in rules {
        let text = |name: &str| r.child_text(name).unwrap_or("").to_string();
        let methods = text("AllowedMethods");
        let origins = text("AllowedOrigins");
        if split_list(&methods).is_empty() || split_list(&origins).is_empty() {
            return Err(bad(None, ""));
        }
        if split_list(&methods).iter().any(|m| !CORS_METHODS.contains(&m.to_ascii_uppercase().as_str())) {
            return Err(bad(None, ""));
        }
        if split_list(&origins).len() > 64
            || split_list(&text("AllowedHeaders")).len() > 64
            || split_list(&text("ExposedHeaders")).len() > 64
        {
            return Err(bad(None, ""));
        }
        let max_age = match r.child_text("MaxAgeInSeconds") {
            Some(t) => t.trim().parse::<u32>().map_err(|_| bad(None, ""))?,
            None => 0,
        };
        out.push(CorsRule {
            allowed_methods: methods,
            allowed_origins: origins,
            allowed_headers: text("AllowedHeaders"),
            exposed_headers: text("ExposedHeaders"),
            max_age,
        });
    }
    Ok(out)
}

/// Apply the sections present in the body to `current`. Sections that are absent keep their value; a present section replaces the
/// old one as a whole.
fn apply(root: &Node, current: &ServiceProps) -> Result<ServiceProps, Invalid> {
    if root.name != "StorageServiceProperties" {
        return Err(bad(Some(root.end), PARSE));
    }
    if root.children.is_empty() {
        let p = root.after;
        return Err(bad(Some(p), &format!("{PARSE}: 'None' is an invalid XmlNodeType. Line {}, position {}.", p.line, p.col)));
    }
    let mut next = current.clone();
    if let Some(n) = root.child("Logging") {
        return Err(bad(Some(n.start), "Element Logging not supported for File service."));
    }
    if let Some(n) = root.child("HourMetrics") {
        next.hour = parse_metrics(n)?;
    }
    if let Some(n) = root.child("MinuteMetrics") {
        next.minute = parse_metrics(n)?;
    }
    if let Some(n) = root.child("Cors") {
        next.cors = parse_cors(n)?;
    }
    if let Some(p) = root.child("ProtocolSettings") {
        if let Some(m) = p.child("SMB").and_then(|s| s.child("Multichannel")) {
            next.multichannel = match m.child_text("Enabled") {
                Some(t) => xml::parse_bool(t).ok_or_else(|| bad(Some(m.end), PARSE))?,
                None => false,
            };
        }
    }
    Ok(next)
}

pub fn get_properties(state: &mut State, req: &Request) -> Response {
    let acct = state.accounts.entry(req.account.to_ascii_lowercase()).or_default();
    Resp::new(200u16).xml(xml::document(&acct.props.to_xml())).build()
}

pub fn set_properties(state: &mut State, req: &Request) -> Response {
    if req.body.is_empty() {
        return errors::invalid_header(&req.ctx, "Content-Length", "0");
    }
    let Ok(body) = std::str::from_utf8(&req.body) else {
        return errors::invalid_xml_no_pos(&req.ctx, PARSE);
    };
    let root = match xml::parse(body) {
        Ok(r) => r,
        Err(e) => return errors::invalid_xml(&req.ctx, e.pos, &e.reason),
    };
    let acct = state.accounts.entry(req.account.to_ascii_lowercase()).or_default();
    match apply(&root, &acct.props) {
        Ok(next) => {
            acct.props = next;
            Resp::new(202u16).build()
        }
        Err(inv) => match inv.pos {
            Some(p) => errors::invalid_xml(&req.ctx, p, &inv.reason),
            None => errors::invalid_xml_no_pos(&req.ctx, &inv.reason),
        },
    }
}

/// A key derived from the fields it returns (see `azemu_core::storage::sig`), as Queue answers.
pub fn user_delegation_key(req: &Request, accounts: &storage::StorageAccounts) -> Response {
    let Ok(body) = std::str::from_utf8(&req.body) else {
        return errors::invalid_xml_no_pos(&req.ctx, PARSE);
    };
    let root = match xml::parse(body) {
        Ok(r) => r,
        Err(e) => return errors::invalid_xml(&req.ctx, e.pos, &e.reason),
    };
    if root.name != "KeyInfo" {
        return errors::invalid_xml(&req.ctx, root.end, PARSE);
    }
    let Some(expiry) = root.child_text("Expiry").and_then(parse_iso8601) else {
        return errors::invalid_xml(&req.ctx, root.end, PARSE);
    };
    let start = match root.child_text("Start") {
        Some(t) => match parse_iso8601(t) {
            Some(s) => s,
            None => return errors::invalid_xml(&req.ctx, root.end, PARSE),
        },
        None => req.ctx.time,
    };
    if expiry <= start || expiry - start > Duration::days(7) {
        return errors::invalid_xml(&req.ctx, root.end, "Key expiry time must be after the start time and at most 7 days from it.");
    }
    let version = req.header("x-ms-version").unwrap_or(storage::DEFAULT_VERSION);
    let oid = uuid::Uuid::new_v4().to_string();
    let tid = uuid::Uuid::nil().to_string();
    let (start_s, expiry_s) = (iso8601_secs(start), iso8601_secs(expiry));
    let key = storage::sig::user_delegation_key_b64(accounts, &req.account, &oid, &tid, &start_s, &expiry_s, "f", version);
    let out = format!(
        "<UserDelegationKey><SignedOid>{oid}</SignedOid><SignedTid>{tid}</SignedTid><SignedStart>{start_s}</SignedStart><SignedExpiry>{expiry_s}</SignedExpiry><SignedService>f</SignedService><SignedVersion>{}</SignedVersion><Value>{key}</Value></UserDelegationKey>",
        storage::xml_escape(version),
    );
    Resp::new(200u16).xml(xml::document(&out)).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_str(body: &str) -> Result<ServiceProps, Invalid> {
        let root = xml::parse(body).unwrap();
        apply(&root, &ServiceProps::default())
    }

    #[test]
    fn default_is_what_a_new_account_reports() {
        assert_eq!(
            ServiceProps::default().to_xml(),
            "<StorageServiceProperties><HourMetrics><Version>1.0</Version><Enabled>false</Enabled><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></HourMetrics><MinuteMetrics><Version>1.0</Version><Enabled>false</Enabled><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></MinuteMetrics><Cors /><ProtocolSettings><SMB><Multichannel><Enabled>false</Enabled></Multichannel></SMB></ProtocolSettings></StorageServiceProperties>"
        );
    }

    #[test]
    fn logging_is_refused_with_its_position() {
        let e = apply_str("<StorageServiceProperties><Logging><Version>1.0</Version></Logging></StorageServiceProperties>").err().unwrap();
        assert_eq!(e.reason, "Element Logging not supported for File service.");
        assert_eq!(e.pos, Some(Pos { line: 1, col: 28 }));
    }

    #[test]
    fn cors_limits_and_absent_sections() {
        let rule = "<CorsRule><AllowedOrigins>a.com</AllowedOrigins><AllowedMethods>GET</AllowedMethods><AllowedHeaders /><ExposedHeaders /><MaxAgeInSeconds>0</MaxAgeInSeconds></CorsRule>";
        let five = format!("<StorageServiceProperties><Cors>{}</Cors></StorageServiceProperties>", rule.repeat(5));
        assert_eq!(apply_str(&five).ok().map(|p| p.cors.len()), Some(5));
        let six = format!("<StorageServiceProperties><Cors>{}</Cors></StorageServiceProperties>", rule.repeat(6));
        assert!(apply_str(&six).is_err());
        let p = apply_str("<StorageServiceProperties><ProtocolSettings><SMB><Multichannel><Enabled>true</Enabled></Multichannel></SMB></ProtocolSettings></StorageServiceProperties>").ok().unwrap();
        assert!(p.multichannel);
        assert_eq!(p.hour, ServiceProps::default().hour);
    }

    #[test]
    fn empty_root_is_refused_like_azure() {
        let e = apply_str("<StorageServiceProperties />").err().unwrap();
        assert!(e.reason.contains("'None' is an invalid XmlNodeType"));
        assert_eq!(e.pos, Some(Pos { line: 1, col: 29 }));
    }
}
