//! Parsing of `multipart/mixed` batch requests.
//!
//! A batch is one outer multipart body holding either a single request (a
//! query) or one change set, which is itself a multipart body of
//! `application/http` parts, each an embedded HTTP request.

/// One embedded HTTP request.
#[derive(Debug, Clone, PartialEq)]
pub struct Inner {
    pub method: String,
    /// Path and query as written in the request line (scheme and host removed).
    pub target: String,
    /// Header names are lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Inner {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    ChangeSet(Vec<Inner>),
    Single(Inner),
}

/// The `boundary` parameter of a `multipart/mixed` content type.
pub fn boundary(content_type: &str) -> Option<String> {
    let mut parts = content_type.split(';').map(str::trim);
    if !parts.next()?.eq_ignore_ascii_case("multipart/mixed") {
        return None;
    }
    parts.find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("boundary").then(|| v.trim().trim_matches('"').to_string())
    })
}

/// Split a multipart body into its parts (raw text between delimiter lines).
/// A delimiter is a line that is exactly `--boundary` (or `--boundary--` to
/// close), so the same characters inside a request line or body do not count.
fn split<'a>(body: &'a str, boundary: &str) -> Vec<&'a str> {
    let delim = format!("--{boundary}");
    // (offset where the delimiter line starts, offset just after it, closing?)
    let mut marks: Vec<(usize, usize, bool)> = Vec::new();
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix(delim.as_str()) {
            let closing = rest.starts_with("--");
            let tail = if closing { &rest[2..] } else { rest };
            if tail.trim().is_empty() {
                marks.push((offset, offset + line.len(), closing));
                if closing {
                    break;
                }
            }
        }
        offset += line.len();
    }
    let mut out = Vec::new();
    for pair in marks.windows(2) {
        let part = &body[pair[0].1..pair[1].0];
        let part = part.strip_suffix("\r\n").or_else(|| part.strip_suffix('\n')).unwrap_or(part);
        out.push(part);
    }
    out
}

/// Separate a header block from what follows it at the first empty line.
fn split_head(text: &str) -> (&str, &str) {
    if let Some(i) = text.find("\r\n\r\n") {
        return (&text[..i], &text[i + 4..]);
    }
    if let Some(i) = text.find("\n\n") {
        return (&text[..i], &text[i + 2..]);
    }
    (text, "")
}

fn headers_of(head: &str) -> Vec<(String, String)> {
    head.lines().filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect()
}

/// Strip `scheme://host` from an absolute request URL.
fn target_of(url: &str) -> String {
    match url.split_once("://") {
        Some((_, rest)) => match rest.find('/') {
            Some(i) => rest[i..].to_string(),
            None => "/".to_string(),
        },
        None => url.to_string(),
    }
}

fn parse_http(text: &str) -> Option<Inner> {
    let text = text.trim_start_matches(['\r', '\n']);
    let (head, body) = split_head(text);
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut it = request_line.split_whitespace();
    let method = it.next()?.to_ascii_uppercase();
    let url = it.next()?;
    let rest: Vec<&str> = lines.collect();
    Some(Inner { method, target: target_of(url), headers: headers_of(&rest.join("\n")), body: body.to_string() })
}

/// Parse a batch request body. `Err` carries nothing: the caller answers 400.
pub fn parse(content_type: &str, body: &str) -> Result<Vec<Part>, ()> {
    let outer = boundary(content_type).ok_or(())?;
    let mut parts = Vec::new();
    for raw in split(body, &outer) {
        let (head, rest) = split_head(raw);
        let headers = headers_of(head);
        let ct = headers.iter().find(|(k, _)| k == "content-type").map(|(_, v)| v.as_str()).unwrap_or("");
        if let Some(inner_boundary) = boundary(ct) {
            let mut ops = Vec::new();
            for p in split(rest, &inner_boundary) {
                let (_, http) = split_head(p);
                ops.push(parse_http(http).ok_or(())?);
            }
            parts.push(Part::ChangeSet(ops));
        } else {
            parts.push(Part::Single(parse_http(rest).ok_or(())?));
        }
    }
    if parts.is_empty() {
        return Err(());
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = "--batch_1\r\nContent-Type: multipart/mixed; boundary=changeset_2\r\n\r\n--changeset_2\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\nContent-ID: 0\r\n\r\nPOST https://acct.table.core.windows.net/mytable HTTP/1.1\r\nContent-Type: application/json\r\nAccept: application/json;odata=minimalmetadata\r\n\r\n{\"PartitionKey\": \"p\", \"RowKey\": \"r\"}\r\n--changeset_2\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\nContent-ID: 1\r\n\r\nDELETE https://acct.table.core.windows.net/mytable(PartitionKey='p',RowKey='x') HTTP/1.1\r\nIf-Match: *\r\n\r\n\r\n--changeset_2--\r\n\r\n--batch_1--\r\n";

    #[test]
    fn parses_a_change_set() {
        let parts = parse("multipart/mixed; boundary=batch_1", BODY).unwrap();
        assert_eq!(parts.len(), 1);
        let Part::ChangeSet(ops) = &parts[0] else { panic!("expected a change set") };
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].method, "POST");
        assert_eq!(ops[0].target, "/mytable");
        assert_eq!(ops[0].header("accept"), Some("application/json;odata=minimalmetadata"));
        assert_eq!(ops[0].body.trim(), "{\"PartitionKey\": \"p\", \"RowKey\": \"r\"}");
        assert_eq!(ops[1].method, "DELETE");
        assert_eq!(ops[1].target, "/mytable(PartitionKey='p',RowKey='x')");
        assert_eq!(ops[1].header("if-match"), Some("*"));
        assert_eq!(ops[1].body.trim(), "");
    }

    #[test]
    fn parses_a_single_query() {
        let body = "--batch_9\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\n\r\nGET http://127.0.0.1:10002/acct/t(PartitionKey='a',RowKey='b') HTTP/1.1\r\nAccept: application/json\r\n\r\n\r\n--batch_9--\r\n";
        let parts = parse("multipart/mixed;boundary=batch_9", body).unwrap();
        let [Part::Single(i)] = parts.as_slice() else { panic!() };
        assert_eq!(i.method, "GET");
        assert_eq!(i.target, "/acct/t(PartitionKey='a',RowKey='b')");
    }

    #[test]
    fn lf_only_line_endings_work() {
        let body = BODY.replace("\r\n", "\n");
        let parts = parse("multipart/mixed; boundary=batch_1", &body).unwrap();
        let Part::ChangeSet(ops) = &parts[0] else { panic!() };
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("application/json", BODY).is_err());
        assert!(parse("multipart/mixed", BODY).is_err());
        assert!(parse("multipart/mixed; boundary=zzz", BODY).is_err());
        assert!(parse("multipart/mixed; boundary=batch_1", "--batch_1\r\n\r\nnot http\r\n--batch_1--").is_err());
    }

    #[test]
    fn boundary_parameter() {
        assert_eq!(boundary("multipart/mixed; boundary=\"b 1\""), Some("b 1".into()));
        assert_eq!(boundary("Multipart/Mixed;Boundary=x"), Some("x".into()));
        assert_eq!(boundary("text/plain; boundary=x"), None);
    }

    #[test]
    fn body_may_hold_newlines_dashes_and_query_strings() {
        let body = "preamble ignored\r\n--b\r\nContent-Type: multipart/mixed; boundary=c\r\n\r\n--c\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\n\r\nPUT https://h/t(PartitionKey='a--b',RowKey='x')?timeout=5 HTTP/1.1\r\nContent-Type: application/json\r\nIf-Match:   W/\"datetime'2020-01-01T00%3A00%3A00Z'\"  \r\n\r\n{\r\n  \"s\": \"line1\\nline2 -- dashes\"\r\n}\r\n--c--\r\n--b--\r\nepilogue ignored";
        let parts = parse("multipart/mixed; boundary=b", body).unwrap();
        let [Part::ChangeSet(ops)] = parts.as_slice() else { panic!("{parts:?}") };
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].target, "/t(PartitionKey='a--b',RowKey='x')?timeout=5");
        assert_eq!(ops[0].header("if-match"), Some("W/\"datetime'2020-01-01T00%3A00%3A00Z'\""));
        assert!(ops[0].body.contains("line2 -- dashes"));
        assert!(crate::json::parse(&ops[0].body).is_ok());
    }

    #[test]
    fn several_parts_and_an_empty_change_set() {
        let body = "--b\r\nContent-Type: multipart/mixed; boundary=c\r\n\r\n--c--\r\n--b\r\nContent-Type: application/http\r\n\r\nGET http://h NOPE\r\n\r\n\r\n--b--\r\n";
        let parts = parse("multipart/mixed; boundary=b", body).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], Part::ChangeSet(vec![]));
        let Part::Single(i) = &parts[1] else { panic!() };
        assert_eq!((i.method.as_str(), i.target.as_str()), ("GET", "/"));
    }

    #[test]
    fn method_is_upper_cased_and_headers_lower_cased() {
        let body = "--b\r\nContent-Type: application/http\r\n\r\nmerge /t(PartitionKey='a',RowKey='b') HTTP/1.1\r\nX-Custom-Header: V\r\n\r\n\r\n--b--";
        let Part::Single(i) = &parse("multipart/mixed; boundary=b", body).unwrap()[0] else { panic!() };
        assert_eq!(i.method, "MERGE");
        assert_eq!(i.header("x-custom-header"), Some("V"));
    }
}
