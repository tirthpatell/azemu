//! Rule descriptions: request XML to [`Rule`], and [`Rule`] back to XML.

use azemu_servicebus_sql::{Params, Value as SqlValue};

use super::error::{ApiError, ApiResult};
use super::render::{el, SB_NS, XSI_NS};
use super::util::{esc, plain, ts_7};
use super::xml::Node;
use crate::rules::{CorrelationFilter, Rule, RuleFilter, DEFAULT_RULE_NAME};

/// .NET durations have no counterpart in the SQL value type; they are kept as a marked string
/// so they round-trip through the API.
const DURATION_MARK: &str = "\u{0}azemu-duration:";
const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema";
const SER_NS: &str = "http://schemas.microsoft.com/2003/10/Serialization/";

fn type_of(n: &Node) -> &str {
    let t = n.attr("type").unwrap_or("");
    t.rsplit(':').next().unwrap_or(t)
}

fn kv_value(n: &Node) -> ApiResult<SqlValue> {
    let text = n.val();
    let bad = ApiError::bad_request;
    Ok(match type_of(n) {
        "int" | "long" | "short" | "byte" | "unsignedByte" | "unsignedShort" | "unsignedInt" | "unsignedLong" => {
            SqlValue::Long(text.parse().map_err(|_| bad())?)
        }
        "boolean" => SqlValue::Bool(match text {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return Err(bad()),
        }),
        "double" | "float" | "decimal" => SqlValue::Double(text.parse().map_err(|_| bad())?),
        "dateTime" => {
            let t = azemu_core::time::parse_iso8601(text).ok_or_else(bad)?;
            SqlValue::Timestamp(super::util::ms_of(t))
        }
        "duration" => SqlValue::Str(format!("{DURATION_MARK}{text}")),
        _ => SqlValue::Str(n.text.clone()),
    })
}

/// `KeyValueOfstringanyType` children. Parameter names are stored without `@`.
fn key_values(parent: &Node, strip_at: bool) -> ApiResult<Vec<(String, SqlValue)>> {
    let mut out = Vec::new();
    for e in parent.children_named("KeyValueOfstringanyType") {
        let key = e.child("Key").map(|k| k.val().to_string()).filter(|k| !k.is_empty()).ok_or_else(ApiError::bad_request)?;
        let value = e.child("Value").ok_or_else(ApiError::bad_request)?;
        let key = if strip_at { key.strip_prefix('@').unwrap_or(&key).to_string() } else { key };
        out.push((key, kv_value(value)?));
    }
    Ok(out)
}

fn params_of(n: Option<&Node>) -> ApiResult<Params> {
    let mut p = Params::new();
    if let Some(n) = n.and_then(|n| n.child("Parameters")) {
        p.extend(key_values(n, true)?);
    }
    Ok(p)
}

/// Build the rule `name` from a `RuleDescription` element.
pub fn parse(name: &str, desc: &Node, now: i64) -> ApiResult<Rule> {
    let bad = |m: String| ApiError::bad(m, true);
    let filter = desc.child("Filter");
    let mut rule = match filter.map(type_of) {
        None | Some("") | Some("TrueFilter") => Rule { name: name.into(), filter: RuleFilter::True, action: None, created_ms: now },
        Some("FalseFilter") => Rule { name: name.into(), filter: RuleFilter::False, action: None, created_ms: now },
        Some("SqlFilter") => {
            let expr = filter.and_then(|f| f.child("SqlExpression")).map(|e| e.val().to_string()).unwrap_or_default();
            if expr.is_empty() {
                return Err(ApiError::bad_request());
            }
            Rule::sql_filter(name, &expr, params_of(filter)?, now)
                .map_err(|e| bad(format!("The SQL expression of the filter is not valid: {e}")))?
        }
        Some("CorrelationFilter") => {
            let f = filter.ok_or_else(ApiError::bad_request)?;
            let text = |n: &str| f.child(n).map(|c| c.val().to_string()).filter(|s| !s.is_empty());
            let mut cf = CorrelationFilter {
                correlation_id: text("CorrelationId"),
                message_id: text("MessageId"),
                to: text("To"),
                reply_to: text("ReplyTo"),
                label: text("Label"),
                session_id: text("SessionId"),
                reply_to_session_id: text("ReplyToSessionId"),
                content_type: text("ContentType"),
                properties: Default::default(),
            };
            if let Some(p) = f.child("Properties") {
                cf.properties.extend(key_values(p, false)?);
            }
            Rule { name: name.into(), filter: RuleFilter::Correlation(cf), action: None, created_ms: now }
        }
        Some(other) => return Err(bad(format!("The filter type '{other}' is not supported."))),
    };
    let action = desc.child("Action");
    match action.map(type_of) {
        None | Some("") | Some("EmptyRuleAction") => {}
        Some("SqlRuleAction") => {
            let expr = action.and_then(|a| a.child("SqlExpression")).map(|e| e.val().to_string()).unwrap_or_default();
            rule = rule
                .with_action(&expr, params_of(action)?)
                .map_err(|e| bad(format!("The SQL expression of the action is not valid: {e}")))?;
        }
        Some(other) => return Err(bad(format!("The action type '{other}' is not supported."))),
    }
    Ok(rule)
}

fn int_type(v: i64) -> &'static str {
    if (i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&v) {
        "int"
    } else {
        "long"
    }
}

fn kv_xml(out: &mut String, key: &str, v: &SqlValue) {
    out.push_str("<KeyValueOfstringanyType>");
    el(out, "Key", key);
    let (ty, ns, text) = match v {
        SqlValue::Str(s) => match s.strip_prefix(DURATION_MARK) {
            Some(d) => ("duration", SER_NS, d.to_string()),
            None => ("string", XSD_NS, s.clone()),
        },
        SqlValue::Long(n) => (int_type(*n), XSD_NS, n.to_string()),
        SqlValue::Bool(b) => ("boolean", XSD_NS, b.to_string()),
        SqlValue::Double(d) => ("double", XSD_NS, d.to_string()),
        SqlValue::Timestamp(ms) => {
            let frac = ms.rem_euclid(1000);
            let text = if frac == 0 { plain(*ms) } else { format!("{}.{}", plain(*ms), format!("{frac:03}").trim_end_matches('0')) };
            ("dateTime", XSD_NS, text)
        }
        SqlValue::Null => ("string", XSD_NS, String::new()),
    };
    out.push_str(&format!("<Value i:type=\"d6p1:{ty}\" xmlns:d6p1=\"{ns}\">{}</Value>", esc(&text)));
    out.push_str("</KeyValueOfstringanyType>");
}

fn params_xml(out: &mut String, p: &Params) {
    if p.is_empty() {
        return;
    }
    out.push_str("<Parameters>");
    for (k, v) in p {
        kv_xml(out, &format!("@{k}"), v);
    }
    out.push_str("</Parameters>");
}

/// `RuleDescription` element. `full` is what GET and the lists return: they carry an empty
/// `<Parameters/>` after `RequiresPreprocessing`, which the PUT answers leave out.
pub fn desc_xml(rule: &Rule, full: bool) -> String {
    let mut o = format!("<RuleDescription xmlns=\"{SB_NS}\" xmlns:i=\"{XSI_NS}\">");
    match &rule.filter {
        RuleFilter::True => o.push_str(
            "<Filter i:type=\"TrueFilter\"><SqlExpression>1=1</SqlExpression><CompatibilityLevel>20</CompatibilityLevel></Filter>",
        ),
        RuleFilter::False => o.push_str(
            "<Filter i:type=\"FalseFilter\"><SqlExpression>1=0</SqlExpression><CompatibilityLevel>20</CompatibilityLevel></Filter>",
        ),
        RuleFilter::Sql { expression, parameters, compatibility_level, .. } => {
            // The model's `$Default` rule is the SQL filter `1=1`; the API calls it a true filter.
            if rule.name == DEFAULT_RULE_NAME && expression == "1=1" && *compatibility_level == 20 {
                o.push_str(
                    "<Filter i:type=\"TrueFilter\"><SqlExpression>1=1</SqlExpression><CompatibilityLevel>20</CompatibilityLevel></Filter>",
                );
            } else {
                o.push_str("<Filter i:type=\"SqlFilter\">");
                el(&mut o, "SqlExpression", expression);
                el(&mut o, "CompatibilityLevel", &compatibility_level.to_string());
                params_xml(&mut o, parameters);
                o.push_str("<RequiresPreprocessing>true</RequiresPreprocessing>");
                if full && parameters.is_empty() {
                    o.push_str("<Parameters/>");
                }
                o.push_str("</Filter>");
            }
        }
        RuleFilter::Correlation(c) => {
            o.push_str("<Filter i:type=\"CorrelationFilter\">");
            for (n, v) in [
                ("CorrelationId", &c.correlation_id),
                ("MessageId", &c.message_id),
                ("To", &c.to),
                ("ReplyTo", &c.reply_to),
                ("Label", &c.label),
                ("SessionId", &c.session_id),
                ("ReplyToSessionId", &c.reply_to_session_id),
                ("ContentType", &c.content_type),
            ] {
                if let Some(v) = v {
                    el(&mut o, n, v);
                }
            }
            if !c.properties.is_empty() {
                o.push_str("<Properties>");
                for (k, v) in &c.properties {
                    kv_xml(&mut o, k, v);
                }
                o.push_str("</Properties>");
            }
            o.push_str("</Filter>");
        }
    }
    match &rule.action {
        None => o.push_str("<Action i:type=\"EmptyRuleAction\"/>"),
        Some(a) => {
            o.push_str("<Action i:type=\"SqlRuleAction\">");
            el(&mut o, "SqlExpression", &a.expression);
            o.push_str("<CompatibilityLevel>20</CompatibilityLevel>");
            params_xml(&mut o, &a.parameters);
            o.push_str("<RequiresPreprocessing>true</RequiresPreprocessing>");
            if full && a.parameters.is_empty() {
                o.push_str("<Parameters/>");
            }
            o.push_str("</Action>");
        }
    }
    el(&mut o, "CreatedAt", &ts_7(rule.created_ms));
    el(&mut o, "Name", &rule.name);
    o.push_str("</RuleDescription>");
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::xml;

    fn rule_desc(body: &str) -> Node {
        xml::parse(body.as_bytes()).unwrap().child("content").unwrap().child("RuleDescription").unwrap().clone()
    }

    #[test]
    fn correlation_properties_round_trip() {
        let d = rule_desc(
            "<entry><content><RuleDescription><Filter xsi:type=\"CorrelationFilter\"><CorrelationId>c</CorrelationId><Properties>\
             <KeyValueOfstringanyType><Key>k_dur</Key><Value xsi:type=\"d6p1:duration\">P1DT2H3M</Value></KeyValueOfstringanyType>\
             <KeyValueOfstringanyType><Key>k_dt</Key><Value xsi:type=\"d6p1:dateTime\">2020-07-05T11:12:13</Value></KeyValueOfstringanyType>\
             <KeyValueOfstringanyType><Key>k_long</Key><Value xsi:type=\"d6p1:long\">2147483650</Value></KeyValueOfstringanyType>\
             <KeyValueOfstringanyType><Key>k_int</Key><Value xsi:type=\"d6p1:int\">2</Value></KeyValueOfstringanyType>\
             </Properties></Filter><Action xsi:type=\"SqlRuleAction\"><SqlExpression>SET P = @p</SqlExpression><Parameters>\
             <KeyValueOfstringanyType><Key>@p</Key><Value xsi:type=\"d6p1:boolean\">true</Value></KeyValueOfstringanyType></Parameters></Action></RuleDescription></content></entry>",
        );
        let r = parse("r", &d, 1_725_473_856_280).unwrap();
        let x = desc_xml(&r, false);
        assert!(
            x.contains(
                "<Value i:type=\"d6p1:duration\" xmlns:d6p1=\"http://schemas.microsoft.com/2003/10/Serialization/\">P1DT2H3M</Value>"
            ),
            "{x}"
        );
        assert!(x.contains("d6p1:dateTime\" xmlns:d6p1=\"http://www.w3.org/2001/XMLSchema\">2020-07-05T11:12:13<"), "{x}");
        assert!(x.contains("d6p1:long"), "{x}");
        assert!(x.contains("<Key>@p</Key>"), "{x}");
        assert!(x.contains("<Name>r</Name>"));
    }

    /// Recording `mgmt_tests/test_mgmt_rules.pyTestServiceBusAdministrationClientRuletest_mgmt_rule_create.json`, entries 3 and 4: the user properties
    /// of a correlation filter come back in the order they were sent (`key_string`, `key_int`, `key_long`, `key_bool`, ...), not sorted by name.
    #[test]
    fn correlation_properties_keep_the_order_they_were_sent_in() {
        let kv = |k: &str, t: &str, v: &str| {
            format!("<KeyValueOfstringanyType><Key>{k}</Key><Value xsi:type=\"d6p1:{t}\">{v}</Value></KeyValueOfstringanyType>")
        };
        let body = format!(
            "<entry><content><RuleDescription><Filter xsi:type=\"CorrelationFilter\"><Properties>{}{}{}{}</Properties></Filter></RuleDescription></content></entry>",
            kv("key_string", "string", "str1"),
            kv("key_int", "int", "2"),
            kv("key_bool", "boolean", "false"),
            kv("key_int", "int", "3"),
        );
        let r = parse("r", &rule_desc(&body), 0).unwrap();
        let x = desc_xml(&r, false);
        let keys: Vec<&str> = x.split("<Key>").skip(1).map(|p| p.split("</Key>").next().unwrap()).collect();
        assert_eq!(keys, ["key_string", "key_int", "key_bool"], "{x}");
        assert!(
            x.contains("<Key>key_int</Key><Value i:type=\"d6p1:int\" xmlns:d6p1=\"http://www.w3.org/2001/XMLSchema\">3<"),
            "the last value of a repeated key wins: {x}"
        );
    }

    #[test]
    fn default_rule_is_a_true_filter() {
        let x = desc_xml(&Rule::default_rule(0), false);
        assert!(x.contains("i:type=\"TrueFilter\""), "{x}");
        let x = desc_xml(&Rule::sql_filter("a", "1=1", Params::new(), 0).unwrap(), true);
        assert!(x.contains("i:type=\"SqlFilter\"") && x.contains("<Parameters/>"), "{x}");
    }

    #[test]
    fn bad_filters_are_refused() {
        let d = rule_desc("<entry><content><RuleDescription><Filter xsi:type=\"SqlFilter\"><SqlExpression>a ==== </SqlExpression></Filter></RuleDescription></content></entry>");
        assert!(parse("r", &d, 0).is_err());
        let d = rule_desc("<entry><content><RuleDescription><Filter xsi:type=\"Bogus\"/></RuleDescription></content></entry>");
        assert!(parse("r", &d, 0).is_err());
    }
}
