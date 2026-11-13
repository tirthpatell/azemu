//! OData JSON payload formats: `nometadata`, `minimalmetadata`, `fullmetadata`.

use crate::json::{write_str, ObjWriter};
use crate::state::Entity;
use crate::values::{format_ticks, wire, Value};

/// The `Content-Type` of a whole response. Azure writes a space after each semicolon here (recordings
/// `test_table.pyTestTabletest_create_table_fail_on_exist.json` entry 2, `test_table_entity.pyTestTableEntity*`; 1,752 answers).
pub const CT_NO: &str = "application/json; odata=nometadata; streaming=true; charset=utf-8";
pub const CT_MINIMAL: &str = "application/json; odata=minimalmetadata; streaming=true; charset=utf-8";
pub const CT_FULL: &str = "application/json; odata=fullmetadata; streaming=true; charset=utf-8";
/// The `Content-Type` of one part inside a `$batch` answer. Azure writes these without spaces (233 parts in the recordings,
/// for example `test_table_batch.pyTestTableBatch*`), unlike the header of the whole response.
pub const PART_CT_NO: &str = "application/json;odata=nometadata;streaming=true;charset=utf-8";
pub const PART_CT_MINIMAL: &str = "application/json;odata=minimalmetadata;streaming=true;charset=utf-8";
pub const PART_CT_FULL: &str = "application/json;odata=fullmetadata;streaming=true;charset=utf-8";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    No,
    Minimal,
    Full,
}

impl Level {
    /// From an `Accept` header: the first `application/json` entry decides;
    /// plain `application/json` and anything else mean minimal metadata.
    pub fn from_accept(accept: Option<&str>) -> Level {
        let Some(accept) = accept else {
            return Level::Minimal;
        };
        for item in accept.split(',') {
            let mut parts = item.split(';').map(str::trim);
            if !parts.next().is_some_and(|m| m.eq_ignore_ascii_case("application/json")) {
                continue;
            }
            for p in parts {
                if let Some(v) = p.strip_prefix("odata=") {
                    return match v.to_ascii_lowercase().as_str() {
                        "nometadata" => Level::No,
                        "fullmetadata" => Level::Full,
                        _ => Level::Minimal,
                    };
                }
            }
            return Level::Minimal;
        }
        Level::Minimal
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Level::No => CT_NO,
            Level::Minimal => CT_MINIMAL,
            Level::Full => CT_FULL,
        }
    }

    /// The `Content-Type` of a part of a `$batch` answer (no spaces; see `PART_CT_MINIMAL`).
    pub fn part_content_type(self) -> &'static str {
        match self {
            Level::No => PART_CT_NO,
            Level::Minimal => PART_CT_MINIMAL,
            Level::Full => PART_CT_FULL,
        }
    }
}

/// Everything needed to write entity JSON.
pub struct Fmt<'a> {
    pub level: Level,
    /// `Endpoint::base_url()`: `http://acct.table.localhost:4577` or `http://127.0.0.1:10002/acct`.
    pub base: &'a str,
    pub account: &'a str,
    pub table: &'a str,
    /// The `$select` of the request as it was sent (decoded). Azure repeats it at the end of `odata.metadata`
    /// (`.../$metadata#tbl/@Element&$select=age,ratio`; recording `test_table_entity.pyTestTableEntitytest_get_entity_with_select.json`, entry 2).
    pub select_raw: Option<&'a str>,
}

/// Percent-encode a key for use inside a `(PartitionKey='..',RowKey='..')` URL.
fn encode_key(k: &str) -> String {
    let mut out = String::new();
    for b in k.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'!'
            | b'$'
            | b'&'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
            | b':'
            | b'@' => out.push(b as char),
            b'\'' => out.push_str("''"),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// `table(PartitionKey='p',RowKey='r')`
pub fn edit_link(table: &str, pk: &str, rk: &str) -> String {
    format!("{table}(PartitionKey='{}',RowKey='{}')", encode_key(pk), encode_key(rk))
}

impl Fmt<'_> {
    pub fn entity_url(&self, pk: &str, rk: &str) -> String {
        format!("{}/{}", self.base, edit_link(self.table, pk, rk))
    }

    pub fn metadata_url(&self, element: bool) -> String {
        let select = match self.select_raw.map(str::trim) {
            // spaces are written as %20 (recording `test_table_entity_async.pyTestTableEntityAsynctest_query_entities_with_select.json`, entry 4)
            Some(sel) if !sel.is_empty() => format!("&$select={}", sel.replace(' ', "%20")),
            _ => String::new(),
        };
        format!("{}/$metadata#{}{}{}", self.base, self.table, if element { "/@Element" } else { "" }, select)
    }

    /// One entity. `single` adds `odata.metadata` (a GET or insert response; query
    /// results carry it once on the envelope). `select` limits the properties and
    /// lists system properties only when named.
    pub fn entity(&self, e: &Entity, single: bool, select: Option<&[String]>) -> String {
        let mut w = ObjWriter::new();
        let sel = |name: &str| select.is_none_or(|s| s.iter().any(|x| x == name));
        let etag = e.etag();
        match self.level {
            Level::No => {}
            Level::Minimal => {
                if single {
                    w.str("odata.metadata", &self.metadata_url(true));
                }
                w.str("odata.etag", &etag);
            }
            Level::Full => {
                if single {
                    w.str("odata.metadata", &self.metadata_url(true));
                }
                w.str("odata.type", &format!("{}.{}", self.account, self.table));
                w.str("odata.id", &self.entity_url(&e.pk, &e.rk));
                w.str("odata.etag", &etag);
                w.str("odata.editLink", &edit_link(self.table, &e.pk, &e.rk));
            }
        }
        if sel("PartitionKey") {
            w.str("PartitionKey", &e.pk);
        }
        if sel("RowKey") {
            w.str("RowKey", &e.rk);
        }
        if sel("Timestamp") {
            if self.level == Level::Full {
                w.str("Timestamp@odata.type", "Edm.DateTime");
            }
            w.str("Timestamp", &format_ticks(e.ts));
        }
        for (name, v) in &e.props {
            if !sel(name) {
                continue;
            }
            self.property(&mut w, name, v);
        }
        w.finish()
    }

    fn property(&self, w: &mut ObjWriter, name: &str, v: &Value) {
        let (text, ann) = wire(v);
        if let (Some(a), true) = (ann, self.level != Level::No) {
            w.str(&format!("{name}@odata.type"), a);
        }
        w.raw(name, &text);
    }
}

/// `{"odata.metadata":"..","value":[..]}` (the metadata member is left out for `nometadata`).
pub fn envelope(level: Level, metadata_url: &str, items: &[String]) -> String {
    let mut s = String::from("{");
    if level != Level::No {
        s.push_str("\"odata.metadata\":");
        write_str(&mut s, metadata_url);
        s.push(',');
    }
    s.push_str("\"value\":[");
    s.push_str(&items.join(","));
    s.push_str("]}");
    s
}

/// Parse `$select`: comma separated names, spaces around them allowed.
pub fn parse_select(raw: Option<&str>) -> Option<Vec<String>> {
    let raw = raw?;
    let names: Vec<String> = raw.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if names.is_empty() || names.iter().any(|n| n == "*") {
        None
    } else {
        Some(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent() -> Entity {
        Entity {
            pk: "p".into(),
            rk: "r k".into(),
            props: vec![
                ("n".into(), Value::Int32(1)),
                ("big".into(), Value::Int64(5)),
                ("d".into(), Value::DateTime(0)),
                ("f".into(), Value::Double(5.0)),
            ],
            ts: 0,
        }
    }

    fn fmt(level: Level) -> Fmt<'static> {
        Fmt { level, base: "http://a.table.localhost:1", account: "a", table: "t", select_raw: None }
    }

    #[test]
    fn accept_levels() {
        assert_eq!(Level::from_accept(None), Level::Minimal);
        assert_eq!(Level::from_accept(Some("application/json")), Level::Minimal);
        assert_eq!(Level::from_accept(Some("application/json;odata=nometadata")), Level::No);
        assert_eq!(Level::from_accept(Some("application/json; odata=fullmetadata")), Level::Full);
        assert_eq!(Level::from_accept(Some("*/*")), Level::Minimal);
    }

    #[test]
    fn minimal_matches_recorded_shape() {
        let s = fmt(Level::Minimal).entity(&ent(), true, None);
        assert_eq!(
            s,
            "{\"odata.metadata\":\"http://a.table.localhost:1/$metadata#t/@Element\",\"odata.etag\":\"W/\\\"datetime'1970-01-01T00%3A00%3A00Z'\\\"\",\"PartitionKey\":\"p\",\"RowKey\":\"r k\",\"Timestamp\":\"1970-01-01T00:00:00Z\",\"n\":1,\"big@odata.type\":\"Edm.Int64\",\"big\":\"5\",\"d@odata.type\":\"Edm.DateTime\",\"d\":\"1970-01-01T00:00:00Z\",\"f\":5.0}"
        );
    }

    #[test]
    fn metadata_url_repeats_the_select() {
        // recordings `test_table_entity.pyTestTableEntitytest_get_entity_with_select.json` entry 2 (`.../@Element&$select=age,ratio`) and
        // `test_table_entity_async.pyTestTableEntityAsynctest_query_entities_with_select.json` entry 4 (`#querytable&$select=age,%20sex`)
        let mut f = fmt(Level::Minimal);
        f.select_raw = Some("age,ratio");
        assert_eq!(f.metadata_url(true), "http://a.table.localhost:1/$metadata#t/@Element&$select=age,ratio");
        f.select_raw = Some("age, sex");
        assert_eq!(f.metadata_url(false), "http://a.table.localhost:1/$metadata#t&$select=age,%20sex");
        f.select_raw = Some("");
        assert_eq!(f.metadata_url(false), "http://a.table.localhost:1/$metadata#t");
    }

    #[test]
    fn nometadata_has_no_annotations() {
        let s = fmt(Level::No).entity(&ent(), true, None);
        assert!(!s.contains("odata"));
        assert!(s.contains("\"big\":\"5\""));
    }

    #[test]
    fn full_adds_ids_and_timestamp_type() {
        let s = fmt(Level::Full).entity(&ent(), true, None);
        assert!(s.contains("\"odata.type\":\"a.t\""));
        assert!(s.contains("\"odata.id\":\"http://a.table.localhost:1/t(PartitionKey='p',RowKey='r%20k')\""));
        assert!(s.contains("\"odata.editLink\":\"t(PartitionKey='p',RowKey='r%20k')\""));
        assert!(s.contains("\"Timestamp@odata.type\":\"Edm.DateTime\""));
    }

    #[test]
    fn select_lists_only_named_properties() {
        let sel = vec!["n".to_string(), "PartitionKey".to_string()];
        let s = fmt(Level::Minimal).entity(&ent(), false, Some(&sel));
        assert!(s.contains("\"PartitionKey\":\"p\"") && s.contains("\"n\":1"));
        assert!(!s.contains("RowKey") && !s.contains("Timestamp") && !s.contains("big"));
    }

    #[test]
    fn select_parsing() {
        assert_eq!(parse_select(Some("age, sex")), Some(vec!["age".to_string(), "sex".to_string()]));
        assert_eq!(parse_select(Some("*")), None);
        assert_eq!(parse_select(None), None);
    }

    #[test]
    fn edit_link_doubles_quotes() {
        assert_eq!(edit_link("t", "O'Brien", "x/y"), "t(PartitionKey='O''Brien',RowKey='x%2Fy')");
    }

    #[test]
    fn envelope_shape() {
        assert_eq!(envelope(Level::No, "u", &["{}".into()]), "{\"value\":[{}]}");
        assert_eq!(envelope(Level::Minimal, "u", &[]), "{\"odata.metadata\":\"u\",\"value\":[]}");
    }
}
