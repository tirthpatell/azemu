//! What Azure checks on every request before it looks at the resource (checked against a real account in November 2026):
//! the signature path, the API version, the page size, the consistency level and the session token.
//!
//! Signatures are not checked here (any key works). Azure signs the path the client built, but reads it back after its own
//! decoding, so a name with `#` or with a percent escape in it cannot be addressed: that is the one 401 this module raises.

use crate::errors::{CosmosError, R};
use crate::out::Cx;

/// The consistency level of the account (`GET /` says it too).
const ACCOUNT_LEVEL: &str = "Session";

/// Runs every check; the first that fails is the answer.
pub fn check(cx: &Cx) -> R<()> {
    signed_path(cx)?;
    api_version(cx)?;
    page_size(cx)?;
    consistency(cx)?;
    session_format(cx)?;
    Ok(())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
            let h = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
            out.push(h(b[i + 1]) * 16 + h(b[i + 2]));
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A name that Azure cannot address: it cuts the path at `#`, decodes percent escapes a second time, trims a trailing space,
/// and does not take `/` or `\` inside a name. The signature the client made then does not fit the path Azure reads, so the
/// answer is 401 with the text it signed (checked against a real account in November 2026).
fn signed_path(cx: &Cx) -> R<()> {
    let once = cx.req.decoded_path();
    let link = if cx.req.path.split('/').any(|seg| {
        let d = percent_decode(seg);
        d.contains('/') || d.contains('\\')
    }) {
        // Azure finds no resource in such a path: the text it signed is empty
        None
    } else {
        let link = match once.find('#') {
            Some(i) => once[..i].to_string(),
            None => percent_decode(&once),
        };
        let link = link.trim_end().to_string();
        if link == once {
            return Ok(());
        }
        Some(link.trim_matches('/').to_string())
    };
    let (kind, link) = match link {
        None => (String::new(), String::new()),
        Some(link) => {
            let segs: Vec<&str> = link.split('/').filter(|s| !s.is_empty()).collect();
            let kind = match segs.len() {
                0 => "",
                n if n % 2 == 1 => segs[n - 1],
                n => segs[n - 2],
            };
            (kind.to_ascii_lowercase(), link.clone())
        }
    };
    let date = cx.header("x-ms-date").or_else(|| cx.header("date")).unwrap_or("").to_ascii_lowercase();
    let payload = format!("{}\n{}\n{}\n{}\n\n", cx.req.method.as_str().to_ascii_lowercase(), kind, link, date);
    Err(CosmosError::plain(
        401,
        "Unauthorized",
        format!(
            "The input authorization token can't serve the request. The wrong key is being used or the expected payload is not built as per the protocol. \
             For more info: https://aka.ms/cosmosdb-tsg-unauthorized. Server used the following payload to sign: '{payload}'"
        ),
    ))
}

/// `x-ms-version` is required and is a date. Azure knows a short list of versions; the emulator does not know which dates
/// exist, so it refuses only a missing value, a value that is not a date and one older than the first version (2014-08-21).
fn api_version(cx: &Cx) -> R<()> {
    let ok = cx.header("x-ms-version").is_some_and(|v| {
        let p: Vec<&str> = v.split('-').collect();
        p.len() == 3
            && p[0].len() == 4
            && p[1].len() == 2
            && p[2].len() == 2
            && v.bytes().all(|b| b.is_ascii_digit() || b == b'-')
            && v >= "2014-08-21"
    });
    if ok {
        return Ok(());
    }
    Err(CosmosError::plain(
        400,
        "BadRequest",
        "Invalid API version. Ensure a valid x-ms-version header value is passed. Please update to the latest version of Azure Cosmos DB SDK.",
    ))
}

/// `x-ms-max-item-count` is an integer from -1 to 2,147,483,647 (0 and -1 mean "the default"). The list of partition key
/// ranges only refuses a number below -1 (a value that is not a number, or is too big, is ignored there).
fn page_size(cx: &Cx) -> R<()> {
    let Some(raw) = cx.header("x-ms-max-item-count").filter(|v| !v.is_empty()) else { return Ok(()) };
    let ranges = cx.req.path.trim_end_matches('/').ends_with("/pkranges");
    match raw.parse::<i32>() {
        Ok(n) if n >= -1 => Ok(()),
        Err(_) if ranges => Ok(()),
        _ => Err(CosmosError::plain(
            400,
            "BadRequest",
            format!(
                "The input PageSize {raw} is invalid. Ensure to pass a valid page size which must be a positive integer or -1 for a dynamic page size."
            ),
        )),
    }
}

/// A request may ask for a weaker level than the account's, not a stronger one. The names are case sensitive.
fn consistency(cx: &Cx) -> R<()> {
    let Some(v) = cx.header("x-ms-consistency-level").filter(|v| !v.is_empty()) else { return Ok(()) };
    match v {
        "Strong" | "BoundedStaleness" => Err(CosmosError::plain(
            400,
            "BadRequest",
            format!(
                "ConsistencyLevel {v} specified in the request is invalid when service is configured with consistency level {ACCOUNT_LEVEL}. \
                 Ensure the request consistency level is not stronger than the service consistency level."
            ),
        )),
        "Session" | "ConsistentPrefix" | "Eventual" => Ok(()),
        _ => Err(CosmosError::plain(
            400,
            "BadRequest",
            format!("Value '{v}' specified for the header 'x-ms-consistency-level' is invalid. "),
        )),
    }
}

fn invalid_session(part: &str) -> CosmosError {
    CosmosError::plain(400, "BadRequest", format!("The session token provided '{part}' is invalid."))
}

/// The progress of partition key range 0 that a session token carries, `None` when it has none. A token is
/// `<range>:<version>#<lsn>[#<region>=<lsn>...]`, several ranges separated by commas. The emulator has one range, "0".
fn parse_session(token: &str) -> R<Option<i64>> {
    let mut lsn0 = None;
    for part in token.split(',') {
        let part = part.trim();
        let Some((range, rest)) = part.split_once(':') else { return Err(invalid_session(part)) };
        let mut fields = rest.split('#');
        if fields.next().and_then(|v| v.parse::<i64>().ok()).is_none() {
            return Err(invalid_session(rest));
        }
        let Some(n) = fields.next().and_then(|v| v.parse::<i64>().ok()) else { return Err(invalid_session(rest)) };
        if range == "0" {
            lsn0 = Some(lsn0.map_or(n, |m: i64| m.max(n)));
        }
    }
    Ok(lsn0)
}

/// Documents and stored procedures check the session token they are sent; the other resources ignore it.
fn session_format(cx: &Cx) -> R<()> {
    let Some(t) = cx.header("x-ms-session-token").map(str::trim).filter(|t| !t.is_empty()) else { return Ok(()) };
    let segs = cx.req.segments();
    if segs.len() >= 5 && segs[0] == "dbs" && segs[2] == "colls" && matches!(segs[4].as_str(), "docs" | "sprocs") {
        parse_session(t)?;
    }
    Ok(())
}

/// A read whose session token is ahead of the container is a 404 with sub-status 1002: the replica has not seen what the
/// client saw. The usual cause is a container that was deleted and made again under the same name.
pub fn session_not_ahead(cx: &Cx, container_lsn: u64) -> R<()> {
    let Some(t) = cx.header("x-ms-session-token").map(str::trim).filter(|t| !t.is_empty()) else { return Ok(()) };
    if let Some(n) = parse_session(t)? {
        if n > 0 && n as u64 > container_lsn {
            return Err(CosmosError::plain(404, "NotFound", "The read session is not available for the input session token.").sub(1002));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_tokens() {
        assert_eq!(parse_session("0:-1#12").unwrap(), Some(12));
        assert_eq!(parse_session("0:-1#12#4=3").unwrap(), Some(12));
        assert_eq!(parse_session("1:-1#9").unwrap(), None);
        assert_eq!(parse_session("0:-1#5,1:-1#9").unwrap(), Some(5));
        for bad in ["garbage", "0", "-1", "0:-1#x", "0:x#5", "0:-1"] {
            assert!(parse_session(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_session("0:-1#x").unwrap_err().message, "The session token provided '-1#x' is invalid.");
    }

    #[test]
    fn percent_escapes() {
        assert_eq!(percent_decode("p%20q"), "p q");
        assert_eq!(percent_decode("a%b"), "a%b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%4"), "%4");
    }
}
