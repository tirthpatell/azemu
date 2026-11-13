//! Shared Key and Shared Key Lite string-to-sign for Blob and Queue.
//!
//! The service's rules leave room, and clients differ, so [`candidates`] returns every
//! string a correct client may have signed, the most likely one first:
//!
//! - `Content-Length: 0` is signed as an empty string from version 2015-02-21 on;
//!   a request without `x-ms-version` may have either form.
//! - A `Content-Length` the client did not sign (added by the HTTP stack), a `Range`
//!   the client left out (the Python SDK signs no `Range`), and a `Date` next to
//!   `x-ms-date` are tried both ways.
//! - The resource path is the one on the wire: percent-encoded as sent. The decoded
//!   path and a freshly encoded path are tried too.
//! - With a path-style endpoint (`http://127.0.0.1:10000/acct/container`) the
//!   client signs the URL path as it is, so the account name appears twice:
//!   `/acct/acct/container`.
//! - Query values keep `+` literally (Python, .NET) or read it as a space (JavaScript).
//! - `x-ms-*` headers are sorted the way the service sorts them (hyphens ignored first,
//!   as the Python SDK emulates) or by plain byte order.

use std::collections::BTreeMap;

use http::HeaderMap;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

use crate::Request;

/// Everything but unreserved characters and `/` is escaped.
const PATH_SET: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~').remove(b'/');

/// `(account, signature, lite)` from `Authorization: SharedKey account:signature`.
pub fn parse_authorization(req: &Request) -> Option<(String, String, bool)> {
    let a = req.header("authorization")?.trim();
    let (scheme, rest) = a.split_once(' ')?;
    let lite = scheme.eq_ignore_ascii_case("sharedkeylite");
    if !lite && !scheme.eq_ignore_ascii_case("sharedkey") {
        return None;
    }
    let (account, sig) = rest.trim().split_once(':')?;
    Some((account.to_string(), sig.trim().to_string(), lite))
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}

/// How the bytes of a header value on the wire become the characters that were signed.
///
/// HTTP does not say. The Python and JavaScript SDKs sign the Unicode string and put each
/// character up to U+00FF on the wire as one Latin-1 byte (they cannot send more), the Go
/// SDK signs and sends UTF-8. Either way the signature is over the UTF-8 bytes of the
/// characters, so verification decodes the wire bytes both ways and tries both.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wire {
    Utf8,
    Latin1,
}

fn decode(v: &http::HeaderValue, wire: Wire) -> String {
    let b = v.as_bytes();
    let s = match wire {
        Wire::Utf8 => String::from_utf8_lossy(b).into_owned(),
        Wire::Latin1 => b.iter().map(|&c| c as char).collect(),
    };
    s.trim().to_string()
}

fn header_all(h: &HeaderMap, name: &str, wire: Wire) -> String {
    h.get_all(name).iter().map(|v| decode(v, wire)).collect::<Vec<_>>().join(",")
}

// ---- header ordering ------------------------------------------------------

#[rustfmt::skip]
const LV0: [u16; 128] = [
    0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0x71c,0,0x71f,0x721,0x723,0x725,
    0,0,0,0x72d,0x803,0,0,0x733,0,0xd03,0xd1a,0xd1c,0xd1e,
    0xd20,0xd22,0xd24,0xd26,0xd28,0xd2a,0,0,0,0,0,0,0,
    0xe02,0xe09,0xe0a,0xe1a,0xe21,0xe23,0xe25,0xe2c,0xe32,0xe35,0xe36,0xe48,0xe51,
    0xe70,0xe7c,0xe7e,0xe89,0xe8a,0xe91,0xe99,0xe9f,0xea2,0xea4,0xea6,0xea7,0xea9,
    0,0,0,0x743,0x744,0x748,0xe02,0xe09,0xe0a,0xe1a,0xe21,0xe23,0xe25,
    0xe2c,0xe32,0xe35,0xe36,0xe48,0xe51,0xe70,0xe7c,0xe7e,0xe89,0xe8a,0xe91,0xe99,
    0xe9f,0xea2,0xea4,0xea6,0xea7,0xea9,0,0x74c,0,0x750,0,
];

fn lv4(c: u8) -> u16 {
    match c {
        39 => 0x8012,
        45 => 0x8212,
        _ => 0,
    }
}

/// The service's culture-aware header name comparison, as the Python SDK emulates it.
/// `None` when a name has a non-ASCII byte.
fn service_cmp(lhs: &str, rhs: &str) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering::*;
    let (l, r) = (lhs.as_bytes(), rhs.as_bytes());
    if !lhs.is_ascii() || !rhs.is_ascii() {
        return None;
    }
    let weight = |level: usize, s: &[u8], i: usize| -> u16 {
        if i < s.len() {
            if level == 0 {
                LV0[s[i] as usize]
            } else {
                lv4(s[i])
            }
        } else {
            1
        }
    };
    let (mut level, mut i, mut j) = (0usize, 0usize, 0usize);
    while level < 2 {
        if level == 1 && i != j {
            return Some(if i > j { Less } else { Greater });
        }
        let w1 = weight(level, l, i);
        let w2 = weight(level, r, j);
        if w1 == 1 && w2 == 1 {
            i = 0;
            j = 0;
            level += 1;
        } else if w1 == w2 {
            i += 1;
            j += 1;
        } else if w1 == 0 {
            i += 1;
        } else if w2 == 0 {
            j += 1;
        } else {
            return Some(if w1 < w2 { Less } else { Greater });
        }
    }
    Some(Equal)
}

fn canonical_headers(h: &HeaderMap, wire: Wire) -> Vec<String> {
    let mut items: Vec<(String, String)> = Vec::new();
    for name in h.keys() {
        let n = name.as_str().to_ascii_lowercase();
        if n.starts_with("x-ms-") && !items.iter().any(|(k, _)| *k == n) {
            items.push((n.clone(), header_all(h, &n, wire)));
        }
    }
    let render = |v: &[(String, String)]| v.iter().map(|(k, val)| format!("{k}:{val}\n")).collect::<String>();
    let mut ordinal = items.clone();
    ordinal.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    let mut service = items;
    if service.iter().all(|(k, _)| k.is_ascii()) {
        service.sort_by(|a, b| service_cmp(&a.0, &b.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.0.cmp(&b.0)));
        push_unique(&mut out, render(&service));
    }
    push_unique(&mut out, render(&ordinal));
    out
}

// ---- resource ---------------------------------------------------------------

/// The URL path as the client signed it, including the account segment on a
/// path-style endpoint. Several spellings, most likely first.
fn signed_paths(req: &Request) -> Vec<String> {
    let mut bases: Vec<String> = Vec::new();
    if req.ctx.endpoint.path_style {
        let acct = &req.account;
        if req.path == "/" {
            bases.push(format!("/{acct}"));
            bases.push(format!("/{acct}/"));
        } else {
            bases.push(format!("/{acct}{}", req.path));
        }
    } else {
        bases.push(req.path.clone());
    }
    let mut out = Vec::new();
    for b in bases {
        let decoded = percent_decode_str(&b).decode_utf8_lossy().into_owned();
        let encoded = utf8_percent_encode(&decoded, PATH_SET).to_string();
        push_unique(&mut out, b);
        push_unique(&mut out, decoded);
        push_unique(&mut out, encoded);
    }
    out
}

/// `\nname:value` lines, names lower-cased and sorted, several values joined with `,`.
fn query_part(pairs: &[(String, String)]) -> String {
    let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in pairs {
        m.entry(k.to_ascii_lowercase()).or_default().push(v.clone());
    }
    let mut s = String::new();
    for (k, mut vals) in m {
        vals.sort();
        s.push('\n');
        s.push_str(&k);
        s.push(':');
        s.push_str(&vals.join(","));
    }
    s
}

fn lite_query_part(req: &Request) -> String {
    match req.query.get("comp") {
        Some(c) => format!("?comp={c}"),
        None => String::new(),
    }
}

// ---- strings to sign ----------------------------------------------------------

/// Every string a correct client may have signed. `account` is the name from the
/// `Authorization` header. The first entry is the primary reading, the one shown in
/// error messages.
pub fn candidates(req: &Request, account: &str, lite: bool) -> Vec<String> {
    let mut out = candidates_for(req, account, lite, Wire::Utf8);
    // only when a header value has bytes above 0x7F do the two readings differ
    if req.headers.values().any(|v| !v.as_bytes().is_ascii()) {
        for s in candidates_for(req, account, lite, Wire::Latin1) {
            push_unique(&mut out, s);
        }
    }
    out
}

fn candidates_for(req: &Request, account: &str, lite: bool, wire: Wire) -> Vec<String> {
    let h = &req.headers;
    let verb = req.method.as_str().to_ascii_uppercase();
    let version = req.header("x-ms-version").unwrap_or("");

    // Content-Length options
    let cl = header_all(h, "content-length", wire);
    let mut cl_opts: Vec<String> = Vec::new();
    if cl == "0" {
        let old = !version.is_empty() && version < "2015-02-21";
        if old {
            cl_opts.push("0".into());
            cl_opts.push(String::new());
        } else {
            cl_opts.push(String::new());
            cl_opts.push("0".into());
        }
    } else {
        cl_opts.push(cl.clone());
        if !cl.is_empty() {
            cl_opts.push(String::new());
        }
    }
    // Date options
    let date = header_all(h, "date", wire);
    let mut date_opts = vec![date.clone()];
    if !date.is_empty() && h.contains_key("x-ms-date") {
        date_opts.push(String::new());
    }
    // Range options
    let range = header_all(h, "range", wire);
    let mut range_opts = vec![range.clone()];
    if !range.is_empty() {
        range_opts.push(String::new());
    }

    let mut heads: Vec<String> = Vec::new();
    for rg in &range_opts {
        for dt in &date_opts {
            for c in &cl_opts {
                let s = if lite {
                    format!("{verb}\n{}\n{}\n{dt}\n", header_all(h, "content-md5", wire), header_all(h, "content-type", wire))
                } else {
                    format!(
                        "{verb}\n{}\n{}\n{c}\n{}\n{}\n{dt}\n{}\n{}\n{}\n{}\n{rg}\n",
                        header_all(h, "content-encoding", wire),
                        header_all(h, "content-language", wire),
                        header_all(h, "content-md5", wire),
                        header_all(h, "content-type", wire),
                        header_all(h, "if-modified-since", wire),
                        header_all(h, "if-match", wire),
                        header_all(h, "if-none-match", wire),
                        header_all(h, "if-unmodified-since", wire),
                    )
                };
                push_unique(&mut heads, s);
            }
        }
    }
    let canon_headers = canonical_headers(h, wire);
    let paths = signed_paths(req);
    let mut accounts = vec![account.to_string()];
    push_unique(&mut accounts, account.to_ascii_lowercase());
    // `+` in the query is a space (`Query::parse`), which is what the Rust SDKs sign. A client that
    // sends a raw `+` but signed it as a plus (the Python and JavaScript SDKs sign the percent-decoded
    // value, and never send a raw `+`) is also accepted.
    let literal_plus = req.query.raw().contains('+');

    let mut out: Vec<String> = Vec::new();
    for a in &accounts {
        for p in &paths {
            let mut queries = Vec::new();
            if lite {
                queries.push(lite_query_part(req));
            } else {
                queries.push(query_part(req.query.pairs()));
                if literal_plus {
                    queries.push(query_part(&req.query.pairs_plus_literal()));
                }
            }
            for q in &queries {
                for ch in &canon_headers {
                    for hd in &heads {
                        push_unique(&mut out, format!("{hd}{ch}/{a}{p}{q}"));
                        if out.len() >= 512 {
                            return out;
                        }
                    }
                }
            }
        }
    }
    out
}

/// Every string a correct client may have signed for the Table service.
///
/// Table differs from Blob and Queue (Microsoft Learn, "Authorize with Shared Key"):
///
/// - Shared Key: `VERB \n Content-MD5 \n Content-Type \n Date \n CanonicalizedResource`, no
///   canonicalized headers. The `Date` line is never empty: with `x-ms-date` on the request it
///   holds that value.
/// - Shared Key Lite: `Date \n CanonicalizedResource`, with the same `Date` rule.
/// - The resource is `/account` plus the encoded URL path, plus `?comp=value` when the
///   query has `comp`. No other query parameter is signed.
///
/// `Date` from the `Date` header is tried too, in case a client signs that one when it sends both.
pub fn table_candidates(req: &Request, account: &str, lite: bool) -> Vec<String> {
    let h = &req.headers;
    let verb = req.method.as_str().to_ascii_uppercase();
    let xdate = header_all(h, "x-ms-date", Wire::Utf8);
    let date = header_all(h, "date", Wire::Utf8);
    let mut dates = Vec::new();
    for d in [xdate, date] {
        if !d.is_empty() {
            push_unique(&mut dates, d);
        }
    }
    if dates.is_empty() {
        dates.push(String::new());
    }
    let md5 = header_all(h, "content-md5", Wire::Utf8);
    let ctype = header_all(h, "content-type", Wire::Utf8);
    let query = lite_query_part(req);
    let mut accounts = vec![account.to_string()];
    push_unique(&mut accounts, account.to_ascii_lowercase());
    let mut out = Vec::new();
    for a in &accounts {
        for p in signed_paths(req) {
            for d in &dates {
                let s = if lite { format!("{d}\n/{a}{p}{query}") } else { format!("{verb}\n{md5}\n{ctype}\n{d}\n/{a}{p}{query}") };
                push_unique(&mut out, s);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_sort_ignores_hyphens_first() {
        use std::cmp::Ordering::*;
        // plain byte order puts "x-ms-a-b" before "x-ms-ab" ('-' < 'b'); the service
        // ignores the hyphen first and then orders by its position
        assert_eq!(service_cmp("x-ms-blob-type", "x-ms-blob-type"), Some(Equal));
        assert_eq!(service_cmp("x-ms-date", "x-ms-version"), Some(Less));
        assert_eq!(service_cmp("x-ms-meta-a", "x-ms-meta-b"), Some(Less));
        assert_eq!(service_cmp("x-ms-meta-ab", "x-ms-meta-a-b"), Some(Less));
        assert_eq!(service_cmp("x-ms-meta-a-b", "x-ms-meta-ab"), Some(Greater));
    }
}
