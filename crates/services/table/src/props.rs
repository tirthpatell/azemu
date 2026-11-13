//! Table service properties (logging, metrics, CORS): parse, validate, render.

use crate::errors::{invalid_xml, Res};
use crate::xml::{self, Node};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Retention {
    pub enabled: bool,
    pub days: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Logging {
    pub version: String,
    pub delete: bool,
    pub read: bool,
    pub write: bool,
    pub retention: Retention,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Metrics {
    pub version: String,
    pub enabled: bool,
    pub include_apis: Option<bool>,
    pub retention: Retention,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CorsRule {
    pub allowed_origins: String,
    pub allowed_methods: String,
    pub allowed_headers: String,
    pub exposed_headers: String,
    pub max_age: u32,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ServiceProps {
    pub logging: Logging,
    pub hour: Metrics,
    pub minute: Metrics,
    pub cors: Vec<CorsRule>,
}

impl Default for ServiceProps {
    fn default() -> Self {
        ServiceProps {
            logging: Logging {
                version: "1.0".into(),
                delete: false,
                read: false,
                write: false,
                retention: Retention { enabled: false, days: None },
            },
            hour: Metrics {
                version: "1.0".into(),
                enabled: true,
                include_apis: Some(true),
                retention: Retention { enabled: true, days: Some(7) },
            },
            minute: Metrics {
                version: "1.0".into(),
                enabled: false,
                include_apis: None,
                retention: Retention { enabled: false, days: None },
            },
            cors: Vec::new(),
        }
    }
}

const METHODS: [&str; 8] = ["DELETE", "GET", "HEAD", "MERGE", "POST", "OPTIONS", "PUT", "PATCH"];

fn text<'a>(n: &'a Node, name: &str) -> Res<&'a str> {
    n.child_text(name).ok_or_else(invalid_xml)
}

fn boolean(s: &str) -> Res<bool> {
    match s {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(invalid_xml()),
    }
}

fn retention(n: &Node) -> Res<Retention> {
    let r = n.child("RetentionPolicy").ok_or_else(invalid_xml)?;
    let enabled = boolean(text(r, "Enabled")?)?;
    let days = match r.child_text("Days") {
        Some(d) => Some(d.parse::<u32>().map_err(|_| invalid_xml())?),
        None => None,
    };
    if enabled {
        match days {
            Some(1..=365) => {}
            _ => return Err(invalid_xml()),
        }
    }
    Ok(Retention { enabled, days: if enabled { days } else { None } })
}

fn logging(n: &Node) -> Res<Logging> {
    Ok(Logging {
        version: text(n, "Version")?.to_string(),
        delete: boolean(text(n, "Delete")?)?,
        read: boolean(text(n, "Read")?)?,
        write: boolean(text(n, "Write")?)?,
        retention: retention(n)?,
    })
}

fn metrics(n: &Node) -> Res<Metrics> {
    let enabled = boolean(text(n, "Enabled")?)?;
    let include_apis = match n.child_text("IncludeAPIs") {
        Some(v) => Some(boolean(v)?),
        None if enabled => return Err(invalid_xml()),
        None => None,
    };
    Ok(Metrics {
        version: text(n, "Version")?.to_string(),
        enabled,
        include_apis: if enabled { include_apis } else { None },
        retention: retention(n)?,
    })
}

fn cors_rule(n: &Node) -> Res<CorsRule> {
    let methods = text(n, "AllowedMethods")?;
    if methods.is_empty() || !methods.split(',').all(|m| METHODS.contains(&m.trim().to_ascii_uppercase().as_str())) {
        return Err(invalid_xml());
    }
    let origins = text(n, "AllowedOrigins")?;
    if origins.is_empty() {
        return Err(invalid_xml());
    }
    let max_age = text(n, "MaxAgeInSeconds")?.parse::<u32>().map_err(|_| invalid_xml())?;
    Ok(CorsRule {
        allowed_origins: origins.to_string(),
        allowed_methods: methods.to_string(),
        allowed_headers: text(n, "AllowedHeaders")?.to_string(),
        exposed_headers: text(n, "ExposedHeaders")?.to_string(),
        max_age,
    })
}

/// Apply a `Set Table Service Properties` body to `current`. Sections that are
/// not in the body keep their value. Nothing changes when the body is invalid.
pub fn apply(current: &ServiceProps, body: &str) -> Res<ServiceProps> {
    let root = xml::parse(body).map_err(|_| invalid_xml())?;
    if root.name != "StorageServiceProperties" {
        return Err(invalid_xml());
    }
    let mut next = current.clone();
    if let Some(n) = root.child("Logging") {
        next.logging = logging(n)?;
    }
    if let Some(n) = root.child("HourMetrics") {
        next.hour = metrics(n)?;
    }
    if let Some(n) = root.child("MinuteMetrics") {
        next.minute = metrics(n)?;
    }
    if let Some(n) = root.child("Cors") {
        let rules: Res<Vec<CorsRule>> = n.children_named("CorsRule").map(cors_rule).collect();
        let rules = rules?;
        if rules.len() > 5 {
            return Err(invalid_xml());
        }
        next.cors = rules;
    }
    Ok(next)
}

fn retention_xml(r: &Retention) -> String {
    match (r.enabled, r.days) {
        (true, Some(d)) => format!("<RetentionPolicy><Enabled>true</Enabled><Days>{d}</Days></RetentionPolicy>"),
        _ => "<RetentionPolicy><Enabled>false</Enabled></RetentionPolicy>".to_string(),
    }
}

fn metrics_xml(tag: &str, m: &Metrics) -> String {
    let mut s = format!("<{tag}><Version>{}</Version><Enabled>{}</Enabled>", xml::escape(&m.version), m.enabled);
    if let (true, Some(i)) = (m.enabled, m.include_apis) {
        s.push_str(&format!("<IncludeAPIs>{i}</IncludeAPIs>"));
    }
    s.push_str(&retention_xml(&m.retention));
    s.push_str(&format!("</{tag}>"));
    s
}

fn elem(tag: &str, v: &str) -> String {
    if v.is_empty() {
        format!("<{tag} />")
    } else {
        format!("<{tag}>{}</{tag}>", xml::escape(v))
    }
}

/// The `Get Table Service Properties` body (with the byte order mark Azure sends).
pub fn render(p: &ServiceProps) -> String {
    let l = &p.logging;
    let mut s = String::from("\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><StorageServiceProperties>");
    s.push_str(&format!(
        "<Logging><Version>{}</Version><Read>{}</Read><Write>{}</Write><Delete>{}</Delete>{}</Logging>",
        xml::escape(&l.version),
        l.read,
        l.write,
        l.delete,
        retention_xml(&l.retention)
    ));
    s.push_str(&metrics_xml("HourMetrics", &p.hour));
    s.push_str(&metrics_xml("MinuteMetrics", &p.minute));
    if p.cors.is_empty() {
        s.push_str("<Cors />");
    } else {
        s.push_str("<Cors>");
        for r in &p.cors {
            s.push_str("<CorsRule>");
            s.push_str(&elem("AllowedMethods", &r.allowed_methods));
            s.push_str(&elem("AllowedOrigins", &r.allowed_origins));
            s.push_str(&elem("AllowedHeaders", &azemu_core::storage::cors::header_list_in_azure_order(&r.allowed_headers)));
            s.push_str(&elem("ExposedHeaders", &azemu_core::storage::cors::header_list_in_azure_order(&r.exposed_headers)));
            s.push_str(&format!("<MaxAgeInSeconds>{}</MaxAgeInSeconds></CorsRule>", r.max_age));
        }
        s.push_str("</Cors>");
    }
    s.push_str("</StorageServiceProperties>");
    s
}

pub fn stats_xml(now: &str) -> String {
    format!(
        "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><StorageServiceStats><GeoReplication><Status>live</Status><LastSyncTime>{now}</LastSyncTime></GeoReplication></StorageServiceStats>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(body: &str) -> Res<ServiceProps> {
        apply(&ServiceProps::default(), body)
    }

    #[test]
    fn recorded_set_all_sections_off() {
        let p = set("<?xml version='1.0' encoding='utf-8'?>\n<StorageServiceProperties><Logging><Version>1.0</Version><Delete>false</Delete><Read>false</Read><Write>false</Write><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></Logging><HourMetrics><Version>1.0</Version><Enabled>false</Enabled><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></HourMetrics><MinuteMetrics><Version>1.0</Version><Enabled>false</Enabled><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></MinuteMetrics><Cors /></StorageServiceProperties>").unwrap();
        assert!(!p.hour.enabled && p.cors.is_empty());
        let out = render(&p);
        assert!(out.contains("<Logging><Version>1.0</Version><Read>false</Read><Write>false</Write><Delete>false</Delete><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></Logging>"));
        assert!(out.ends_with("<Cors /></StorageServiceProperties>"));
    }

    #[test]
    fn untouched_sections_keep_their_value() {
        let p = set("<StorageServiceProperties><MinuteMetrics><Version>1.0</Version><Enabled>true</Enabled><IncludeAPIs>true</IncludeAPIs><RetentionPolicy><Enabled>true</Enabled><Days>2</Days></RetentionPolicy></MinuteMetrics></StorageServiceProperties>").unwrap();
        assert!(p.minute.enabled);
        assert_eq!(p.hour, ServiceProps::default().hour);
    }

    #[test]
    fn retention_366_and_zero_are_invalid() {
        for d in ["366", "0"] {
            let body = format!("<StorageServiceProperties><MinuteMetrics><Version>1.0</Version><Enabled>true</Enabled><IncludeAPIs>true</IncludeAPIs><RetentionPolicy><Enabled>true</Enabled><Days>{d}</Days></RetentionPolicy></MinuteMetrics></StorageServiceProperties>");
            assert_eq!(set(&body).unwrap_err().code, "InvalidXmlDocument");
        }
    }

    #[test]
    fn six_cors_rules_are_invalid() {
        let rule = "<CorsRule><AllowedOrigins>a.com</AllowedOrigins><AllowedMethods>GET</AllowedMethods><AllowedHeaders /><ExposedHeaders /><MaxAgeInSeconds>0</MaxAgeInSeconds></CorsRule>";
        let ok = format!("<StorageServiceProperties><Cors>{}</Cors></StorageServiceProperties>", rule.repeat(5));
        assert_eq!(set(&ok).unwrap().cors.len(), 5);
        let bad = format!("<StorageServiceProperties><Cors>{}</Cors></StorageServiceProperties>", rule.repeat(6));
        assert_eq!(set(&bad).unwrap_err().code, "InvalidXmlDocument");
    }

    #[test]
    fn cors_round_trip_keeps_order_of_origins_and_puts_header_prefixes_last() {
        let body = "<StorageServiceProperties><Cors><CorsRule><AllowedOrigins>www.xyz.com,www.ab.com</AllowedOrigins><AllowedMethods>GET,PUT</AllowedMethods><AllowedHeaders>x-ms-meta-data*,x-ms-meta-foo</AllowedHeaders><ExposedHeaders>x-ms-meta-abc</ExposedHeaders><MaxAgeInSeconds>500</MaxAgeInSeconds></CorsRule></Cors></StorageServiceProperties>";
        let p = set(body).unwrap();
        let out = render(&p);
        assert!(out.contains("<AllowedOrigins>www.xyz.com,www.ab.com</AllowedOrigins>"));
        // Azure answers names first, prefixes (ending in `*`) last: recording
        // `test_table_service_properties.pyTestTableServicePropertiestest_set_cors.json`, entry 2
        assert!(out.contains("<AllowedHeaders>x-ms-meta-foo,x-ms-meta-data*</AllowedHeaders>"), "{out}");
    }

    #[test]
    fn bad_documents() {
        for bad in ["", "<Other/>", "<StorageServiceProperties><Logging/></StorageServiceProperties>", "not xml"] {
            assert!(set(bad).is_err(), "{bad}");
        }
        let bad_method = "<StorageServiceProperties><Cors><CorsRule><AllowedOrigins>a</AllowedOrigins><AllowedMethods>FETCH</AllowedMethods><AllowedHeaders /><ExposedHeaders /><MaxAgeInSeconds>0</MaxAgeInSeconds></CorsRule></Cors></StorageServiceProperties>";
        assert!(set(bad_method).is_err());
    }
}
