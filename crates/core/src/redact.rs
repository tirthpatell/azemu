//! Keeping credentials out of the log.
//!
//! A client that uses `DefaultAzureCredential` without a local identity endpoint sends a real Entra ID token to the
//! emulator, and CI keeps logs. So no log line, at any level, may carry the value of a credential. Core's own log
//! lines use these functions, and a service that logs a header or a query string must too.

use http::HeaderMap;

/// What replaces a credential in the log.
pub const REDACTED: &str = "<redacted>";

/// Header names (lower case) whose values are credentials.
const SECRET_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-ms-copy-source-authorization",
    "x-ms-encryption-key",
    "x-ms-source-encryption-key",
    "aeg-sas-key",
    "aeg-sas-token",
    "servicebussupplementaryauthorization",
    "servicebusdlqsupplementaryauthorization",
    "cookie",
    "set-cookie",
];

/// Query parameter names (lower case) whose values are credentials: the SAS signature (`sig`), a Function key
/// (`code`) and the usual OAuth ones.
const SECRET_QUERY: &[&str] =
    &["sig", "code", "access_token", "refresh_token", "client_secret", "client_assertion", "assertion", "password"];

/// Is the value of this header a credential?
pub fn is_secret_header(name: &str) -> bool {
    SECRET_HEADERS.iter().any(|s| s.eq_ignore_ascii_case(name))
}

/// Is the value of this query parameter a credential?
pub fn is_secret_query(name: &str) -> bool {
    SECRET_QUERY.iter().any(|s| s.eq_ignore_ascii_case(name))
}

/// The headers as text for a log line (`{"name": "value", ...}`), with credentials replaced.
pub fn headers(h: &HeaderMap) -> String {
    let mut out = String::from("{");
    for (i, (k, v)) in h.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let shown = if is_secret_header(k.as_str()) { REDACTED.to_string() } else { String::from_utf8_lossy(v.as_bytes()).into_owned() };
        out.push_str(&format!("{:?}: {:?}", k.as_str(), shown));
    }
    out.push('}');
    out
}

/// A raw query string (without the `?`) for a log line, with the values of credentials replaced. The names are
/// compared after percent-decoding, so `s%69g=` does not get past it.
pub fn query(raw: &str) -> String {
    raw.split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, _)) if is_secret_query(&decode(k)) => format!("{k}={REDACTED}"),
            None if is_secret_query(&decode(pair)) => format!("{pair}={REDACTED}"),
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(&s.replace('+', " ")).decode_utf8_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_replaced_in_headers_and_queries() {
        let mut h = HeaderMap::new();
        h.insert("Authorization", "Bearer eyJ-real-token".parse().unwrap());
        h.insert("x-ms-version", "2021-08-06".parse().unwrap());
        h.insert("AEG-SAS-Key", "k3y".parse().unwrap());
        let s = headers(&h);
        assert!(!s.contains("eyJ-real-token") && !s.contains("k3y"), "{s}");
        assert!(s.contains("2021-08-06") && s.contains(REDACTED), "{s}");
        let q = query("restype=container&sig=SECRETSIG&sv=2021&S%69G=more&code=abc&flag");
        assert_eq!(q, format!("restype=container&sig={REDACTED}&sv=2021&S%69G={REDACTED}&code={REDACTED}&flag"));
    }
}
