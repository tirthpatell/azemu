//! Writing result rows as delimited text or JSON.

use super::input::Delimited;
use super::json;
use super::value::Value;

/// One result row: named values (a select list), or a whole input record (`SELECT *` on JSON).
pub enum OutRow {
    Fields { names: Vec<String>, values: Vec<Value> },
    Whole(Value),
}

fn contains(hay: &[u8], pat: &[u8]) -> bool {
    !pat.is_empty() && hay.windows(pat.len()).any(|w| w == pat)
}

/// One field. It is put in quotes when it holds a column separator, a record separator, a quote, an escape
/// character or a line break; inside quotes a quote is written as escape + quote (or doubled when there is
/// no escape character) and the escape character is doubled. An empty field is written as nothing.
pub fn csv_field(c: &Delimited, text: &str, out: &mut Vec<u8>) {
    let b = text.as_bytes();
    let needs = !b.is_empty()
        && (contains(b, &c.column)
            || contains(b, &c.record)
            || contains(b, &c.quote)
            || contains(b, &c.escape)
            || b.contains(&b'\n')
            || b.contains(&b'\r'));
    if !needs || c.quote.is_empty() {
        out.extend_from_slice(b);
        return;
    }
    out.extend_from_slice(&c.quote);
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(&c.quote) {
            out.extend_from_slice(if c.escape.is_empty() { &c.quote } else { &c.escape });
            out.extend_from_slice(&c.quote);
            i += c.quote.len();
        } else if !c.escape.is_empty() && b[i..].starts_with(&c.escape) {
            out.extend_from_slice(&c.escape);
            out.extend_from_slice(&c.escape);
            i += c.escape.len();
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out.extend_from_slice(&c.quote);
}

fn csv_values(values: impl Iterator<Item = String>, c: &Delimited, out: &mut Vec<u8>) {
    for (i, v) in values.enumerate() {
        if i > 0 {
            out.extend_from_slice(&c.column);
        }
        csv_field(c, &v, out);
    }
    out.extend_from_slice(&c.record);
}

/// The header line for these column names.
pub fn csv_header(c: &Delimited, names: &[String], out: &mut Vec<u8>) {
    csv_values(names.iter().cloned(), c, out);
}

pub fn csv_row(c: &Delimited, row: &OutRow, out: &mut Vec<u8>) {
    match row {
        OutRow::Fields { values, .. } => csv_values(values.iter().map(Value::to_text), c, out),
        OutRow::Whole(Value::Object(o)) => {
            csv_values(o.iter().filter(|(_, v)| !matches!(v, Value::Missing)).map(|(_, v)| v.to_text()), c, out)
        }
        OutRow::Whole(v) => csv_values(std::iter::once(v.to_text()), c, out),
    }
}

/// The column names of a row, for a header line.
pub fn names_of(row: &OutRow) -> Vec<String> {
    match row {
        OutRow::Fields { names, .. } => names.clone(),
        OutRow::Whole(Value::Object(o)) => o.iter().filter(|(_, v)| !matches!(v, Value::Missing)).map(|(k, _)| k.clone()).collect(),
        OutRow::Whole(_) => vec!["_1".into()],
    }
}

pub fn json_row(record: &[u8], row: &OutRow, out: &mut Vec<u8>) {
    match row {
        OutRow::Fields { names, values } => {
            let obj = Value::Object(names.iter().cloned().zip(values.iter().cloned()).collect());
            json::write(&obj, out);
        }
        OutRow::Whole(v) => json::write(v, out),
    }
    out.extend_from_slice(record);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(c: &Delimited, s: &str) -> String {
        let mut o = vec![];
        csv_field(c, s, &mut o);
        String::from_utf8(o).unwrap()
    }

    #[test]
    fn fields_are_quoted_when_they_must_be() {
        let d = Delimited::default();
        assert_eq!(f(&d, "abc"), "abc");
        assert_eq!(f(&d, ""), "");
        assert_eq!(f(&d, "a,b"), "\"a,b\"");
        assert_eq!(f(&d, "a\"b"), "\"a\"\"b\"");
        assert_eq!(f(&d, "a\nb"), "\"a\nb\"");
        let e = Delimited { column: b";".to_vec(), quote: b"'".to_vec(), record: b"%".to_vec(), escape: b"\\".to_vec(), headers: false };
        assert_eq!(f(&e, "a,b"), "a,b");
        assert_eq!(f(&e, "a;b"), "'a;b'");
        assert_eq!(f(&e, "it's"), "'it\\'s'");
        assert_eq!(f(&e, "a\\b"), "'a\\\\b'");
        assert_eq!(f(&e, "100%"), "'100%'");
    }

    #[test]
    fn rows() {
        let d = Delimited::default();
        let mut o = vec![];
        csv_row(
            &d,
            &OutRow::Fields { names: vec![], values: vec![Value::Int(1), Value::Null, Value::Str("x,y".into()), Value::Float(2.5)] },
            &mut o,
        );
        assert_eq!(o, b"1,,\"x,y\",2.5\n");
        let row = OutRow::Fields { names: vec!["name".into(), "gone".into()], values: vec![Value::Str("owner".into()), Value::Missing] };
        let mut o = vec![];
        json_row(b";", &row, &mut o);
        assert_eq!(o, b"{\"name\":\"owner\"};");
        let mut o = vec![];
        json_row(b"\n", &OutRow::Fields { names: vec!["n".into()], values: vec![Value::Missing] }, &mut o);
        assert_eq!(o, b"{}\n");
    }
}
