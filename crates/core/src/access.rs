//! Who may talk to the emulator: the host check and the cross-site check.
//!
//! The emulator has no authentication, and it listens on the developer's own machine. Two things can still reach it
//! from a web page the developer has open:
//!
//! * **DNS rebinding.** A page on `evil.example` makes its name resolve to `127.0.0.1`; the browser then sends
//!   requests to the emulator as "same origin" with `Host: evil.example`. The host check refuses every request whose
//!   host is not one the emulator is meant to serve. An attacker's name always has a dot and is never under
//!   `localhost` or a suffix the user listed, so the check stops it.
//! * **Cross-site requests.** A page can send a form `POST` (no preflight) or a `fetch` to `http://localhost:4577`.
//!   A request is cross-site when the browser says so (`Sec-Fetch-Site: cross-site`) or when its `Origin` (older
//!   browsers: `Referer`) names a host that fails the host check. Admin paths (`/_azemu/...`) and WebSocket upgrades
//!   refuse all of them. A service request is refused only when the browser says `Sec-Fetch-Site: cross-site` (every
//!   browser since 2020 to 2023 sends it, and a program that sets `Origin` itself, such as a CORS test, does not),
//!   and then unless the service's own CORS rules accept the origin (core asks the service with a preflight) or
//!   `--allow-origin` allows it. A service request with only an `Origin` of another site goes through as it does
//!   on Azure, where `Origin` only decides whether CORS headers are added.
//!
//! Programs that are not browsers (every SDK) send neither header and are not affected.

use std::net::IpAddr;
use std::str::FromStr;

use anyhow::{bail, Result};
use http::{header, HeaderMap};

use crate::{json_error, Response};

/// Names that are served without being listed: the Docker host names.
const DOCKER_HOSTS: &[&str] = &["host.docker.internal", "gateway.docker.internal"];

/// A short list of public top-level domains and a few public second-level suffixes. `--tls-name` and `--allow-host`
/// refuse these on their own (`com`, `co.uk`): a certificate or a host check for a whole TLD would cover every site
/// below it. A single label that is not in this list (`azemu`, `corp`, `db`) is fine: it is a Docker or LAN name.
const PUBLIC_SUFFIXES: &[&str] = &[
    // generic
    "com",
    "net",
    "org",
    "edu",
    "gov",
    "mil",
    "int",
    "info",
    "biz",
    "name",
    "pro",
    "aero",
    "coop",
    "museum",
    "mobi",
    "asia",
    "tel",
    "travel",
    "jobs",
    "cat",
    "xxx",
    "arpa",
    "post",
    "app",
    "dev",
    "page",
    "cloud",
    "online",
    "site",
    "tech",
    "xyz",
    "top",
    "club",
    "shop",
    "store",
    "blog",
    "live",
    "work",
    "link",
    "click",
    "win",
    "vip",
    "icu",
    "life",
    "world",
    "today",
    "space",
    "website",
    "agency",
    "email",
    "network",
    "systems",
    "solutions",
    "services",
    "software",
    "digital",
    "company",
    "group",
    "ltd",
    "inc",
    "bank",
    "azure",
    "microsoft",
    "windows",
    "google",
    "amazon",
    "aws",
    // country codes (the common ones)
    "io",
    "ai",
    "co",
    "me",
    "tv",
    "cc",
    "ws",
    "fm",
    "ly",
    "gl",
    "sh",
    "gg",
    "to",
    "us",
    "uk",
    "de",
    "fr",
    "es",
    "it",
    "nl",
    "be",
    "ch",
    "at",
    "se",
    "no",
    "dk",
    "fi",
    "pl",
    "ru",
    "cn",
    "jp",
    "kr",
    "in",
    "au",
    "nz",
    "ca",
    "br",
    "mx",
    "ar",
    "cl",
    "co",
    "za",
    "ie",
    "pt",
    "gr",
    "tr",
    "cz",
    "hu",
    "ro",
    "ua",
    "il",
    "sg",
    "hk",
    "tw",
    "id",
    "my",
    "th",
    "vn",
    "ph",
    "eu",
    "pk",
    "ng",
    "ke",
    "eg",
    "sa",
    "ae",
    "is",
    "lt",
    "lv",
    "ee",
    "sk",
    "si",
    "hr",
    "rs",
    "bg",
    "by",
    "kz",
    "ir",
    "cx",
    "tk",
    "ml",
    "ga",
    "cf",
    "gq",
    // second level
    "co.uk",
    "org.uk",
    "ac.uk",
    "gov.uk",
    "me.uk",
    "ltd.uk",
    "net.uk",
    "com.au",
    "net.au",
    "org.au",
    "edu.au",
    "gov.au",
    "co.jp",
    "ne.jp",
    "or.jp",
    "co.nz",
    "org.nz",
    "co.in",
    "net.in",
    "org.in",
    "co.za",
    "com.br",
    "com.cn",
    "com.mx",
    "com.tr",
    "co.kr",
    "com.sg",
    "com.hk",
    "com.tw",
    "com.ar",
    "com.co",
    "co.il",
    "com.ua",
    "co.id",
    "com.my",
    "com.ph",
    "com.vn",
    "com.pk",
    "com.ng",
    "com.eg",
    "com.sa",
    "github.io",
    "gitlab.io",
    "pages.dev",
    "workers.dev",
    "netlify.app",
    "vercel.app",
    "herokuapp.com",
    "azurewebsites.net",
    "cloudapp.net",
    "cloudfront.net",
    "amazonaws.com",
    "appspot.com",
    "web.app",
    "firebaseapp.com",
];

/// A DNS suffix given with `--tls-name` or `--allow-host`, checked and normalised: lower case, no leading `*.` or `.`,
/// no trailing dot. Refuses an empty name, a bad character, an empty label, a wildcard anywhere but at the start, and a
/// public suffix on its own (see `PUBLIC_SUFFIXES`).
pub(crate) fn parse_dns_suffix(raw: &str, option: &str) -> Result<String> {
    let lower = raw.trim().to_ascii_lowercase();
    let name = lower.strip_prefix("*.").unwrap_or(&lower).trim_start_matches('.').trim_end_matches('.');
    let labels_ok = !name.is_empty()
        && name.split('.').all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    if !labels_ok {
        bail!("invalid {option} '{raw}': use a DNS name such as azemu or vault.azemu (a wildcard is only allowed as a leading '*.', and no part of the name may be empty)");
    }
    if PUBLIC_SUFFIXES.contains(&name) {
        bail!(
            "refused {option} '{raw}': '{name}' is a public suffix, so it would cover every site below it. Name your own domain (for example corp.{name}) or a single name that is not a top-level domain (for example azemu)"
        );
    }
    Ok(name.to_string())
}

/// An IP address or a range given with `--tls-name`: IPv4 ranges must be at least a /8, IPv6 at least a /32.
pub(crate) fn check_ip_range(ip: IpAddr, prefix: u8, raw: &str, option: &str) -> Result<()> {
    let min = if ip.is_ipv4() { 8 } else { 32 };
    if prefix < min {
        bail!(
            "refused {option} '{raw}': the range is wider than a /{min}, so it would cover a large part of the address space. Use a narrower range such as {}",
            if ip.is_ipv4() { "10.0.0.0/8 or 192.168.0.0/16" } else { "fd00::/32" }
        );
    }
    Ok(())
}

/// The host (without port) of an authority such as `acct.blob.localhost:4577`, `[::1]:4577` or `127.0.0.1`.
struct HostPart {
    /// Lower case, brackets removed, one trailing dot removed.
    host: String,
    ip: Option<IpAddr>,
    port: Option<u16>,
}

fn parse_authority(authority: &str) -> Option<HostPart> {
    let a = authority.trim();
    let bracketed = a.starts_with('[');
    if a.is_empty()
        || a.len() > 300
        || !a.is_ascii()
        || a.bytes().any(|b| b <= b' ' || b == 0x7f || matches!(b, b'/' | b'@' | b'\\' | b'?' | b'#') || (b == b'%' && !bracketed))
    {
        return None;
    }
    let (host, port): (&str, Option<&str>) = if let Some(rest) = a.strip_prefix('[') {
        let end = rest.find(']')?;
        let tail = &rest[end + 1..];
        let port = match tail {
            "" => None,
            t => Some(t.strip_prefix(':')?),
        };
        (&rest[..end], port)
    } else if a.matches(':').count() > 1 {
        // a bare IPv6 address with no brackets (not valid in a Host header, but harmless to read)
        (a, None)
    } else {
        match a.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (a, None),
        }
    };
    let port = match port {
        None => None,
        Some(p) if !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) => Some(p.parse::<u16>().ok()?),
        Some(_) => return None,
    };
    let host = host.to_ascii_lowercase();
    if let Ok(ip) = IpAddr::from_str(host.split('%').next().unwrap_or(&host)) {
        return Some(HostPart { host, ip: Some(ip), port });
    }
    if bracketed {
        // only an IPv6 address may sit in brackets
        return None;
    }
    let name = host.strip_suffix('.').unwrap_or(&host).to_string();
    if name.is_empty() || !name.split('.').all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    {
        return None;
    }
    Some(HostPart { host: name, ip: None, port })
}

/// An origin as the browser writes it: `scheme://host[:port]`.
struct OriginPart {
    scheme: String,
    host: HostPart,
}

fn parse_origin(origin: &str) -> Option<OriginPart> {
    let o = origin.trim();
    let (scheme, rest) = o.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    // a Referer is a whole URL: keep the authority only
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    Some(OriginPart { scheme, host: parse_authority(authority)? })
}

impl OriginPart {
    fn effective_port(&self) -> u16 {
        self.host.port.unwrap_or(if self.scheme == "https" { 443 } else { 80 })
    }
}

/// One `--allow-origin` value.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OriginRule {
    /// `http://localhost:3000`: scheme, host and port.
    Exact { scheme: String, host: String, port: u16 },
    /// `app.example.com` or `app.example.com:3000`: the host (and the port when given), any scheme.
    Host { host: String, port: Option<u16> },
    /// `*.example.com`: the name and everything below it.
    Suffix(String),
}

/// What an `Origin`, `Referer` and `Sec-Fetch-Site` say about who sent the request.
#[derive(Debug, Clone, Default)]
pub(crate) struct Browser {
    /// The `Origin` header, or the origin of the `Referer` when there is no `Origin`; as sent.
    pub origin: Option<String>,
    pub sec_fetch_site: Option<String>,
    /// Another web site sent it (see the module comment): the rule for admin paths and WebSocket upgrades.
    pub cross_site: bool,
    /// The rule for service requests: the browser said `Sec-Fetch-Site: cross-site`.
    pub cross_site_fetch: bool,
    /// `--allow-origin` allows this origin (or `*` is set).
    pub permitted: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AccessPolicy {
    /// DNS suffixes served besides the built-in ones (`--tls-name` names and `--allow-host`).
    suffixes: Vec<String>,
    rules: Vec<OriginRule>,
    /// `--allow-origin '*'`: no cross-site check at all.
    any_origin: bool,
}

impl AccessPolicy {
    pub fn new(allow_hosts: &[String], tls_suffixes: Vec<String>, allow_origins: &[String]) -> Result<AccessPolicy> {
        let mut suffixes = tls_suffixes;
        for raw in allow_hosts.iter().flat_map(|s| s.split(',')) {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            // an IP address or range here is harmless and not needed (IP literals are always served)
            if IpAddr::from_str(raw.split('/').next().unwrap_or(raw)).is_ok() {
                continue;
            }
            suffixes.push(parse_dns_suffix(raw, "--allow-host")?);
        }
        suffixes.sort();
        suffixes.dedup();
        let mut policy = AccessPolicy { suffixes, ..AccessPolicy::default() };
        for raw in allow_origins.iter().flat_map(|s| s.split(',')) {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            if raw == "*" {
                policy.any_origin = true;
            } else if let Some(origin) = raw.contains("://").then(|| parse_origin(raw)).flatten() {
                let port = origin.effective_port();
                policy.rules.push(OriginRule::Exact { scheme: origin.scheme, host: origin.host.host, port });
            } else if let Some(suffix) = raw.strip_prefix("*.") {
                match parse_authority(suffix) {
                    Some(h) if h.ip.is_none() && h.port.is_none() => policy.rules.push(OriginRule::Suffix(h.host)),
                    _ => bail!("invalid --allow-origin '{raw}': use an origin such as http://localhost:3000, a host name such as app.example.com, *.example.com, or *"),
                }
            } else {
                match parse_authority(raw) {
                    Some(h) => policy.rules.push(OriginRule::Host { host: h.host, port: h.port }),
                    None => bail!("invalid --allow-origin '{raw}': use an origin such as http://localhost:3000, a host name such as app.example.com, *.example.com, or *"),
                }
            }
        }
        Ok(policy)
    }

    /// The DNS suffixes served besides the built-in names.
    pub fn suffixes(&self) -> &[String] {
        &self.suffixes
    }

    pub fn any_origin(&self) -> bool {
        self.any_origin
    }

    /// Is this host (name or IP, port already removed) one the emulator serves?
    fn host_served(&self, h: &HostPart) -> bool {
        if h.ip.is_some() {
            return true;
        }
        let host = h.host.as_str();
        if host == "localhost" || host.ends_with(".localhost") {
            return true;
        }
        // a single label with no dot: a Docker Compose service name, a LAN name
        if !host.contains('.') {
            return true;
        }
        if DOCKER_HOSTS.contains(&host) {
            return true;
        }
        self.suffixes.iter().any(|s| host == s || host.ends_with(&format!(".{s}")))
    }

    /// `Some(refusal)` when the request's host is not served. `authority` is what routing uses; the other
    /// `Host`/URI authority of the request, when there is one and it differs, is checked too.
    pub fn check_host(&self, authority: &str, others: &[&str]) -> Option<Response> {
        for a in std::iter::once(authority).chain(others.iter().copied()) {
            let ok = parse_authority(a).is_some_and(|h| self.host_served(&h));
            if !ok {
                return Some(host_refusal(a));
            }
        }
        None
    }

    fn origin_served(&self, origin: &str) -> bool {
        parse_origin(origin).is_some_and(|o| self.host_served(&o.host))
    }

    fn rule_allows(&self, origin: &str) -> bool {
        let Some(o) = parse_origin(origin) else { return false };
        self.rules.iter().any(|r| match r {
            OriginRule::Exact { scheme, host, port } => *scheme == o.scheme && *host == o.host.host && *port == o.effective_port(),
            OriginRule::Host { host, port } => *host == o.host.host && port.is_none_or(|p| p == o.effective_port()),
            OriginRule::Suffix(s) => o.host.host == *s || o.host.host.ends_with(&format!(".{s}")),
        })
    }

    /// Read `Origin`, `Referer` and `Sec-Fetch-Site`.
    pub fn classify(&self, headers: &HeaderMap) -> Browser {
        let get = |n: header::HeaderName| headers.get(n).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
        let sec = get(header::HeaderName::from_static("sec-fetch-site"));
        let origin_h = get(header::ORIGIN);
        let referer_h = get(header::REFERER);
        let mut b = Browser { sec_fetch_site: sec.clone(), ..Browser::default() };
        if sec.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("cross-site")) {
            b.cross_site = true;
            b.cross_site_fetch = true;
        }
        // an Origin header that is not valid UTF-8 counts as present and not served
        let origin_present = headers.contains_key(header::ORIGIN);
        match (origin_h, referer_h) {
            (Some(o), _) => {
                if !self.origin_served(&o) {
                    b.cross_site = true;
                }
                b.origin = Some(o);
            }
            (None, _) if origin_present => b.cross_site = true,
            (None, Some(r)) => {
                if !self.origin_served(&r) {
                    b.cross_site = true;
                }
                b.origin = parse_origin(&r).map(|o| format!("{}://{}", o.scheme, o.host.host));
            }
            (None, None) => {}
        }
        b.permitted = self.any_origin || b.origin.as_deref().is_some_and(|o| self.rule_allows(o));
        b
    }
}

/// 403 for a host the emulator does not serve; the body names the host and the option.
pub(crate) fn host_refusal(authority: &str) -> Response {
    let shown: String = authority.chars().filter(|c| !c.is_control()).take(120).collect();
    let name = parse_authority(authority).map(|h| h.host).unwrap_or_else(|| shown.clone());
    json_error::response(
        http::StatusCode::FORBIDDEN,
        "HostNotAllowed",
        &format!(
            "azemu refused this request: it is addressed to the host '{shown}', which this emulator does not serve. It answers to IP addresses, localhost and *.localhost, names with no dot (such as a Docker Compose service name), host.docker.internal, and names equal to or under a suffix given with --tls-name or --allow-host. Start azemu with --allow-host {name} (or set AZEMU_ALLOW_HOST={name}) to serve this name. The check stops DNS rebinding from web pages. If the aim is only to reach azemu from another container or machine without name set-up, start it with --standard-ports (every service gets a port of its own, so a name with no dot works: http://azemu:10000/{{account}}/... for Blob), or use a name such as {{account}}.blob.core.localhost on the gateway port."
        ),
    )
}

/// What a refused cross-site request was for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    Admin,
    Service,
    WebSocket,
}

/// 403 for a request another web site sent. No CORS headers: the other site must not be able to read it.
pub(crate) fn cross_site_refusal(b: &Browser, target: Target) -> Response {
    let origin = b.origin.as_deref().unwrap_or("(none)");
    let sec = b.sec_fetch_site.as_deref().unwrap_or("(none)");
    let origin: String = origin.chars().filter(|c| !c.is_control()).take(120).collect();
    let (what, how) = match target {
        Target::Admin => (
            "The azemu admin paths (/_azemu/...) refuse every request from another web site.",
            "Run the call from a program, or start azemu with --allow-origin '*' (or AZEMU_ALLOW_ORIGIN='*') to turn this check off.",
        ),
        Target::Service => (
            "The service has no CORS rule that accepts this origin.",
            "Allow the page with --allow-origin <origin> (or AZEMU_ALLOW_ORIGIN), or add a CORS rule for the origin to the storage account.",
        ),
        Target::WebSocket => (
            "A WebSocket connection from another web site is refused.",
            "Allow the page with --allow-origin <origin> (or AZEMU_ALLOW_ORIGIN).",
        ),
    };
    json_error::response(
        http::StatusCode::FORBIDDEN,
        "CrossSiteRequestRefused",
        &format!("azemu refused this request because another web site sent it (Origin: {origin}, Sec-Fetch-Site: {sec}). {what} {how}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(hosts: &[&str], origins: &[&str]) -> AccessPolicy {
        AccessPolicy::new(
            &hosts.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            vec![],
            &origins.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
        .unwrap()
    }

    fn served(p: &AccessPolicy, a: &str) -> bool {
        p.check_host(a, &[]).is_none()
    }

    #[test]
    fn the_host_check_serves_the_documented_names() {
        let p = policy(&["core.windows.net", "corp.example"], &[]);
        for ok in [
            "127.0.0.1:4577",
            "127.0.0.1",
            "[::1]:4577",
            "[::1]",
            "10.1.2.3:80",
            "192.168.0.5",
            "[fe80::1%eth0]:80",
            "localhost",
            "LOCALHOST:4577",
            "localhost.",
            "acct.blob.core.localhost:4577",
            "acct.dfs.core.windows.net.localhost:10000",
            "x.localhost",
            "azemu",
            "azemu:4577",
            "db_1",
            "host.docker.internal:10000",
            "gateway.docker.internal",
            "acct.blob.core.windows.net",
            "core.windows.net",
            "a.b.corp.example:443",
            "CORP.EXAMPLE",
        ] {
            assert!(served(&p, ok), "{ok} should be served");
        }
        for no in [
            "evil.example",
            "evil.example:4577",
            "acct.blob.attacker.example",
            "evil.com",
            "localhost.evil.com",
            "notlocalhost.com",
            "127.0.0.1.evil.com",
            "evil.localhost.example",
            "a.windows.net",
            "xcore.windows.net",
            "corp.example.evil.com",
            "",
            " ",
            "a b",
            "evil.com/x",
            "user@evil.com",
            "ev\\il.com",
            "[::1",
            "[acct.blob.localhost]",
            "[localhost]:80",
            "exa mple.com",
            "ü.example",
            ":4577",
            "host.docker.internal.evil.com",
            "evil.example.:80",
            "..",
            "a..b",
            "x.y:99999",
            "x.y:abc",
        ] {
            let r = p.check_host(no, &[]);
            assert!(r.is_some(), "{no:?} should be refused");
            assert_eq!(r.unwrap().status(), 403);
        }
        // without the options the real Azure names are not served
        let bare = policy(&[], &[]);
        assert!(!served(&bare, "acct.blob.core.windows.net"));
        assert!(!served(&bare, "management.azure.com"));
        // two hosts in one request: both must pass
        assert!(bare.check_host("localhost", &["evil.com"]).is_some());
        assert!(bare.check_host("localhost", &["127.0.0.1:1"]).is_none());
    }

    #[test]
    fn the_refusal_names_the_host_and_the_option() {
        let r = policy(&[], &[]).check_host("acct.blob.attacker.example:4577", &[]).unwrap();
        let body = String::from_utf8_lossy(r.body()).to_string();
        assert!(body.contains("acct.blob.attacker.example:4577"), "{body}");
        assert!(body.contains("--allow-host acct.blob.attacker.example"), "{body}");
        assert!(body.contains("AZEMU_ALLOW_HOST"), "{body}");
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(http::HeaderName::from_bytes(k.as_bytes()).unwrap(), http::HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn cross_site_is_sec_fetch_site_or_an_origin_that_fails_the_host_check() {
        let p = policy(&["corp.example"], &["http://localhost:3000", "app.example.org", "*.partner.test"]);
        let c = |pairs: &[(&str, &str)]| p.classify(&headers(pairs));
        // not cross-site: no headers (an SDK), same-origin, same-site, none, an origin that passes the host check
        assert!(!c(&[]).cross_site);
        assert!(!c(&[("sec-fetch-site", "same-origin")]).cross_site);
        assert!(!c(&[("sec-fetch-site", "same-site"), ("origin", "http://localhost:3000")]).cross_site);
        assert!(!c(&[("sec-fetch-site", "none")]).cross_site);
        assert!(!c(&[("origin", "http://127.0.0.1:8080")]).cross_site);
        assert!(!c(&[("origin", "https://app.corp.example")]).cross_site);
        assert!(!c(&[("origin", "http://azemu:80")]).cross_site);
        assert!(!c(&[("referer", "http://localhost:3000/page?x=1")]).cross_site);
        // cross-site
        assert!(c(&[("sec-fetch-site", "cross-site")]).cross_site);
        assert!(c(&[("sec-fetch-site", "cross-site")]).cross_site_fetch);
        assert!(c(&[("origin", "http://evil.example")]).cross_site);
        assert!(!c(&[("origin", "http://evil.example")]).cross_site_fetch, "an Origin alone does not block a service request");
        assert!(c(&[("origin", "null")]).cross_site);
        assert!(c(&[("origin", "chrome-extension://abcdefghijklmnop")]).cross_site);
        assert!(c(&[("origin", "http://localhost.evil.com")]).cross_site);
        assert!(c(&[("referer", "https://evil.example/page")]).cross_site);
        // the Origin wins over the Referer
        assert!(!c(&[("origin", "http://localhost:3000"), ("referer", "https://evil.example/page")]).cross_site);
        // allowed by --allow-origin: an exact origin (scheme, host and port), a host, a wildcard host
        assert!(c(&[("origin", "http://evil.example")]).origin.is_some());
        assert!(!c(&[("origin", "http://evil.example")]).permitted);
        assert!(c(&[("origin", "http://localhost:3000"), ("sec-fetch-site", "cross-site")]).permitted);
        assert!(!c(&[("origin", "http://localhost:3001"), ("sec-fetch-site", "cross-site")]).permitted);
        assert!(!c(&[("origin", "https://localhost:3000")]).permitted);
        assert!(c(&[("origin", "http://app.example.org")]).permitted);
        assert!(c(&[("origin", "https://app.example.org:8443")]).permitted);
        assert!(c(&[("origin", "https://a.b.partner.test")]).permitted);
        assert!(c(&[("origin", "https://partner.test")]).permitted);
        assert!(!c(&[("origin", "https://notpartner.test")]).permitted);
        // `*` allows everything (the check is off)
        let any = policy(&[], &["*"]);
        let b = any.classify(&headers(&[("sec-fetch-site", "cross-site")]));
        assert!(b.cross_site && b.permitted && any.any_origin());
    }

    #[test]
    fn host_and_origin_options_are_checked() {
        for bad in ["com", "*.com", "co.uk", "*", ".", "", "a..b", "a b", "xn--ü", "*.*.corp", "corp.*"] {
            if bad.is_empty() {
                continue; // an empty value is ignored
            }
            assert!(AccessPolicy::new(&[bad.to_string()], vec![], &[]).is_err(), "--allow-host {bad:?} must be refused");
        }
        assert!(AccessPolicy::new(
            &["azemu".into(), "corp.example".into(), "*.corp.example".into(), "core.windows.net".into(), "10.0.0.0/8".into()],
            vec![],
            &[]
        )
        .is_ok());
        assert!(AccessPolicy::new(&["a,b.example".into()], vec![], &[]).is_ok());
        assert!(AccessPolicy::new(&[], vec![], &["not a host".into()]).is_err());
        assert!(AccessPolicy::new(&[], vec![], &["http://localhost:3000,*.x.test,*".into()]).is_ok());
    }

    #[test]
    fn suffix_and_range_rules() {
        for ok in ["azemu", "corp", "db", "vault.corp.example", "*.mail.example", ".internal", "localhost", "core.windows.net"] {
            assert!(parse_dns_suffix(ok, "--tls-name").is_ok(), "{ok}");
        }
        for bad in ["com", "COM", "*.com", ".com", "co.uk", "io", "github.io", "*", "a..b", ""] {
            assert!(parse_dns_suffix(bad, "--tls-name").is_err(), "{bad}");
        }
        let v4 = |s: &str| IpAddr::from_str(s).unwrap();
        assert!(check_ip_range(v4("10.0.0.0"), 8, "10.0.0.0/8", "--tls-name").is_ok());
        assert!(check_ip_range(v4("10.0.0.0"), 7, "10.0.0.0/7", "--tls-name").is_err());
        assert!(check_ip_range(v4("0.0.0.0"), 0, "0.0.0.0/0", "--tls-name").is_err());
        assert!(check_ip_range(v4("fd00::"), 32, "fd00::/32", "--tls-name").is_ok());
        assert!(check_ip_range(v4("::"), 0, "::/0", "--tls-name").is_err());
        assert!(check_ip_range(v4("fd00::"), 31, "fd00::/31", "--tls-name").is_err());
    }
}
