//! The body of Query Blob Contents: `QueryRequest` with the expression and the two serialization settings.

use super::input::Delimited;
use super::sql::{self, Select, SqlError};
use crate::errors::ApiError;
use crate::xml::{self, Node};

/// The query expression may be 256 KiB (REST documentation).
pub const MAX_EXPRESSION: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum InputFormat {
    Delimited(Delimited),
    Json { record: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutputFormat {
    Delimited(Delimited),
    Json { record: Vec<u8> },
}

/// A request that has been read and checked, with the expression parsed. A syntax error in the expression is
/// kept, not returned: the service answers 200 and puts it in the stream as a fatal `ParseError`.
#[derive(Debug, Clone)]
pub struct Plan {
    pub select: Result<Select, SqlError>,
    pub input: InputFormat,
    pub output: OutputFormat,
}

fn missing_node(name: &str) -> ApiError {
    ApiError::new("MissingRequiredXmlNode").detail("XmlNodeName", name)
}

fn bad_value(name: &str, value: &str) -> ApiError {
    ApiError::new("InvalidXmlNodeValue").detail("XmlNodeName", name).detail("XmlNodeValue", value)
}

/// Separators are at most this long.
const MAX_SEPARATOR: usize = 16;

fn separator(cfg: &Node, name: &str, default: &[u8], empty_means_none: bool) -> Result<Vec<u8>, ApiError> {
    match cfg.child(name) {
        None => Ok(if empty_means_none && name == "EscapeChar" { vec![] } else { default.to_vec() }),
        Some(n) if n.text.is_empty() => Ok(if empty_means_none { vec![] } else { default.to_vec() }),
        Some(n) if n.text.len() > MAX_SEPARATOR => Err(bad_value(name, &n.text)),
        Some(n) => Ok(n.text.as_bytes().to_vec()),
    }
}

fn delimited(cfg: Option<&Node>) -> Result<Delimited, ApiError> {
    let d = Delimited::default();
    let Some(cfg) = cfg else { return Ok(d) };
    let headers = match cfg.child_text("HasHeaders").map(|t| t.trim().to_ascii_lowercase()) {
        None => false,
        Some(t) if t == "true" => true,
        Some(t) if t == "false" || t.is_empty() => false,
        Some(t) => return Err(bad_value("HasHeaders", &t)),
    };
    Ok(Delimited {
        column: separator(cfg, "ColumnSeparator", &d.column, false)?,
        // an empty quote is the default quote (the JavaScript SDK sends one when none is set); an empty escape means none
        quote: separator(cfg, "FieldQuote", &d.quote, false)?,
        record: separator(cfg, "RecordSeparator", &d.record, false)?,
        escape: separator(cfg, "EscapeChar", &d.escape, true)?,
        headers,
    })
}

fn json_record(cfg: Option<&Node>) -> Result<Vec<u8>, ApiError> {
    match cfg {
        Some(c) => separator(c, "RecordSeparator", b"\n", false),
        None => Ok(b"\n".to_vec()),
    }
}

/// Look up `<InputSerialization><Format>` / `<OutputSerialization><Format>`.
fn format_of<'a>(root: &'a Node, which: &str) -> Result<Option<(&'a Node, String)>, ApiError> {
    let Some(ser) = root.child(which) else { return Ok(None) };
    let fmt = ser.child("Format").ok_or_else(|| missing_node("Format"))?;
    let ty = fmt.child_text("Type").ok_or_else(|| missing_node("Type"))?.trim().to_ascii_lowercase();
    Ok(Some((fmt, ty)))
}

pub fn parse(body: &[u8]) -> Result<Plan, ApiError> {
    let text = std::str::from_utf8(body).map_err(|_| ApiError::invalid_xml("The request body is not valid UTF-8."))?;
    let root = xml::parse(text).map_err(|e| ApiError::invalid_xml(&e))?;
    if root.name != "QueryRequest" {
        return Err(ApiError::invalid_xml("The root element must be QueryRequest."));
    }
    let qt = root.child_text("QueryType").ok_or_else(|| missing_node("QueryType"))?;
    if qt.trim() != "SQL" {
        return Err(bad_value("QueryType", qt));
    }
    let expression = root.child_text("Expression").ok_or_else(|| missing_node("Expression"))?;
    if expression.len() > MAX_EXPRESSION {
        return Err(ApiError::new("InvalidInput").msg("The maximum size of the query expression is 256 KiB."));
    }

    let input = match format_of(&root, "InputSerialization")? {
        None => InputFormat::Delimited(Delimited::default()),
        Some((fmt, ty)) => match ty.as_str() {
            "delimited" | "csv" => InputFormat::Delimited(delimited(fmt.child("DelimitedTextConfiguration"))?),
            "json" => InputFormat::Json { record: json_record(fmt.child("JsonTextConfiguration"))? },
            "parquet" => {
                return Err(ApiError::not_implemented("Query Blob Contents with parquet input").unsupported("Blob_Query (Parquet input)"))
            }
            _ => return Err(bad_value("Type", &ty)),
        },
    };
    let output = match format_of(&root, "OutputSerialization")? {
        // no output settings: the answer is in the format of the input, with the input's separators
        None => match &input {
            InputFormat::Delimited(d) => OutputFormat::Delimited(Delimited { headers: false, ..d.clone() }),
            InputFormat::Json { record } => OutputFormat::Json { record: record.clone() },
        },
        Some((fmt, ty)) => match ty.as_str() {
            "delimited" | "csv" => OutputFormat::Delimited(delimited(fmt.child("DelimitedTextConfiguration"))?),
            "json" => OutputFormat::Json { record: json_record(fmt.child("JsonTextConfiguration"))? },
            "arrow" => {
                return Err(ApiError::not_implemented("Query Blob Contents with arrow output").unsupported("Blob_Query (Arrow output)"))
            }
            _ => return Err(bad_value("Type", &ty)),
        },
    };
    Ok(Plan { select: sql::parse(expression), input, output })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(inner: &str) -> String {
        format!("<?xml version='1.0' encoding='utf-8'?>\n<QueryRequest>{inner}</QueryRequest>")
    }

    #[test]
    fn the_recorded_requests() {
        // the body of the Python SDK's test with custom dialects (test_quick_query_readall_with_serialization_setting)
        let p = parse(
            req("<QueryType>SQL</QueryType><Expression>SELECT * from BlobStorage</Expression><InputSerialization><Format><Type>delimited</Type><DelimitedTextConfiguration><ColumnSeparator>,</ColumnSeparator><FieldQuote>\"</FieldQuote><RecordSeparator>\n</RecordSeparator><EscapeChar /><HasHeaders>false</HasHeaders></DelimitedTextConfiguration></Format></InputSerialization><OutputSerialization><Format><Type>delimited</Type><DelimitedTextConfiguration><ColumnSeparator>;</ColumnSeparator><FieldQuote>'</FieldQuote><RecordSeparator>.</RecordSeparator><EscapeChar>\\</EscapeChar><HasHeaders>false</HasHeaders></DelimitedTextConfiguration></Format></OutputSerialization>")
                .as_bytes(),
        )
        .unwrap();
        assert!(p.select.is_ok());
        assert_eq!(p.input, InputFormat::Delimited(Delimited::default()));
        assert_eq!(
            p.output,
            OutputFormat::Delimited(Delimited {
                column: b";".to_vec(),
                quote: b"'".to_vec(),
                record: b".".to_vec(),
                escape: b"\\".to_vec(),
                headers: false
            })
        );
        let p = parse(req("<QueryType>SQL</QueryType><Expression>SELECT name from BlobStorage</Expression><InputSerialization><Format><Type>json</Type><JsonTextConfiguration><RecordSeparator>\n</RecordSeparator></JsonTextConfiguration></Format></InputSerialization><OutputSerialization><Format><Type>json</Type><JsonTextConfiguration><RecordSeparator>;</RecordSeparator></JsonTextConfiguration></Format></OutputSerialization>").as_bytes()).unwrap();
        assert_eq!(p.input, InputFormat::Json { record: b"\n".to_vec() });
        assert_eq!(p.output, OutputFormat::Json { record: b";".to_vec() });
        // only the expression: delimited both ways with the defaults
        let p = parse(req("<QueryType>SQL</QueryType><Expression>SELECT * from BlobStorage</Expression>").as_bytes()).unwrap();
        assert_eq!(p.output, OutputFormat::Delimited(Delimited::default()));
        // only the input is given: the answer follows it
        let p = parse(req("<QueryType>SQL</QueryType><Expression>SELECT * from BlobStorage</Expression><InputSerialization><Format><Type>json</Type></Format></InputSerialization>").as_bytes()).unwrap();
        assert_eq!(p.output, OutputFormat::Json { record: b"\n".to_vec() });
    }

    #[test]
    fn empty_settings_are_the_defaults_except_the_escape_character() {
        // what the JavaScript SDK sends for a csv configuration with only a record separator
        let p = parse(req("<QueryType>SQL</QueryType><Expression>SELECT * FROM BlobStorage</Expression><InputSerialization><Format><Type>delimited</Type><DelimitedTextConfiguration><ColumnSeparator>,</ColumnSeparator><FieldQuote /><RecordSeparator>\n</RecordSeparator><EscapeChar /><HasHeaders>false</HasHeaders></DelimitedTextConfiguration></Format></InputSerialization>").as_bytes()).unwrap();
        assert_eq!(p.input, InputFormat::Delimited(Delimited::default()));
    }

    #[test]
    fn refusals_name_what_is_wrong() {
        let code = |b: &str| parse(b.as_bytes()).unwrap_err();
        assert_eq!(code("not xml").code, "InvalidXmlDocument");
        assert_eq!(code("<Other/>").code, "InvalidXmlDocument");
        assert_eq!(code(&req("<Expression>x</Expression>")).code, "MissingRequiredXmlNode");
        assert_eq!(code(&req("<QueryType>SQL</QueryType>")).code, "MissingRequiredXmlNode");
        assert_eq!(code(&req("<QueryType>XYZ</QueryType><Expression>x</Expression>")).code, "InvalidXmlNodeValue");
        let ser = |i: &str, o: &str| req(&format!("<QueryType>SQL</QueryType><Expression>x</Expression>{i}{o}"));
        let fmt = |w: &str, t: &str| format!("<{w}Serialization><Format><Type>{t}</Type></Format></{w}Serialization>");
        assert_eq!(code(&ser(&fmt("Input", "arrow"), "")).code, "InvalidXmlNodeValue");
        assert_eq!(code(&ser("", &fmt("Output", "parquet"))).code, "InvalidXmlNodeValue");
        let e = code(&ser(&fmt("Input", "parquet"), ""));
        assert_eq!((e.status, e.code.as_str()), (501, "NotImplemented"));
        assert!(e.message.contains("parquet"));
        let e = code(&ser("", &fmt("Output", "arrow")));
        assert_eq!(e.status, 501);
        assert!(e.message.contains("arrow"));
        assert_eq!(code(&ser("<InputSerialization />", "")).code, "MissingRequiredXmlNode");
        let bad = "<InputSerialization><Format><Type>delimited</Type><DelimitedTextConfiguration><HasHeaders>maybe</HasHeaders></DelimitedTextConfiguration></Format></InputSerialization>";
        assert_eq!(code(&ser(bad, "")).code, "InvalidXmlNodeValue");
        let huge = req(&format!("<QueryType>SQL</QueryType><Expression>{}</Expression>", "x".repeat(MAX_EXPRESSION + 1)));
        assert_eq!(code(&huge).code, "InvalidInput");
    }

    #[test]
    fn a_bad_expression_is_kept_for_the_stream() {
        let p = parse(req("<QueryType>SQL</QueryType><Expression>SELECT FROM</Expression>").as_bytes()).unwrap();
        assert!(p.select.is_err());
    }
}
