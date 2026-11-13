//! The challenge handshake, `api-version` check and the headers every response carries.

use azemu_core::resp::Resp;
use azemu_core::{json_error, Request, Response};

const TENANT: &str = "00000000-0000-0000-0000-000000000000";
const SERVICE_VERSION: &str = "1.9.2455.1";
const REGION: &str = "westus";

/// Host name endings of the real Key Vault and Managed HSM services in every Azure cloud. A request that arrives under one
/// of them (a hosts-file set-up) is answered with the real resource name.
const AZURE_VAULT_SUFFIXES: &[&str] = &[
    "vault.azure.net",
    "vault.azure.cn",
    "vault.usgovcloudapi.net",
    "vault.microsoftazure.de",
    "managedhsm.azure.net",
    "managedhsm.azure.cn",
    "managedhsm.usgovcloudapi.net",
    "vault.microsoft.scloud",
    "vault.cloud.microsoft",
];

/// The `resource` of the challenge for a request that came to `authority` (the `Host` value, with its port).
///
/// The clients refuse a challenge unless the host they called ends with a dot and the host of the resource, so the
/// resource is the request's own domain: `myvault.vault.localhost:4577` gets `https://vault.localhost:4577` (the host
/// without its first label, the port kept; Python and Go compare with the port, JavaScript and the others without).
/// Under a real Azure suffix the real name is kept (`https://vault.azure.net`). A host without a dot
/// (`azemu:10010`, the standard-port form) has no name that passes in any client, so it also gets
/// `https://vault.azure.net`; the same for an IPv6 literal. An IPv4 address loses its first number
/// (`127.0.0.1:10010` gets `https://0.0.1:10010`): the Python and Go checks pass, JavaScript's does not.
pub fn challenge_resource(authority: &str) -> String {
    const REAL: &str = "https://vault.azure.net";
    let authority = authority.trim().to_ascii_lowercase();
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !authority.starts_with('[') && p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
        _ => (authority.as_str(), None),
    };
    let host = host.trim_end_matches('.');
    if host.starts_with('[') || host.is_empty() {
        return REAL.into();
    }
    if AZURE_VAULT_SUFFIXES.iter().any(|s| host == *s || host.ends_with(&format!(".{s}"))) {
        return REAL.into();
    }
    let Some((_, domain)) = host.split_once('.') else { return REAL.into() };
    if domain.is_empty() {
        return REAL.into();
    }
    match port {
        Some(p) => format!("https://{domain}:{p}"),
        None => format!("https://{domain}"),
    }
}

/// The 401 every request without a bearer token gets, whether or not it has a body.
fn challenge(req: &Request) -> Response {
    // The wording does not depend on the `api-version`. The recordings from before March 2026 say "a Bearer or PoP token" for every
    // version (`7.0` to `7.6`, `2016-10-01`), the ones from March 2026 on say "a Bearer, PoP, or MTLS_POP token" for every version,
    // also `7.4` and `7.5` (recorded 2026-10-28: `test_key_client.pyTestKeyClienttest_40x_handling[7.4_vault].json` entry 0). The
    // service changed over time; the emulator answers as the service does now.
    let message = "AKV10000: Request is missing a Bearer, PoP, or MTLS_POP token.";
    let mut resp = json_error::response(401, "Unauthorized", message);
    resp.headers_mut().insert(
        "WWW-Authenticate",
        http_header(&format!(
            "Bearer authorization=\"https://login.microsoftonline.com/{TENANT}\", resource=\"{}\"",
            challenge_resource(&req.ctx.endpoint.authority)
        )),
    );
    resp
}

fn http_header(v: &str) -> azemu_core::http::HeaderValue {
    azemu_core::http::HeaderValue::from_str(v).unwrap_or_else(|_| azemu_core::http::HeaderValue::from_static(""))
}

/// `7.0`, `7.5-preview.1`, `2016-10-01`, `2025-07-01`, `2026-01-01-preview`.
fn valid_api_version(v: &str) -> bool {
    let is_digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let (core, preview) = match v.split_once("-preview") {
        Some((c, rest)) => (c, Some(rest)),
        None => (v, None),
    };
    if let Some(rest) = preview {
        let ok = rest.is_empty() || rest.strip_prefix('.').map(is_digits).unwrap_or(false);
        if !ok {
            return false;
        }
    }
    let dotted: Vec<&str> = core.split('.').collect();
    if dotted.len() == 2 && dotted.iter().all(|p| is_digits(p)) {
        return true;
    }
    let dashed: Vec<&str> = core.split('-').collect();
    dashed.len() == 3 && dashed[0].len() == 4 && dashed[1].len() == 2 && dashed[2].len() == 2 && dashed.iter().all(|p| is_digits(p))
}

/// Checks done before routing. `Some(response)` stops the request.
pub fn precheck(req: &Request) -> Option<Response> {
    let bearer = req
        .header("authorization")
        .map(|h| {
            let h = h.trim().as_bytes();
            h.len() > 7 && h[..7].eq_ignore_ascii_case(b"bearer ")
        })
        .unwrap_or(false);
    if !bearer {
        return Some(challenge(req));
    }
    match req.query.get("api-version") {
        None | Some("") => {
            Some(crate::errors::bad_parameter("The api-version query parameter (?api-version=) is required for all requests."))
        }
        Some(v) if !valid_api_version(v) => Some(crate::errors::bad_parameter(&format!("Invalid api-version '{v}'."))),
        Some(_) => None,
    }
}

/// Add the headers real Key Vault sends on every response.
pub fn finish(mut resp: Response, req: &Request) -> Response {
    let status = resp.status().as_u16();
    let h = resp.headers_mut();
    let mut set = |k: &'static str, v: &str| {
        if !h.contains_key(k) {
            h.insert(k, http_header(v));
        }
    };
    if status != 204 && status != 304 {
        set("content-type", "application/json; charset=utf-8");
    }
    set("cache-control", "no-cache");
    set("pragma", "no-cache");
    set("expires", "-1");
    set("strict-transport-security", "max-age=31536000;includeSubDomains");
    set("x-content-type-options", "nosniff");
    set("x-ms-keyvault-network-info", "conn_type=Ipv4;addr=127.0.0.1;act_addr_fam=InterNetwork;");
    set("x-ms-keyvault-region", REGION);
    set("x-ms-keyvault-service-version", SERVICE_VERSION);
    set("x-ms-request-id", &req.ctx.request_id);
    if let Some(id) = req.header("x-ms-client-request-id") {
        set("x-ms-client-request-id", id);
    }
    resp
}

/// 204 with no body (purge).
pub fn no_content() -> Response {
    Resp::new(204u16).build()
}

#[cfg(test)]
mod tests {
    use super::challenge_resource;

    #[test]
    fn the_resource_is_the_request_domain() {
        assert_eq!(challenge_resource("myvault.vault.localhost:4577"), "https://vault.localhost:4577");
        assert_eq!(challenge_resource("MyVault.vault.localhost:4577"), "https://vault.localhost:4577");
        assert_eq!(challenge_resource("v.vault.localhost"), "https://vault.localhost");
        assert_eq!(challenge_resource("v.vault.example.test:8443"), "https://vault.example.test:8443");
        // IPv4: the first number goes
        assert_eq!(challenge_resource("127.0.0.1:10010"), "https://0.0.1:10010");
    }

    #[test]
    fn real_azure_names_keep_the_real_resource() {
        for h in [
            "myvault.vault.azure.net",
            "myvault.vault.azure.net:443",
            "x.managedhsm.azure.net",
            "x.vault.usgovcloudapi.net",
            "x.vault.azure.cn",
        ] {
            assert_eq!(challenge_resource(h), "https://vault.azure.net", "{h}");
        }
    }

    #[test]
    fn hosts_no_client_accepts_get_the_default() {
        for h in ["azemu:10010", "azemu", "localhost:4577", "[::1]:4577", "", ":4577"] {
            assert_eq!(challenge_resource(h), "https://vault.azure.net", "{h:?}");
        }
    }
}
