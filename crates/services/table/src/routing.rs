//! Request path to operation target.
//!
//! Table URLs are `/Tables`, `/Tables('name')`, `/$batch`, `/{table}`,
//! `/{table}()` and `/{table}(PartitionKey='p',RowKey='r')`. The path arrives
//! percent-encoded without the account segment.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `/` (service properties and statistics).
    Root,
    /// `/Tables`
    Tables,
    /// `/Tables('name')`
    TableItem(String),
    /// `/$batch`
    Batch,
    /// `/{table}`
    Table(String),
    /// `/{table}()`
    Query(String),
    /// `/{table}(PartitionKey='..',RowKey='..')`
    Entity { table: String, pk: String, rk: String },
    /// `/{table}/...` or `/{table}/`: the URL goes below a table.
    Nested { table: String, rest: String },
    /// A key predicate that is not `PartitionKey='..',RowKey='..'`.
    BadKeys(String),
}

pub fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = h {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read a `'quoted'` string (`''` is a quote) at the start of `s`; returns it and the rest.
fn quoted(s: &str) -> Option<(String, &str)> {
    let s = s.strip_prefix('\'')?;
    let mut out = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == '\'' {
            if s[i + 1..].starts_with('\'') {
                out.push('\'');
                chars.next();
            } else {
                return Some((out, &s[i + 1..]));
            }
        } else {
            out.push(c);
        }
    }
    None
}

/// `PartitionKey='p',RowKey='r'` in either order, spaces around separators allowed.
fn parse_keys(pred: &str) -> Option<(String, String)> {
    let mut pk = None;
    let mut rk = None;
    let mut rest = pred.trim();
    loop {
        let (name, after) = rest.split_once('=')?;
        let (val, after) = quoted(after.trim_start())?;
        match name.trim() {
            "PartitionKey" if pk.is_none() => pk = Some(val),
            "RowKey" if rk.is_none() => rk = Some(val),
            _ => return None,
        }
        let after = after.trim_start();
        if after.is_empty() {
            break;
        }
        rest = after.strip_prefix(',')?.trim_start();
    }
    Some((pk?, rk?))
}

/// Work out what a raw request path addresses.
pub fn parse(raw_path: &str) -> Target {
    let trimmed = raw_path.strip_prefix('/').unwrap_or(raw_path);
    let (first_raw, rest) = match trimmed.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (trimmed, None),
    };
    let first = pct_decode(first_raw);
    if first.is_empty() {
        return Target::Root;
    }
    // The table name ends at the first '(' or '/'.
    let cut = first.find(['(', '/']).unwrap_or(first.len());
    let name = first[..cut].to_string();
    let predicate = first[cut..].strip_prefix('(').map(|p| p.strip_suffix(')').unwrap_or(p).to_string());
    let malformed = first[cut..].starts_with('(') && !first.ends_with(')');
    if let Some(rest) = rest {
        return Target::Nested { table: name, rest: rest.to_string() };
    }
    if first[cut..].starts_with('/') {
        return Target::Nested { table: name, rest: String::new() };
    }
    if malformed {
        return Target::BadKeys(name);
    }
    match predicate {
        None => {
            if name == "$batch" {
                Target::Batch
            } else if name.eq_ignore_ascii_case("Tables") {
                Target::Tables
            } else {
                Target::Table(name)
            }
        }
        Some(pred) => {
            if name.eq_ignore_ascii_case("Tables") {
                return match quoted(pred.trim()) {
                    Some((t, "")) => Target::TableItem(t),
                    _ => Target::BadKeys(name),
                };
            }
            if pred.trim().is_empty() {
                return Target::Query(name);
            }
            match parse_keys(&pred) {
                Some((pk, rk)) => Target::Entity { table: name, pk, rk },
                None => Target::BadKeys(name),
            }
        }
    }
}

/// `^[A-Za-z][A-Za-z0-9]{2,62}$`, checked in the order Azure reports problems:
/// characters first (`InvalidResourceName`), then length (`OutOfRangeInput`).
pub fn table_name_error(name: &str) -> Option<crate::errors::ApiError> {
    use crate::errors::ApiError;
    let valid_chars = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && name.chars().all(|c| c.is_ascii_alphanumeric());
    if name.is_empty() {
        return Some(ApiError::new(400, "OutOfRangeInput", "The specified resource name length is not within the permissible limits."));
    }
    if !valid_chars {
        return Some(ApiError::new(400, "InvalidResourceName", "The specifed resource name contains invalid characters."));
    }
    if name.eq_ignore_ascii_case("tables") {
        // `/Tables` is the table collection, so a table cannot have that name
        return Some(ApiError::new(400, "InvalidResourceName", "The specifed resource name contains invalid characters."));
    }
    if name.len() < 3 || name.len() > 63 {
        return Some(ApiError::new(400, "OutOfRangeInput", "The specified resource name length is not within the permissible limits."));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(t: &str, pk: &str, rk: &str) -> Target {
        Target::Entity { table: t.into(), pk: pk.into(), rk: rk.into() }
    }

    #[test]
    fn plain_targets() {
        assert_eq!(parse("/"), Target::Root);
        assert_eq!(parse(""), Target::Root);
        assert_eq!(parse("/Tables"), Target::Tables);
        assert_eq!(parse("/$batch"), Target::Batch);
        assert_eq!(parse("/mytable"), Target::Table("mytable".into()));
        assert_eq!(parse("/mytable()"), Target::Query("mytable".into()));
        assert_eq!(parse("/Tables('abc')"), Target::TableItem("abc".into()));
        assert_eq!(parse("/Tables(%27abc%27)"), Target::TableItem("abc".into()));
    }

    #[test]
    fn entity_keys_with_quotes_and_encoding() {
        assert_eq!(parse("/t(PartitionKey='p',RowKey='r')"), entity("t", "p", "r"));
        assert_eq!(parse("/t(PartitionKey=%27O%27%27Brien%27,RowKey=%27a%20b%27)"), entity("t", "O'Brien", "a b"));
        assert_eq!(parse("/t(PartitionKey='',RowKey='')"), entity("t", "", ""));
        assert_eq!(parse("/t(PartitionKey='%E4%BD%A0',RowKey='x')"), entity("t", "你", "x"));
        assert_eq!(parse("/t(PartitionKey='a)b,c',RowKey='(x')"), entity("t", "a)b,c", "(x"));
        assert_eq!(parse("/t(PartitionKey='100%25',RowKey='%2541')"), entity("t", "100%", "%41"));
        assert_eq!(parse("/t(RowKey='r',PartitionKey='p')"), entity("t", "p", "r"));
        assert_eq!(parse("/t(PartitionKey='a%2Fb',RowKey='r')"), entity("t", "a/b", "r"));
    }

    #[test]
    fn odd_urls() {
        assert_eq!(parse("/t/Tables"), Target::Nested { table: "t".into(), rest: "Tables".into() });
        assert_eq!(parse("/t/"), Target::Nested { table: "t".into(), rest: String::new() });
        assert_eq!(parse("/t/$batch"), Target::Nested { table: "t".into(), rest: "$batch".into() });
        assert_eq!(parse("/a%2F%2F"), Target::Nested { table: "a".into(), rest: String::new() });
        assert_eq!(parse("/t(PartitionKey='p')"), Target::BadKeys("t".into()));
        assert_eq!(parse("/t(PartitionKey='p',RowKey='r'"), Target::BadKeys("t".into()));
        assert_eq!(parse("/Tables(abc)"), Target::BadKeys("Tables".into()));
    }

    #[test]
    fn table_names() {
        assert!(table_name_error("abc").is_none());
        assert!(table_name_error(&format!("a{}", "b".repeat(62))).is_none());
        assert_eq!(table_name_error("1table").unwrap().code, "InvalidResourceName");
        assert_eq!(table_name_error("my_table").unwrap().code, "InvalidResourceName");
        assert_eq!(table_name_error("aa").unwrap().code, "OutOfRangeInput");
        assert_eq!(table_name_error(&"a".repeat(64)).unwrap().code, "OutOfRangeInput");
        assert_eq!(table_name_error("").unwrap().code, "OutOfRangeInput");
        assert_eq!(table_name_error("Tables").unwrap().code, "InvalidResourceName");
        assert_eq!(table_name_error("tables").unwrap().code, "InvalidResourceName");
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(pct_decode("a%20b%2"), "a b%2");
        assert_eq!(pct_decode("%zz"), "%zz");
        assert_eq!(pct_decode("100%"), "100%");
    }
}
