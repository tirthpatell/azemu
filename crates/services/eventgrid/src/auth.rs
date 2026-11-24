//! Credentials and the headers every Event Grid answer carries.
//!
//! The classic publish endpoint (`POST /api/events`) compares the key or the signed token with the access keys the
//! topic has, when it has any (`check_classic`); the namespace endpoints only check that a credential is there
//! (`aeg-sas-key`, `aeg-sas-token` or an `Authorization` header). A bearer token is never verified.

use azemu_core::http::HeaderValue;
use azemu_core::{Request, Response};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;

use crate::errors::{self, Fail};

pub const CLASSIC_VERSION: &str = "2018-01-01";
pub const NAMESPACE_VERSION: &str = "2024-06-01";

/// `Err(401)` when the request has no credential at all (what the namespace endpoints check).
pub fn check(req: &Request) -> Result<(), Response> {
    let present = |name: &str| req.header(name).map(|v| !v.trim().is_empty()).unwrap_or(false);
    if present("aeg-sas-key") || present("aeg-sas-token") || present("authorization") {
        return Ok(());
    }
    Err(no_credential(req))
}

fn no_credential(req: &Request) -> Response {
    Fail::unauthorized("Request must contain one of the following authorization signature: aeg-sas-token, aeg-sas-key.").into_response(req)
}

/// The host of the request without its port.
fn host_of(req: &Request) -> String {
    req.ctx.endpoint.authority.split(':').next().unwrap_or("").to_string()
}

/// `?api-version=` of a classic publish. Event Grid takes `2018-01-01` and says `UnsupportedApiVersion` to any other
/// version that looks like one (a date, a number); a value that is no version at all, and a missing one, pass.
pub fn check_api_version(req: &Request) -> Option<Response> {
    let v = req.query.get("api-version")?.trim();
    let looks_like_version = v.bytes().next().map(|b| b.is_ascii_digit()).unwrap_or(false);
    let is_the_version = {
        let parts: Vec<Option<u32>> = v.split('-').map(|p| p.parse().ok()).collect();
        parts == [Some(2018), Some(1), Some(1)]
    };
    if !looks_like_version || is_the_version {
        return None;
    }
    let uri = format!("{}{}", req.ctx.endpoint.base_url(), req.path);
    Some(errors::unsupported_api_version(&uri, v))
}

const BAD_BASE64: &str =
    "The input is not a valid Base-64 string as it contains a non-base 64 character, more than two padding characters, or an illegal character among the padding characters.";

/// The domains a topic's own URL is under (`docs/strict-auth.md`, "Audiences"); a star takes the region label.
const TOPIC_DOMAINS: &[&str] =
    &["*.eventgrid.azure.net", "*.eventgrid.azure.cn", "*.eventgrid.azure.us", "eventgrid.azure.net.localhost", "eventgrid.localhost"];

/// Strict mode (`docs/strict-auth.md`): a bearer token is verified, then the principal needs `EventGrid Data Sender` on the topic. Every refusal
/// is 401, also a missing role (Azure sent that in November 2026). `None`: the request carries no bearer token, or the mode is off.
pub fn check_token(auth: &azemu_core::auth::Auth, req: &Request) -> Option<Response> {
    use azemu_core::auth::{Aud, Need, Resource, TokenFailure};
    if !auth.applies(req) {
        return None;
    }
    let topic = req.account.to_ascii_lowercase();
    // the topic's own URL: `{topic}.{region}-1.eventgrid.azure.net` (the region is a label of its own), another cloud's domain, or the emulator's
    let auds = [Aud::exact("https://eventgrid.azure.net"), Aud::host_in(&topic, TOPIC_DOMAINS)];
    let who = match auth.authenticate(req, &auds) {
        Ok(w) => w,
        Err(TokenFailure::Malformed) => return Some(Fail::unauthorized("Unable to read access token.").into_response(req)),
        Err(_) => {
            return Some(
                Fail::unauthorized("Invalid access token. Please ensure that the token is not expired and has the right audience.")
                    .into_response(req),
            )
        }
    };
    let res = Resource::event_grid_topic(&topic);
    let what = format!("{} {}", req.method, req.decoded_path());
    auth.authorize(&what, &who, &Need::data("Microsoft.EventGrid/events/send/action"), &res).err().map(|_| {
        let id = auth.registry().display_id(&res);
        Fail::unauthorized(format!(
            "The principal associated with access token presented with the incoming request does not have permission to send data to {id}."
        ))
        .into_response(req)
    })
}

/// Credentials of `POST /api/events`, as Azure checks them (tried in November 2026). `keys` are the access keys the topic
/// has (from ARM, or given when the topic was declared); a topic with none has nothing to compare with and, as before, takes
/// any key or token that is there. Looked at in this order: `aeg-sas-key`, `aeg-sas-token`, `Authorization`. A bearer token
/// is never checked: there is no Entra tenant to ask.
pub fn check_classic(req: &Request, keys: &[String]) -> Result<(), Response> {
    if let Some(k) = req.header("aeg-sas-key") {
        let k = k.trim();
        if k.is_empty() {
            return Err(Fail::new(400, "BadRequest", "InvalidSas", "Request must have a value for aeg-sas-key.").into_response(req));
        }
        if keys.is_empty() || keys.iter().any(|x| x == k) {
            return Ok(());
        }
        let reason = if STANDARD.decode(k).is_ok() { None } else { Some(BAD_BASE64) };
        return Err(errors::unauthorized_key(req, &host_of(req), reason));
    }
    if let Some(t) = req.header("aeg-sas-token") {
        return check_sas(req, t.trim(), keys);
    }
    let Some(h) = req.header("authorization").map(str::trim).filter(|h| !h.is_empty()) else {
        return Err(no_credential(req));
    };
    let (scheme, rest) = h.split_once(' ').unwrap_or((h, ""));
    let rest = rest.trim();
    match scheme.to_ascii_lowercase().as_str() {
        "bearer" if !rest.is_empty() => Ok(()),
        "bearer" => Err(Fail::unauthorized("Unable to read access token.").into_response(req)),
        "sharedaccesssignature" => check_sas(req, rest, keys),
        _ => Err(Fail::unauthorized("Request has an unsupported Authorization scheme (must be SharedAccessSignature or Bearer).")
            .into_response(req)),
    }
}

/// `r=<resource>&e=<expiry>&s=<signature>`: the signature is the HMAC-SHA256, under the base64-decoded key, of the text
/// before `&s=` (the way the SDKs make it).
fn check_sas(req: &Request, token: &str, keys: &[String]) -> Result<(), Response> {
    if keys.is_empty() {
        return Ok(());
    }
    const FORMAT: &str = "Valid format :r=UrlEncodedResource&e=UrlEncodedUtcExpiration&s=UrlEncodedSignature.";
    let bad = |what: &str| {
        Fail::new(400, "BadRequest", "InvalidSas", format!("{what} value in aeg-sas-token. {FORMAT} Input sas string: {token}."))
            .into_response(req)
    };
    if !token.contains('=') {
        return Err(Fail::new(
            400,
            "BadRequest",
            "InvalidSas",
            format!("Malformed value for aeg-sas-token. {FORMAT} Input sas string: {token}."),
        )
        .into_response(req));
    }
    let decode = |v: &str| percent_encoding::percent_decode_str(&v.replace('+', " ")).decode_utf8_lossy().into_owned();
    // `r` and `e` are read from the text that is signed and from nowhere else: `s` has to be the last pair, and no name may
    // come twice, or a signature made for one expiry could be shown with another.
    let pairs: Vec<&str> = token.split('&').collect();
    let (mut r, mut e, mut s) = (None, None, None);
    for (i, pair) in pairs.iter().enumerate() {
        let Some((n, v)) = pair.split_once('=') else { continue };
        let slot = match n.to_ascii_lowercase().as_str() {
            "r" => &mut r,
            "e" => &mut e,
            "s" if i + 1 == pairs.len() => &mut s,
            "s" => return Err(bad("Invalid")),
            _ => continue,
        };
        if slot.replace(decode(v)).is_some() {
            return Err(bad("Invalid"));
        }
    }
    let unsigned_end = token.len() - pairs.last().map(|l| l.len() + 1).unwrap_or(0).min(token.len());
    let (Some(r), Some(e), Some(s)) = (r, e, s) else { return Err(bad("Invalid")) };
    let Some(expiry) = parse_expiry(&e) else { return Err(bad("Invalid")) };
    let Ok(sig) = STANDARD.decode(s.trim()) else { return Err(bad("Invalid")) };
    let now = req.ctx.time;
    // Ten minutes of life are asked for, less five minutes of grace for clocks that differ.
    if expiry <= now + azemu_core::time::Duration::minutes(5) {
        return Err(Fail::unauthorized(format!(
            "Expired. e:{} must be greater than {} and have at least 00:10:00 of remaining timespan. There is an additional 00:05:00 grace period for clock skew . aeg-sas-token.",
            errors::us_time(expiry),
            errors::us_time(now)
        ))
        .into_response(req));
    }
    let resource_host = r.split("://").last().unwrap_or("").split(['/', '?']).next().unwrap_or("").split(':').next().unwrap_or("");
    if !resource_host.eq_ignore_ascii_case(&host_of(req)) {
        let url = format!("{}/api/events", req.ctx.endpoint.base_url());
        return Err(Fail::unauthorized(format!("The authorization signature does not apply to resource: {url}.")).into_response(req));
    }
    let unsigned = &token[..unsigned_end];
    let signed = keys.iter().any(|k| {
        let key = STANDARD.decode(k).unwrap_or_else(|_| k.as_bytes().to_vec());
        azemu_core::storage::sig::hmac_sha256(&key, unsigned.as_bytes()).as_slice() == sig.as_slice()
    });
    if signed {
        Ok(())
    } else {
        Err(Fail::unauthorized("Invalid signature. The request authorization signature was not signed with any of the topic keys.")
            .into_response(req))
    }
}

/// The expiry of a token: ISO 8601 (`2026-11-14 08:00:00`, what the Python SDK writes) or `11/14/2026 8:00:00 AM`.
fn parse_expiry(e: &str) -> Option<azemu_core::time::OffsetDateTime> {
    let e = e.trim();
    if let Some(t) = azemu_core::time::parse_iso8601(e) {
        return Some(t);
    }
    let (date, time) = e.split_once(' ')?;
    let mut d = date.split('/');
    let (m, day, y): (u8, u8, i32) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let (clock, half) = match time.trim().rsplit_once(' ') {
        Some((c, h)) => (c, Some(h.to_ascii_uppercase())),
        None => (time.trim(), None),
    };
    let mut hms = clock.split(':');
    let mut h: u8 = hms.next()?.parse().ok()?;
    let mi: u8 = hms.next()?.parse().ok()?;
    let sec: u8 = hms.next().map(str::parse).transpose().ok()?.unwrap_or(0);
    match half.as_deref() {
        Some("PM") if h < 12 => h += 12,
        Some("AM") if h == 12 => h = 0,
        _ => {}
    }
    azemu_core::time::parse_iso8601(&format!("{y:04}-{m:02}-{day:02}T{h:02}:{mi:02}:{sec:02}Z"))
}

/// Add the headers Event Grid sets on every answer, unless the handler already did.
pub fn finish(mut resp: Response, req: &Request) -> Response {
    if req.path.starts_with("/_azemu/") {
        return resp;
    }
    let h = resp.headers_mut();
    let mut put = |k: &'static str, v: &str| {
        if !h.contains_key(k) {
            if let Ok(v) = HeaderValue::from_str(v) {
                h.insert(k, v);
            }
        }
    };
    let version = if req.path.starts_with("/topics/") { NAMESPACE_VERSION } else { CLASSIC_VERSION };
    put("api-supported-versions", version);
    put("Server", "Microsoft-HTTPAPI/2.0");
    put("Strict-Transport-Security", "max-age=31536000; includeSubDomains");
    put("x-ms-request-id", &req.ctx.request_id);
    if let Some(c) = req.header("x-ms-client-request-id") {
        put("x-ms-client-request-id", c);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use azemu_core::http::{HeaderMap, Method};
    use azemu_core::{Endpoint, Query, RequestCtx};

    const KEY: &str = "a2V5LW9uZS0wMTIzNDU2Nzg5MDEyMzQ1Njc4OTAxMjM=";

    fn request() -> Request {
        Request {
            method: Method::POST,
            path: "/api/events".into(),
            query: Query::default(),
            headers: HeaderMap::new(),
            body: azemu_core::bytes::Bytes::new(),
            account: "topic1".into(),
            ctx: RequestCtx {
                request_id: "id".into(),
                time: azemu_core::time::OffsetDateTime::now_utc(),
                endpoint: Endpoint {
                    scheme: "https",
                    authority: "topic1.eventgrid.localhost:4577".into(),
                    path_style: false,
                    account: "topic1".into(),
                },
            },
        }
    }

    fn sign(unsigned: &str) -> String {
        let sig = azemu_core::storage::sig::hmac_sha256(&STANDARD.decode(KEY).unwrap(), unsigned.as_bytes());
        STANDARD.encode(sig).replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")
    }

    fn at(minutes: i64) -> String {
        let t = azemu_core::time::OffsetDateTime::now_utc() + azemu_core::time::Duration::minutes(minutes);
        format!("{:04}-{:02}-{:02}%20{:02}%3A{:02}%3A{:02}", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second())
    }

    const R: &str = "r=https%3A%2F%2Ftopic1.eventgrid.localhost%3A4577%2Fapi%2Fevents";

    fn status(token: &str) -> u16 {
        match check_sas(&request(), token, &[KEY.to_string()]) {
            Ok(()) => 200,
            Err(r) => r.status().as_u16(),
        }
    }

    #[test]
    fn the_normal_token_passes() {
        let unsigned = format!("{R}&e={}", at(60));
        assert_eq!(status(&format!("{unsigned}&s={}", sign(&unsigned))), 200);
    }

    #[test]
    fn a_signature_cannot_be_shown_with_another_expiry_or_resource() {
        let old = format!("{R}&e={}", at(-60));
        let sig = sign(&old);
        // the signature is right for the old expiry; a later `e` after `s` must not rescue it
        assert_eq!(status(&format!("{old}&s={sig}&e={}", at(600))), 400);
        // the same for a second resource after `s`
        let other = format!("r=https%3A%2F%2Fother.example%2Fapi%2Fevents&e={}", at(60));
        assert_eq!(status(&format!("{other}&s={}&{R}", sign(&other))), 400);
        // a name twice before `s` is refused too, whichever comes first
        let dup = format!("{R}&e={}&e={}", at(-60), at(600));
        assert_eq!(status(&format!("{dup}&s={}", sign(&dup))), 400);
        let dup = format!("{R}&e={}&e={}", at(600), at(-60));
        assert_eq!(status(&format!("{dup}&s={}", sign(&dup))), 400);
        let dup = format!("{R}&{R}&e={}", at(60));
        assert_eq!(status(&format!("{dup}&s={}", sign(&dup))), 400);
        // two signatures
        let unsigned = format!("{R}&e={}", at(60));
        let s = sign(&unsigned);
        assert_eq!(status(&format!("{unsigned}&s={s}&s={s}")), 400);
        // the expiry that was signed is the one checked: an old one is still expired
        assert_eq!(status(&format!("{old}&s={sig}")), 401);
    }
}
