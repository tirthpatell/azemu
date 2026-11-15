//! The body of Query Blob Contents: `QueryRequest` with the expression and the two serialization settings.

use super::input::Delimited;
use super::sql::{self, ErrKind, Expr, Select, SqlError, Step};
use crate::errors::ApiError;
use crate::xml::{self, Node};

/// The query expression may be 256 KiB (REST documentation).
pub const MAX_EXPRESSION: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum InputFormat {
    Delimited(Delimited),
    Json {
        record: Vec<u8>,
    },
    /// Read by Azure; the emulator has no Parquet reader (see `mod.rs`: a blob that is not Parquet is refused as Azure does).
    Parquet,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutputFormat {
    Delimited(Delimited),
    Json { record: Vec<u8> },
}

/// What the expression needs from the data, to be checked against the first bytes of the blob before the answer starts
/// (Azure refuses these with a `400`, not in the stream).
#[derive(Debug, Clone, Default)]
pub struct Needs {
    /// Column names (and whether each was quoted) that must be in the header row.
    pub names: Vec<(String, bool)>,
    /// The largest `_N` used.
    pub max_ordinal: usize,
}

/// A request that has been read and checked, with the expression parsed and its names and paths resolved against the
/// input format. What is wrong in the expression is an error of the request (`400 ErrorProcessingQueryExpression`).
#[derive(Debug, Clone)]
pub struct Plan {
    pub select: Select,
    pub input: InputFormat,
    pub output: OutputFormat,
    pub needs: Needs,
}

pub const NO_HEADER: &str = "Column names can be used only if 'hasHeader' is set in the config and there is a header row.";
pub const UNKNOWN_COLUMN: &str = "Column name(s) does not exist in the header row.";

pub fn expression_error(message: impl Into<String>) -> ApiError {
    ApiError::new("ErrorProcessingQueryExpression").msg(message)
}

/// Every node of an expression tree.
fn visit(e: &Expr, f: &mut impl FnMut(&Expr)) {
    f(e);
    match e {
        Expr::Lit(_) | Expr::Ref(_) => {}
        Expr::Neg(x) | Expr::Not(x) | Expr::Cast(x, _) | Expr::Extract(_, x) => visit(x, f),
        Expr::IsMissing { e, .. } => visit(e, f),
        Expr::Bin(_, a, b) => {
            visit(a, f);
            visit(b, f);
        }
        Expr::Between { e, lo, hi, .. } => {
            visit(e, f);
            visit(lo, f);
            visit(hi, f);
        }
        Expr::In { e, list, .. } => {
            visit(e, f);
            list.iter().for_each(|x| visit(x, f));
        }
        Expr::Like { e, pat, esc, .. } => {
            visit(e, f);
            visit(pat, f);
            if let Some(x) = esc {
                visit(x, f);
            }
        }
        Expr::Func(_, _, args) => args.iter().for_each(|x| visit(x, f)),
        Expr::Trim { chars, e, .. } => {
            visit(e, f);
            if let Some(c) = chars {
                visit(c, f);
            }
        }
        Expr::Agg(_, arg) => {
            if let Some(a) = arg {
                visit(a, f);
            }
        }
    }
}

/// The checks that need the input format but not the data (checked against Azure in November 2026).
fn resolve(select: &Select, input: &InputFormat) -> Result<Needs, ApiError> {
    let mut needs = Needs::default();
    let mut missing = false;
    let visit_all = |f: &mut dyn FnMut(&Expr)| {
        if let sql::Projection::List(items) = &select.projection {
            items.iter().for_each(|i| visit(&i.expr, &mut |e| f(e)));
        }
        if let Some(w) = &select.filter {
            visit(w, &mut |e| f(e));
        }
    };
    visit_all(&mut |e| match e {
        Expr::IsMissing { .. } => missing = true,
        Expr::Ref(steps) => match steps.first() {
            Some(Step::Ordinal(n)) => needs.max_ordinal = needs.max_ordinal.max(*n),
            Some(Step::Field { name, quoted }) => needs.names.push((name.clone(), *quoted)),
            _ => {}
        },
        _ => {}
    });
    match input {
        InputFormat::Delimited(d) => {
            if select.has_path {
                return Err(expression_error("Record paths are not valid for queries over CSV-formatted input."));
            }
            if missing {
                return Err(expression_error("Missing Expression is not valid for this format!"));
            }
            if !needs.names.is_empty() && !d.headers {
                return Err(expression_error(NO_HEADER));
            }
        }
        InputFormat::Json { .. } => {
            if select.from.iter().any(|s| matches!(s, sql::FromStep::Index(_))) {
                return Err(expression_error("Invalid path for JSON-formatted input."));
            }
            needs = Needs::default();
        }
        InputFormat::Parquet => needs = Needs::default(),
    }
    Ok(needs)
}

/// What a refused expression is called on the wire. Azure writes a syntax error as `Syntax error in query: ` and the
/// expression; the other refusals have a text of their own.
fn sql_error(e: SqlError, expression: &str, input: &InputFormat) -> ApiError {
    let json = matches!(input, InputFormat::Json { .. });
    let headers = matches!(input, InputFormat::Delimited(d) if d.headers);
    expression_error(match (e.kind, json) {
        (ErrKind::Table, false) => "Record paths are not valid for queries over CSV-formatted input.".to_string(),
        (ErrKind::Table, true) => "Invalid path for JSON-formatted input.".to_string(),
        (ErrKind::Column, false) if headers => UNKNOWN_COLUMN.to_string(),
        (ErrKind::Column, false) => NO_HEADER.to_string(),
        (ErrKind::Other(m), _) => m.to_string(),
        _ => format!("Syntax error in query: {expression}"),
    })
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
    // A missing or empty `QueryType` or `Expression` is `InvalidXmlDocument` on Azure, and so is any `QueryType`
    // value other than `SQL`: Azure accepts it and runs the expression (checked in November 2026).
    if root.child_text("QueryType").is_none() {
        return Err(ApiError::invalid_xml("The QueryType element is missing."));
    }
    let expression = match root.child_text("Expression") {
        Some(e) if !e.trim().is_empty() => e,
        _ => return Err(ApiError::invalid_xml("The Expression element is missing or empty.")),
    };
    if expression.len() > MAX_EXPRESSION {
        return Err(ApiError::new("InvalidInput").msg("The maximum size of the query expression is 256 KiB."));
    }

    let input = match format_of(&root, "InputSerialization")? {
        None => InputFormat::Delimited(Delimited::default()),
        Some((fmt, ty)) => match ty.as_str() {
            "delimited" | "csv" => InputFormat::Delimited(delimited(fmt.child("DelimitedTextConfiguration"))?),
            "json" => InputFormat::Json { record: json_record(fmt.child("JsonTextConfiguration"))? },
            "parquet" => InputFormat::Parquet,
            _ => return Err(bad_value("Type", &ty)),
        },
    };
    let output = match format_of(&root, "OutputSerialization")? {
        // no output settings: the answer is in the format of the input, with the input's separators
        None => match &input {
            InputFormat::Delimited(d) => OutputFormat::Delimited(Delimited { headers: false, ..d.clone() }),
            InputFormat::Json { record } => OutputFormat::Json { record: record.clone() },
            InputFormat::Parquet => OutputFormat::Delimited(Delimited::default()),
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
    let select = sql::parse(expression).map_err(|e| sql_error(e, expression, &input))?;
    let needs = resolve(&select, &input)?;
    Ok(Plan { select, input, output, needs })
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
        assert!(matches!(p.select.projection, super::sql::Projection::Star));
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
        // Azure (November 2026): a missing QueryType or Expression is InvalidXmlDocument, an empty Expression too
        assert_eq!(code(&req("<Expression>x</Expression>")).code, "InvalidXmlDocument");
        assert_eq!(code(&req("<QueryType>SQL</QueryType>")).code, "InvalidXmlDocument");
        assert_eq!(code(&req("<QueryType>SQL</QueryType><Expression></Expression>")).code, "InvalidXmlDocument");
        // and a QueryType other than SQL is accepted
        assert!(parse(req("<QueryType>XXX</QueryType><Expression>SELECT * FROM BlobStorage</Expression>").as_bytes()).is_ok());
        let ser = |i: &str, o: &str| req(&format!("<QueryType>SQL</QueryType><Expression>SELECT * FROM BlobStorage</Expression>{i}{o}"));
        let fmt = |w: &str, t: &str| format!("<{w}Serialization><Format><Type>{t}</Type></Format></{w}Serialization>");
        assert_eq!(code(&ser(&fmt("Input", "arrow"), "")).code, "InvalidXmlNodeValue");
        assert_eq!(code(&ser("", &fmt("Output", "parquet"))).code, "InvalidXmlNodeValue");
        // parquet input is read when the blob is looked at (`check_data` in mod.rs)
        assert_eq!(parse(ser(&fmt("Input", "parquet"), "").as_bytes()).unwrap().input, InputFormat::Parquet);
        let e = code(&ser("", &fmt("Output", "arrow")));
        assert_eq!(e.status, 501);
        assert!(e.message.contains("arrow"));
        assert_eq!(code(&ser("<InputSerialization />", "")).code, "MissingRequiredXmlNode");
        let bad = "<InputSerialization><Format><Type>delimited</Type><DelimitedTextConfiguration><HasHeaders>maybe</HasHeaders></DelimitedTextConfiguration></Format></InputSerialization>";
        assert_eq!(code(&ser(bad, "")).code, "InvalidXmlNodeValue");
        let huge = req(&format!("<QueryType>SQL</QueryType><Expression>{}</Expression>", "x".repeat(MAX_EXPRESSION + 1)));
        assert_eq!(code(&huge).code, "InvalidInput");
    }

    /// Answers copied from Azure (November 2026): every refusal of the expression is `400 ErrorProcessingQueryExpression`.
    #[test]
    fn a_bad_expression_is_a_400_with_azures_text() {
        let csv = |expr: &str, headers: bool| {
            let cfg = format!("<InputSerialization><Format><Type>delimited</Type><DelimitedTextConfiguration><HasHeaders>{headers}</HasHeaders></DelimitedTextConfiguration></Format></InputSerialization>");
            parse(req(&format!("<QueryType>SQL</QueryType><Expression>{expr}</Expression>{cfg}")).as_bytes()).unwrap_err()
        };
        let json = |expr: &str| {
            let cfg = "<InputSerialization><Format><Type>json</Type></Format></InputSerialization>";
            parse(req(&format!("<QueryType>SQL</QueryType><Expression>{expr}</Expression>{cfg}")).as_bytes()).unwrap_err()
        };
        let e = csv("SELEC * FROM BlobStorage", true);
        assert_eq!((e.status, e.code.as_str()), (400, "ErrorProcessingQueryExpression"));
        assert_eq!(e.message, "Syntax error in query: SELEC * FROM BlobStorage");
        for bad in [
            "SELECT FROM BlobStorage",
            "SELECT FOO(name) FROM BlobStorage",
            "SELECT 'abc FROM BlobStorage",
            "SELECT CASE WHEN name = 'bob' THEN 1 ELSE 0 END FROM BlobStorage",
            "SELECT city FROM BlobStorage GROUP BY city",
            "SELECT name FROM BlobStorage ORDER BY name",
            "SELECT DISTINCT city FROM BlobStorage",
            "SELECT name FROM (SELECT name FROM BlobStorage)",
            "SELECT 'a' || 'b' FROM BlobStorage",
            "SELECT name FROM BlobStorage WHERE age IS NULL",
            "SELECT name FROM BlobStorage WHERE age IS NOT NULL",
            "SELECT SUBSTRING(name FROM 2) FROM BlobStorage",
            "SELECT COUNT(*), MAX(name) FROM BlobStorage",
            "SELECT COUNT(*), COUNT(*) FROM BlobStorage",
            "SELECT COUNT(*), name FROM BlobStorage",
            "SELECT name FROM BlobStorage WHERE COUNT(*) > 1",
            "SELECT S.name FROM BlobStorage S",
            "SELECT name AS \"n m\" FROM BlobStorage",
            "SELECT name FROM BlobStorage LIMIT -1",
        ] {
            assert_eq!(csv(bad, true).message, format!("Syntax error in query: {bad}"), "{bad}");
        }
        assert_eq!(csv("SELECT * FROM Wrong", true).message, "Record paths are not valid for queries over CSV-formatted input.");
        assert_eq!(csv("SELECT * FROM BlobStorage[*]", true).message, "Record paths are not valid for queries over CSV-formatted input.");
        assert_eq!(csv("SELECT * FROM BlobStorage.x", true).message, "Record paths are not valid for queries over CSV-formatted input.");
        assert_eq!(json("SELECT * FROM Wrong").message, "Invalid path for JSON-formatted input.");
        assert_eq!(json("SELECT * FROM BlobStorage[0]").message, "Invalid path for JSON-formatted input.");
        assert_eq!(csv("SELECT name FROM BlobStorage", false).message, NO_HEADER);
        assert_eq!(csv("SELECT name FROM BlobStorage WHERE x = 1", false).message, NO_HEADER);
        assert_eq!(csv("SELECT DATE_ADD(foo, 1, TO_TIMESTAMP('2007-02-23T')) FROM BlobStorage", true).message, UNKNOWN_COLUMN);
        assert_eq!(
            csv("SELECT name FROM BlobStorage WHERE name IS MISSING", true).message,
            "Missing Expression is not valid for this format!"
        );
        assert_eq!(
            csv("SELECT * FROM BlobStorage WHERE name LIKE 'a%' ESCAPE 'xy'", true).message,
            "An invalid escape character was specified in the LIKE predicate."
        );
        // fine: ordinals without a header row, IS MISSING over JSON, a path into JSON
        assert!(parse(req("<QueryType>SQL</QueryType><Expression>SELECT _1 FROM BlobStorage</Expression>").as_bytes()).is_ok());
    }

    #[test]
    fn the_names_and_ordinals_an_expression_needs_are_kept_for_the_header_check() {
        let cfg = "<InputSerialization><Format><Type>delimited</Type><DelimitedTextConfiguration><HasHeaders>true</HasHeaders></DelimitedTextConfiguration></Format></InputSerialization>";
        let p = parse(
            req(&format!("<QueryType>SQL</QueryType><Expression>SELECT name, _3 FROM BlobStorage WHERE \"City\" = 'x'</Expression>{cfg}"))
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(p.needs.names, vec![("name".to_string(), false), ("City".to_string(), true)]);
        assert_eq!(p.needs.max_ordinal, 3);
    }
}
