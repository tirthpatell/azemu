//! Choosing the service, account and path for a request.

use std::sync::Arc;

use crate::service::{Mount, Service};

/// Result of routing: which service gets the request and with what account/path.
pub(crate) struct Routed {
    pub service: Arc<dyn Service>,
    pub account: String,
    /// Path with the account segment removed (path style only).
    pub path: String,
    pub path_style: bool,
}

/// `Host` header value to a lowercase host without the port.
pub(crate) fn host_only(authority: &str) -> String {
    let a = authority.trim();
    let host = if let Some(rest) = a.strip_prefix('[') {
        // [::1]:4577
        match rest.find(']') {
            Some(i) => &a[..i + 2],
            None => a,
        }
    } else {
        match a.rsplit_once(':') {
            Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => h,
            _ => a,
        }
    };
    host.to_ascii_lowercase()
}

/// Strip a trailing `-secondary` (read-only secondary endpoint of a storage account).
fn account_from_label(label: &str) -> String {
    let l = label.to_ascii_lowercase();
    match l.strip_suffix("-secondary") {
        Some(base) if !base.is_empty() => base.to_string(),
        _ => l,
    }
}

/// Match `{account}.{label}.<anything>` against a service's `AccountHost` mounts.
fn match_account_host(svc: &dyn Service, labels: &[&str]) -> Option<String> {
    if labels.len() < 2 {
        return None;
    }
    svc.mounts()
        .iter()
        .any(|m| matches!(m, Mount::AccountHost(l) if l.eq_ignore_ascii_case(labels[1])))
        .then(|| account_from_label(labels[0]))
}

fn match_host(svc: &dyn Service, labels: &[&str]) -> bool {
    svc.mounts().iter().any(|m| matches!(m, Mount::Host(l) if l.eq_ignore_ascii_case(labels[0])))
}

/// Collapse runs of `/` into one, so `//metadata/endpoints` is `/metadata/endpoints`.
/// Only used to match `PathPrefix` mounts and admin paths; the service gets the path as sent.
pub(crate) fn collapse_slashes(path: &str) -> std::borrow::Cow<'_, str> {
    if !path.contains("//") {
        return std::borrow::Cow::Borrowed(path);
    }
    let mut out = String::with_capacity(path.len());
    let mut prev_slash = false;
    for c in path.chars() {
        if c == '/' {
            if prev_slash {
                continue;
            }
            prev_slash = true;
        } else {
            prev_slash = false;
        }
        out.push(c);
    }
    std::borrow::Cow::Owned(out)
}

/// Longest `PathPrefix` mount that `path` starts with. With `admin_only`, only mounts under
/// `/_azemu/` count. Empty path segments in `path` are ignored.
fn match_prefix<'a>(services: &'a [Arc<dyn Service>], path: &str, admin_only: bool) -> Option<&'a Arc<dyn Service>> {
    let path = collapse_slashes(path);
    let mut best: Option<(&Arc<dyn Service>, usize)> = None;
    for s in services {
        for m in s.mounts() {
            if let Mount::PathPrefix(p) = m {
                if admin_only && !p.starts_with(ADMIN_PREFIX) {
                    continue;
                }
                if path.starts_with(p) && best.is_none_or(|(_, len)| p.len() > len) {
                    best = Some((s, p.len()));
                }
            }
        }
    }
    best.map(|(s, _)| s)
}

/// Paths under this prefix are the emulator's own and are routed whatever the host is.
pub(crate) const ADMIN_PREFIX: &str = "/_azemu/";

/// Gateway routing: steps 1 to 4 of how a request finds its service (see the module comment).
/// `Err` carries the 404 message.
pub(crate) fn route_gateway(services: &[Arc<dyn Service>], path: &str, authority: &str, port: u16) -> Result<Routed, String> {
    let whole = |s: &Arc<dyn Service>, account: String| Routed { service: s.clone(), account, path: path.to_string(), path_style: false };
    // 1. the emulator's own paths, on any host
    if collapse_slashes(path).starts_with(ADMIN_PREFIX) {
        if let Some(s) = match_prefix(services, path, true) {
            return Ok(whole(s, String::new()));
        }
    }
    // 2. a host that names a service decides, whatever the path. `{account}.{label}` goes
    // before `{label}`, so a storage account called `login` is still a storage account.
    let host = host_only(authority);
    let labels: Vec<&str> = host.split('.').collect();
    for s in services {
        if let Some(account) = match_account_host(s.as_ref(), &labels) {
            return Ok(whole(s, account));
        }
    }
    for s in services {
        if match_host(s.as_ref(), &labels) {
            return Ok(whole(s, String::new()));
        }
    }
    // 3. the host names no service (localhost, an IP address, a name nobody mounted): the path
    if let Some(s) = match_prefix(services, path, false) {
        return Ok(whole(s, String::new()));
    }
    Err(no_service_message(services, &host, path, port))
}

/// Pinned routing: everything goes to `svc`.
pub(crate) fn route_pinned(svc: &Arc<dyn Service>, path: &str, authority: &str) -> Routed {
    let host = host_only(authority);
    let labels: Vec<&str> = host.split('.').collect();
    if let Some(account) = match_account_host(svc.as_ref(), &labels) {
        return Routed { service: svc.clone(), account, path: path.to_string(), path_style: false };
    }
    if svc.path_style_on_pinned() {
        let trimmed = path.strip_prefix('/').unwrap_or(path);
        let (first, rest) = match trimmed.find('/') {
            Some(i) => (&trimmed[..i], &trimmed[i..]),
            None => (trimmed, ""),
        };
        if !first.is_empty() {
            return Routed {
                service: svc.clone(),
                account: first.to_string(),
                path: if rest.is_empty() { "/".to_string() } else { rest.to_string() },
                path_style: true,
            };
        }
    }
    // A host name with no dot (a Docker Compose service name or network alias, `azemu`) names the account, vault, store,
    // topic or namespace, the way `{account}.{label}.localhost` does on the gateway. It keeps the HTTP side of a
    // pinned port in step with AMQP, where the namespace is the first label of the host the client dialled; and a
    // second alias gives a second vault. `localhost` and IP addresses use the service's default.
    let account = if svc.path_style_on_pinned() { None } else { bare_host_name(&host) };
    Routed {
        service: svc.clone(),
        account: account.unwrap_or_else(|| svc.default_account().to_string()),
        path: path.to_string(),
        path_style: false,
    }
}

/// The host as an account name when it is one label that is not `localhost` and not an address.
fn bare_host_name(host: &str) -> Option<String> {
    let ok = !host.is_empty()
        && !host.contains(['.', ':', '[', ']'])
        && host != "localhost"
        && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then(|| host.to_string())
}

/// The labels that Azure's storage names carry `.core.` after (`acct.blob.core.windows.net`). The Python storage SDK
/// treats a host that contains `localhost` and not `.core.` as an Azurite address with the account in the path.
fn needs_core_label(label: &str) -> bool {
    matches!(label, "blob" | "queue" | "table" | "dfs" | "file")
}

fn no_service_message(services: &[Arc<dyn Service>], host: &str, path: &str, port: u16) -> String {
    let mut parts = Vec::new();
    for s in services {
        let mut forms = Vec::new();
        for m in s.mounts() {
            forms.push(match m {
                Mount::AccountHost(l) if needs_core_label(l) => format!("http://{{account}}.{l}.core.localhost:{port}"),
                Mount::AccountHost(l) => format!("http://{{account}}.{l}.localhost:{port}"),
                Mount::Host(l) => format!("http://{l}.localhost:{port}"),
                Mount::PathPrefix(p) => format!("http://localhost:{port}{p}"),
            });
        }
        parts.push(format!("{} ({})", s.name(), forms.join(" or ")));
    }
    let mounted = if parts.is_empty() { "none".to_string() } else { parts.join("; ") };
    let standard: Vec<String> = crate::cli::STANDARD_PORTS
        .iter()
        .filter(|(n, _)| services.iter().any(|s| s.name() == *n))
        .map(|(n, p)| format!("{n} {p}"))
        .collect();
    format!(
        "No service matched host '{host}' and path '{path}'. Mounted services: {mounted}. \
         Use the URL form shown for the service you want. \
         If names under .localhost do not resolve where your client runs (inside a container, on Windows, on macOS before 26), \
         start azemu with --standard-ports: every service then has a port of its own and any host name works, \
         for example http://azemu:10000/{{account}}/{{container}} for Blob (ports: {}). \
         A hosts-file line or a Docker Compose network alias for the name also works.",
        standard.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_parsing() {
        assert_eq!(host_only("Acct.Blob.localhost:4577"), "acct.blob.localhost");
        assert_eq!(host_only("localhost"), "localhost");
        assert_eq!(host_only("[::1]:4577"), "[::1]");
        assert_eq!(host_only("[::1]"), "[::1]");
        assert_eq!(host_only("127.0.0.1:80"), "127.0.0.1");
        assert_eq!(host_only(""), "");
        assert_eq!(host_only("weird:port"), "weird:port");
    }

    #[test]
    fn a_bare_host_name_is_an_account_name() {
        assert_eq!(bare_host_name("azemu").as_deref(), Some("azemu"));
        assert_eq!(bare_host_name("my-vault_2").as_deref(), Some("my-vault_2"));
        for no in ["localhost", "", "127.0.0.1", "[::1]", "a.b", "host.docker.internal", "bad name", "x:1"] {
            assert_eq!(bare_host_name(no), None, "{no:?}");
        }
    }

    #[test]
    fn secondary_suffix() {
        assert_eq!(account_from_label("acct-secondary"), "acct");
        assert_eq!(account_from_label("ACCT"), "acct");
        assert_eq!(account_from_label("-secondary"), "-secondary");
        assert_eq!(account_from_label("a-secondary-secondary"), "a-secondary");
    }
}
