//! Request types handed to services.

use bytes::Bytes;
use http::{HeaderMap, Method};
use percent_encoding::percent_decode_str;

/// A fully buffered HTTP request, already routed to a service.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    /// Raw path, still percent-encoded, account segment removed. Starts with "/".
    pub path: String,
    pub query: Query,
    pub headers: HeaderMap,
    pub body: Bytes,
    /// Account / vault / namespace / store name. "" when mounted without one.
    pub account: String,
    pub ctx: RequestCtx,
}

impl Request {
    /// Case-insensitive header lookup. `None` if absent or not valid UTF-8.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Path segments, percent-decoded, empty segments dropped.
    pub fn segments(&self) -> Vec<String> {
        self.path.split('/').filter(|s| !s.is_empty()).map(decode).collect()
    }

    /// The path, percent-decoded (invalid UTF-8 becomes U+FFFD).
    pub fn decoded_path(&self) -> String {
        decode(&self.path)
    }
}

fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// Decode one name or value of a query string: `+` is a space, `%2B` is a plus (form decoding, what
/// ASP.NET, Express and the `url` crate do). The `+` is replaced before the percent escapes are decoded, so
/// a `%2B` stays a plus.
fn decode_query_part(s: &str, plus_is_space: bool) -> String {
    if plus_is_space && s.contains('+') {
        decode(&s.replace('+', " "))
    } else {
        decode(s)
    }
}

fn split_pairs(raw: &str, plus_is_space: bool) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (decode_query_part(k, plus_is_space), decode_query_part(v, plus_is_space)),
            None => (decode_query_part(p, plus_is_space), String::new()),
        })
        .collect()
}

/// Query string: ordered, percent-decoded pairs plus the raw string.
///
/// In a query string `+` is a space and a plus must be sent as `%2B`, the way the Rust SDKs (form encoding), ASP.NET and
/// Express read it. (Paths are different: in a path `+` is always a plus and `Request::segments` leaves it alone.)
/// `a=1&b=x%20y&c=x+y&d=1%2B1&flag` gives
/// `[("a","1"),("b","x y"),("c","x y"),("d","1+1"),("flag","")]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    raw: String,
    pairs: Vec<(String, String)>,
}

impl Query {
    /// Parse a query string (without the leading `?`).
    pub fn parse(raw: &str) -> Query {
        Query { raw: raw.to_string(), pairs: split_pairs(raw, true) }
    }

    /// The pairs with `+` kept as a plus (only `%XX` decoded). For code that has to try how a client
    /// that does not encode a plus would have signed the request (Shared Key checks); nothing else
    /// should read a query this way.
    pub fn pairs_plus_literal(&self) -> Vec<(String, String)> {
        split_pairs(&self.raw, false)
    }

    /// First value for `name` (name match is case-insensitive).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.pairs.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Whether `name` is present, even with an empty value.
    pub fn has(&self, name: &str) -> bool {
        self.pairs.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
    }

    /// All values for `name`, in order.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.pairs.iter().filter(move |(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }

    /// The original string, without the leading `?`.
    pub fn raw(&self) -> &str {
        &self.raw
    }
}

/// Per-request context created by core.
#[derive(Debug, Clone)]
pub struct RequestCtx {
    /// Fresh UUID v4 per request.
    pub request_id: String,
    /// When the request arrived (UTC).
    pub time: time::OffsetDateTime,
    pub endpoint: Endpoint,
}

impl RequestCtx {
    /// The cancellation token of this request: cancelled when the client goes away before the answer is
    /// complete (see [`crate::cancel`]). Call it in the handler, in the task core started for it, and pass the
    /// token to work that runs on another thread. Outside a request (a unit test that calls the service
    /// directly) the token is never cancelled.
    ///
    /// The token is not a field of `RequestCtx` on purpose: services and their tests build a `RequestCtx`
    /// with a struct literal, and a new field would break every one of them.
    pub fn cancel_token(&self) -> crate::cancel::CancelToken {
        crate::cancel::current()
    }
}

/// How the client reached the emulator.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// "http" or "https".
    pub scheme: &'static str,
    /// Host header as sent, with port.
    pub authority: String,
    /// The account came from the path.
    pub path_style: bool,
    pub account: String,
}

impl Endpoint {
    /// URL the client used to reach this account, no trailing slash.
    ///
    /// host style: `http://acct.blob.localhost:4577`;
    /// path style: `http://127.0.0.1:10000/acct`.
    /// Use this for every URL returned to the client. Never invent hostnames.
    pub fn base_url(&self) -> String {
        if self.path_style {
            format!("{}://{}/{}", self.scheme, self.authority, self.account)
        } else {
            format!("{}://{}", self.scheme, self.authority)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parse_basic() {
        let q = Query::parse("a=1&b=x%20y&flag");
        assert_eq!(
            q.pairs(),
            &[("a".to_string(), "1".to_string()), ("b".to_string(), "x y".to_string()), ("flag".to_string(), String::new())]
        );
        assert_eq!(q.raw(), "a=1&b=x%20y&flag");
        assert_eq!(q.get("a"), Some("1"));
        assert_eq!(q.get("B"), Some("x y"));
        assert_eq!(q.get("flag"), Some(""));
        assert!(q.has("FLAG"));
        assert!(!q.has("nope"));
        assert_eq!(q.get("nope"), None);
    }

    #[test]
    fn query_plus_is_a_space_and_percent_2b_is_a_plus() {
        let q = Query::parse("sig=ab+cd%2Bef&x=a%26b");
        assert_eq!(q.get("sig"), Some("ab cd+ef"));
        assert_eq!(q.get("x"), Some("a&b"));
        // space as `+`, as `%20`, a literal plus as `%2B`, in names and values
        let q = Query::parse("%24filter=age+ge+7&prefix=dir%20with%20space%2F&p=1%2B1&a+b=c+d&e=%2b");
        assert_eq!(q.get("$filter"), Some("age ge 7"));
        assert_eq!(q.get("prefix"), Some("dir with space/"));
        assert_eq!(q.get("p"), Some("1+1"));
        assert_eq!(q.get("a b"), Some("c d"));
        assert_eq!(q.get("e"), Some("+"));
        // flags have no value; the raw string is untouched
        let q = Query::parse("comp=list&x+y");
        assert!(q.has("x y"));
        assert_eq!(q.raw(), "comp=list&x+y");
        // a trailing plus, only pluses, an encoded percent next to a plus
        assert_eq!(Query::parse("a=+").get("a"), Some(" "));
        assert_eq!(Query::parse("a=%25+%2B").get("a"), Some("% +"));
        // the literal-plus reading, for signature checks
        let q = Query::parse("a=x+y&b=1%2B1");
        assert_eq!(q.pairs_plus_literal(), vec![("a".to_string(), "x+y".to_string()), ("b".to_string(), "1+1".to_string())]);
    }

    #[test]
    fn query_edge_cases() {
        let q = Query::parse("");
        assert!(q.pairs().is_empty());
        assert_eq!(q.raw(), "");
        let q = Query::parse("&&a=1&&a=2&=v&k=");
        assert_eq!(q.get("a"), Some("1"));
        assert_eq!(q.get_all("A").collect::<Vec<_>>(), vec!["1", "2"]);
        assert_eq!(q.get(""), Some("v"));
        assert_eq!(q.get("k"), Some(""));
        let q = Query::parse("a=b=c");
        assert_eq!(q.get("a"), Some("b=c"));
        // invalid UTF-8 does not panic
        let q = Query::parse("a=%FF%FE");
        assert!(q.get("a").is_some());
    }

    fn req(path: &str) -> Request {
        Request {
            method: Method::GET,
            path: path.to_string(),
            query: Query::default(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
            account: String::new(),
            ctx: RequestCtx {
                request_id: "x".into(),
                time: time::OffsetDateTime::UNIX_EPOCH,
                endpoint: Endpoint { scheme: "http", authority: "h".into(), path_style: false, account: String::new() },
            },
        }
    }

    #[test]
    fn segments_and_decoded_path() {
        let r = req("/c1//a%2Fb/d%20e/");
        assert_eq!(r.segments(), vec!["c1", "a/b", "d e"]);
        assert_eq!(r.decoded_path(), "/c1//a/b/d e/");
        assert!(req("/").segments().is_empty());
    }

    #[test]
    fn header_lookup() {
        let mut r = req("/");
        r.headers.insert("x-ms-version", http::HeaderValue::from_static("2020"));
        r.headers.insert("bad", http::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap());
        assert_eq!(r.header("X-MS-Version"), Some("2020"));
        assert_eq!(r.header("bad"), None);
        assert_eq!(r.header("absent"), None);
    }

    #[test]
    fn base_url_forms() {
        let mut e = Endpoint { scheme: "http", authority: "acct.blob.localhost:4577".into(), path_style: false, account: "acct".into() };
        assert_eq!(e.base_url(), "http://acct.blob.localhost:4577");
        e.path_style = true;
        e.authority = "127.0.0.1:10000".into();
        e.scheme = "https";
        assert_eq!(e.base_url(), "https://127.0.0.1:10000/acct");
    }
}
