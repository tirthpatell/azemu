//! What Azure Storage checks about the credentials of a request before it looks at any key, so that Blob, Queue and
//! Table answer alike: the form of `x-ms-version`, of the `Authorization` header and of a signature, the age of the
//! request date, the `sv` and `sip` of a SAS, and what a request with no credentials is told.
//!
//! Every rule here was observed on real Azure in November 2026 (a general-purpose v2 account; the probes are described in
//! the service pages). None of them needs a key, so they hold for every account name. `AZEMU_LOOSE_AUTH=1` turns off the
//! ones about signatures and dates (see `sig`), because recorded traffic carries placeholder signatures and old dates.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use time::OffsetDateTime;

use crate::time::parse_rfc1123;
use crate::Request;

/// The `WWW-Authenticate` challenge of a request with no credentials (the tenant is not known to an emulator).
pub const CHALLENGE: &str =
    "Bearer authorization_uri=https://login.microsoftonline.com/common/oauth2/authorize resource_id=https://storage.azure.com";

/// The oldest `x-ms-version` Azure accepts (2008-10-27 is the first version of the storage services).
pub const OLDEST_VERSION: &str = "2008-10-27";

/// The newest `x-ms-version` and `sv` azemu accepts. Azure takes only the versions it has published (it refuses
/// 2026-11-14, which lies between two of them); azemu does not carry that list, so that a newer SDK is not refused
/// for being newer than the emulator. A date a year or more past the newest known version is refused.
pub const NEWEST_VERSION: &str = "2027-12-31";

/// The oldest `sv` a SAS may carry.
pub const OLDEST_SAS_VERSION: &str = "2012-02-12";

/// How old (in seconds) the date of a Shared Key request may be. Azure refuses an older one; a date in the future is
/// not refused at any distance (tried up to 30 days).
pub const MAX_DATE_AGE_SECS: i64 = 15 * 60;

/// `YYYY-MM-DD` that is a possible calendar date.
pub fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || !b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() }) {
        return false;
    }
    let (month, day): (u8, u8) = (s[5..7].parse().unwrap_or(0), s[8..10].parse().unwrap_or(0));
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// The value of `x-ms-version` when Azure refuses it (400 `InvalidHeaderValue`): not a date, older than the first
/// version or newer than [`NEWEST_VERSION`]. `None` when it is fine or not sent.
pub fn refused_version(req: &Request) -> Option<&str> {
    let v = req.header("x-ms-version")?.trim();
    (!(is_date(v) && (OLDEST_VERSION..=NEWEST_VERSION).contains(&v))).then_some(v)
}

/// Whether `sv` of a SAS is acceptable: a date from 2012-02-12 on, and not newer than [`NEWEST_VERSION`].
pub fn sas_version_ok(sv: &str) -> bool {
    is_date(sv) && (OLDEST_SAS_VERSION..=NEWEST_VERSION).contains(&sv)
}

/// The three storage services, where they differ in what they answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Blob,
    Queue,
    Table,
}

impl Flavor {
    /// The first `x-ms-version` that is answered with the Bearer challenge when there are no credentials. Older versions
    /// (from before Entra ID could sign in to the service) answer 404 `ResourceNotFound`, so that an anonymous caller
    /// learns nothing about what exists.
    fn first_challenge_version(self) -> &'static str {
        match self {
            Flavor::Blob | Flavor::Queue => "2019-12-12",
            Flavor::Table => "2020-12-06",
        }
    }
}

/// What a request with no credentials is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoCredentials {
    /// 401 `NoAuthenticationInformation` with the [`CHALLENGE`].
    Challenge,
    /// 404 `ResourceNotFound` (an `x-ms-version` from before the challenge existed).
    NotFound,
}

/// The answer to a request with no credentials, by the version it asks for (none: the newest).
pub fn without_credentials(req: &Request, flavor: Flavor) -> NoCredentials {
    match req.header("x-ms-version").map(str::trim) {
        Some(v) if is_date(v) && v < flavor.first_challenge_version() => NoCredentials::NotFound,
        _ => NoCredentials::Challenge,
    }
}

/// What is wrong with an `Authorization` header that no scheme of ours can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadAuthorization {
    /// 400 `InvalidAuthenticationInfo`: one word only, or `Bearer` with no token.
    Format,
    /// 403 `AuthenticationFailed`; the word is the scheme (`Authentication scheme Foo is not supported`).
    UnknownScheme(String),
}

/// Look at the form of the `Authorization` header (not at the credential in it). A SAS in the query is a credential of
/// its own: the header is then not what decides. An empty header counts as none. A bearer token is never looked at (azemu
/// does not validate tokens; there is no tenant to ask).
pub fn bad_authorization(req: &Request) -> Option<BadAuthorization> {
    let value = req.header("authorization")?.trim();
    if value.is_empty() || req.query.has("sig") {
        return None;
    }
    let Some((scheme, rest)) = value.split_once(' ') else {
        return Some(BadAuthorization::Format);
    };
    if scheme.eq_ignore_ascii_case("bearer") {
        return rest.trim().is_empty().then_some(BadAuthorization::Format);
    }
    if scheme.eq_ignore_ascii_case("sharedkey") || scheme.eq_ignore_ascii_case("sharedkeylite") {
        return None;
    }
    Some(BadAuthorization::UnknownScheme(scheme.to_string()))
}

/// The bytes of a SAS signature written as base64. White space is ignored (Azure ignores it there: a signature of letters
/// and spaces that makes valid base64 is read as one), the rest must be padded standard base64.
pub fn decode_signature(sig: &str) -> Option<Vec<u8>> {
    let squeezed: String = sig.chars().filter(|c| !c.is_whitespace()).collect();
    B64.decode(squeezed).ok()
}

/// The bytes of the signature of a Shared Key header: padded standard base64 and nothing else (Azure does not ignore
/// white space there: letters and spaces that would make valid base64 are `400`).
pub fn decode_header_signature(sig: &str) -> Option<Vec<u8>> {
    B64.decode(sig).ok()
}

/// A SAS `sig` that is not usable, as the `AuthenticationErrorDetail` Azure sends: empty, not base64, or not 32 bytes (the
/// size of an HMAC-SHA256). `Ok` holds the 32 bytes.
pub fn sas_signature(sig: &str) -> Result<Vec<u8>, &'static str> {
    if sig.is_empty() {
        return Err("sig is mandatory. Cannot be empty");
    }
    match decode_signature(sig) {
        None => Err("Signature fields not well formed."),
        Some(b) if b.len() != 32 => Err("Signature size is invalid"),
        Some(b) => Ok(b),
    }
}

/// The date Azure reads from a request: `x-ms-date`, else `Date`. RFC 1123 and RFC 850 forms are accepted.
fn request_date(raw: &str) -> Option<OffsetDateTime> {
    if let Some(t) = parse_rfc1123(raw) {
        return Some(t);
    }
    // RFC 850: `Saturday, 14-Nov-26 07:00:00 GMT`
    let (_, rest) = raw.trim().split_once(',')?;
    let mut parts = rest.split_whitespace();
    let dmy = parts.next()?;
    let mut d = dmy.split('-');
    let (day, mon, year) = (d.next()?, d.next()?, d.next()?);
    let spaced: Vec<&str> = [day, mon, year].into_iter().chain(parts).collect();
    parse_rfc1123(&format!("Mon, {}", spaced.join(" ")))
}

/// The date of a Shared Key request is too old, missing or not a date: the `AuthenticationErrorDetail` Azure sends. A
/// date up to [`MAX_DATE_AGE_SECS`] old, or any date in the future, is fine.
pub fn date_problem(req: &Request) -> Option<String> {
    let raw = req.header("x-ms-date").or_else(|| req.header("date")).map(str::trim).filter(|v| !v.is_empty());
    let Some(raw) = raw else {
        return Some("Request date header not specified".to_string());
    };
    let Some(sent) = request_date(raw) else {
        return Some("The Date header in the request is incorrect.".to_string());
    };
    ((req.ctx.time - sent).whole_seconds() > MAX_DATE_AGE_SECS).then(|| format!("Request date header too old: '{raw}'"))
}

/// The address of the caller as far as we know it: `X-Forwarded-For` when present, else loopback.
pub fn client_ip(req: &Request) -> String {
    req.header("x-forwarded-for").and_then(|v| v.split(',').next()).map(|s| s.trim().to_string()).unwrap_or_else(|| "127.0.0.1".to_string())
}

/// Whether `ip` is the address or in the range (`a.b.c.d` or `a.b.c.d-e.f.g.h`) of a SAS `sip`. IPv4 only.
pub fn ip_allowed(range: &str, ip: &str) -> bool {
    let parse = |s: &str| s.trim().parse::<std::net::Ipv4Addr>().ok().map(u32::from);
    let Some(ip) = parse(ip) else { return false };
    match range.split_once('-') {
        Some((a, b)) => match (parse(a), parse(b)) {
            (Some(a), Some(b)) => a <= ip && ip <= b,
            _ => false,
        },
        None => parse(range) == Some(ip),
    }
}

/// What is wrong with the `sip` of a SAS: `Err(detail)` when it is not an address or a range of addresses (403
/// `AuthenticationFailed`); `Ok(true)` when the caller is inside it; `Ok(false)` when not (403 `AuthorizationFailure`).
/// An IPv6 address is well formed to Azure; azemu only knows IPv4 callers, so it never matches.
pub fn sip_matches(req: &Request, sip: &str) -> Result<bool, &'static str> {
    let ok = |s: &str| s.trim().parse::<std::net::IpAddr>().is_ok();
    let well_formed = match sip.split_once('-') {
        Some((a, b)) => ok(a) && ok(b),
        None => ok(sip),
    };
    if !well_formed {
        return Err("SAS field 'sip' is not well formed.");
    }
    Ok(ip_allowed(sip, &client_ip(req)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{Endpoint, Query, RequestCtx};
    use ::time::macros::datetime;

    fn req(headers: &[(&str, &str)], query: &str) -> Request {
        let mut h = http::HeaderMap::new();
        for (k, v) in headers {
            h.append(http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        Request {
            method: http::Method::GET,
            path: "/".into(),
            query: Query::parse(query),
            headers: h,
            body: bytes::Bytes::new(),
            account: "a".into(),
            ctx: RequestCtx {
                request_id: "r".into(),
                time: datetime!(2026-11-14 07:00:00 UTC),
                endpoint: Endpoint { scheme: "http", authority: "h".into(), path_style: false, account: "a".into() },
            },
        }
    }

    // Each row is an answer seen from real Azure (blob, queue and table alike).
    #[test]
    fn versions() {
        for v in ["2008-10-27", "2009-09-19", "2025-07-05", "2026-12-06", "2025-07-05 "] {
            assert_eq!(refused_version(&req(&[("x-ms-version", v)], "")), None, "{v}");
        }
        for v in ["1999-01-01", "2099-01-01", "banana", "2008-10-26", "2025-7-5", "20250705", "2025-13-01"] {
            assert_eq!(refused_version(&req(&[("x-ms-version", v)], "")), Some(v.trim()), "{v}");
        }
        assert_eq!(refused_version(&req(&[], "")), None);
        assert!(sas_version_ok("2012-02-12") && sas_version_ok("2025-07-05"));
        assert!(!sas_version_ok("2012-02-11") && !sas_version_ok("2099-01-01") && !sas_version_ok("banana") && !sas_version_ok(""));
    }

    #[test]
    fn no_credentials_by_version() {
        let ask = |v: Option<&str>, f| {
            without_credentials(&req(&v.map(|v| [("x-ms-version", v)]).into_iter().flatten().collect::<Vec<_>>(), ""), f)
        };
        assert_eq!(ask(None, Flavor::Blob), NoCredentials::Challenge);
        assert_eq!(ask(Some("2019-07-07"), Flavor::Blob), NoCredentials::NotFound);
        assert_eq!(ask(Some("2019-12-12"), Flavor::Queue), NoCredentials::Challenge);
        assert_eq!(ask(Some("2020-10-02"), Flavor::Table), NoCredentials::NotFound);
        assert_eq!(ask(Some("2020-12-06"), Flavor::Table), NoCredentials::Challenge);
    }

    #[test]
    fn authorization_header_forms() {
        let a = |v: &str, q: &str| bad_authorization(&req(&[("authorization", v)], q));
        assert_eq!(a("Foo", ""), Some(BadAuthorization::Format));
        assert_eq!(a("Bearer", ""), Some(BadAuthorization::Format));
        assert_eq!(a("Bearer  ", ""), Some(BadAuthorization::Format));
        assert_eq!(a("Foo bar", ""), Some(BadAuthorization::UnknownScheme("Foo".into())));
        assert_eq!(a("Bearer garbage.token", ""), None);
        assert_eq!(a("SharedKey a:b", ""), None);
        assert_eq!(a("", ""), None);
        assert_eq!(a("Foo bar", "sig=x"), None);
        assert_eq!(bad_authorization(&req(&[], "")), None);
    }

    #[test]
    fn signatures() {
        let good = B64.encode([7u8; 32]);
        assert_eq!(sas_signature(&good).map(|b| b.len()), Ok(32));
        assert_eq!(sas_signature(&format!("{good}\n")).map(|b| b.len()), Ok(32));
        assert_eq!(sas_signature(""), Err("sig is mandatory. Cannot be empty"));
        for bad in ["!!not-base64!!", "abc", "Sanitized", "replay", &good[..43], &format!("{good}A"), &"!".repeat(30)] {
            assert_eq!(sas_signature(bad), Err("Signature fields not well formed."), "{bad}");
        }
        for bad in [
            B64.encode([1u8; 16]),
            B64.encode([1u8; 33]),
            B64.encode([1u8; 64]),
            "Zm9v".repeat(50),
            "this is not a valid signature".to_string(),
        ] {
            assert_eq!(sas_signature(&bad), Err("Signature size is invalid"), "{bad}");
        }
        let url = B64.encode([0xfbu8; 32]).replace('+', "-").replace('/', "_");
        assert!(url.contains('-') && decode_signature(&url).is_none());
        // in a SAS white space is ignored, in a Shared Key header it is not
        assert!(decode_signature("this is not a valid signature").is_some());
        assert!(decode_header_signature("this is not a valid signature").is_none());
    }

    #[test]
    fn dates() {
        let d = |v: &str| date_problem(&req(&[("x-ms-date", v)], ""));
        assert_eq!(d("Sat, 14 Nov 2026 06:46:00 GMT"), None);
        assert_eq!(d("Sat, 14 Nov 2026 06:44:59 GMT"), Some("Request date header too old: 'Sat, 14 Nov 2026 06:44:59 GMT'".into()));
        assert_eq!(d("Sat, 15 Nov 2026 07:00:00 GMT"), None);
        assert_eq!(d("Saturday, 14-Nov-26 07:00:00 GMT"), None);
        assert_eq!(d("garbage").as_deref(), Some("The Date header in the request is incorrect."));
        assert_eq!(d("2026-11-14T07:00:00Z").as_deref(), Some("The Date header in the request is incorrect."));
        assert_eq!(date_problem(&req(&[], "")).as_deref(), Some("Request date header not specified"));
        assert_eq!(date_problem(&req(&[("date", "Sat, 14 Nov 2026 07:00:00 GMT")], "")), None);
    }

    #[test]
    fn source_address() {
        assert_eq!(sip_matches(&req(&[], ""), "127.0.0.1"), Ok(true));
        assert_eq!(sip_matches(&req(&[], ""), "203.0.113.9"), Ok(false));
        assert_eq!(sip_matches(&req(&[], ""), "0.0.0.0-255.255.255.255"), Ok(true));
        assert_eq!(sip_matches(&req(&[("x-forwarded-for", "203.0.113.9, 10.0.0.1")], ""), "203.0.113.9"), Ok(true));
        assert_eq!(sip_matches(&req(&[], ""), "::1"), Ok(false));
        assert_eq!(sip_matches(&req(&[], ""), "banana"), Err("SAS field 'sip' is not well formed."));
        assert!(ip_allowed("127.0.0.0-127.255.255.255", "127.0.0.1"));
        assert!(!ip_allowed("10.0.0.1-10.0.0.9", "127.0.0.1"));
    }
}
