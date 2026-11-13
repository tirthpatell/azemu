//! Entra ID discovery documents, key set, instance discovery, user realm
//! discovery and the authorize endpoint. All URLs are built from the address the
//! client used, so they point back at the emulator.

use azemu_core::resp::Resp;
use azemu_core::{Request, Response};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Value};

use crate::jwt::DEFAULT_USER;
use crate::token::{entra_error, parse_scope, seal, Blob, AC_PREFIX};
use crate::util::{tenant_guid, Params};
use crate::IdentityService;

/// `GET /{tenant}/v2.0/.well-known/openid-configuration` and the v1 form.
pub fn openid_config(base: &str, tenant: &str, v1: bool) -> Response {
    let tid = tenant_guid(tenant);
    let t = format!("{base}/{tenant}");
    let doc: Value = if v1 {
        json!({
            "token_endpoint": format!("{t}/oauth2/token"),
            "token_endpoint_auth_methods_supported": ["client_secret_post", "private_key_jwt", "client_secret_basic"],
            "jwks_uri": format!("{t}/discovery/keys"),
            "response_modes_supported": ["query", "fragment", "form_post"],
            "subject_types_supported": ["pairwise"],
            "id_token_signing_alg_values_supported": ["RS256"],
            "response_types_supported": ["code", "id_token", "code id_token", "token id_token", "token"],
            "scopes_supported": ["openid"],
            "issuer": format!("https://sts.windows.net/{tid}/"),
            "authorization_endpoint": format!("{t}/oauth2/authorize"),
            "device_authorization_endpoint": format!("{t}/oauth2/devicecode"),
            "claims_supported": ["sub", "iss", "aud", "exp", "iat", "nbf", "tid", "oid", "upn", "unique_name", "name"],
            "tenant_region_scope": "NA",
        })
    } else {
        json!({
            "token_endpoint": format!("{t}/oauth2/v2.0/token"),
            "token_endpoint_auth_methods_supported": ["client_secret_post", "private_key_jwt", "client_secret_basic"],
            "jwks_uri": format!("{t}/discovery/v2.0/keys"),
            "response_modes_supported": ["query", "fragment", "form_post"],
            "subject_types_supported": ["pairwise"],
            "id_token_signing_alg_values_supported": ["RS256"],
            "response_types_supported": ["code", "id_token", "code id_token", "id_token token"],
            "scopes_supported": ["openid", "profile", "email", "offline_access"],
            "issuer": format!("{base}/{tid}/v2.0"),
            "request_uri_parameter_supported": false,
            "authorization_endpoint": format!("{t}/oauth2/v2.0/authorize"),
            "device_authorization_endpoint": format!("{t}/oauth2/v2.0/devicecode"),
            "claims_supported": ["sub", "iss", "aud", "exp", "iat", "nbf", "tid", "oid", "name", "preferred_username", "nonce", "ver"],
            "tenant_region_scope": "NA",
        })
    };
    Resp::new(200u16).json(&doc).build()
}

/// `GET /{tenant}/discovery/v2.0/keys`, `/{tenant}/discovery/keys`.
pub async fn keys(svc: &IdentityService, req: &Request) -> Response {
    match svc.signing_key().await {
        Ok(k) => Resp::new(200u16).json(&json!({"keys": [k.jwk()]})).build(),
        Err(e) => entra_error(req, 500, "server_error", 90033, &e),
    }
}

/// `GET /common/discovery/instance?api-version=1.1&authorization_endpoint=…`.
/// The emulator is its own (only) network.
pub fn instance_discovery(req: &Request, base: &str) -> Response {
    let p = Params::parse(req.query.raw());
    // Tenant from the endpoint the client is asking about; `common` when absent.
    let tenant = p
        .nonempty("authorization_endpoint")
        .and_then(|u| u.split_once("://").map(|(_, rest)| rest))
        .and_then(|rest| rest.split('/').nth(1))
        .filter(|t| !t.is_empty())
        .unwrap_or("common");
    let host = req
        .ctx
        .endpoint
        .authority
        .rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
        .map(|(h, _)| h)
        .unwrap_or(&req.ctx.endpoint.authority);
    Resp::new(200u16)
        .json(&json!({
            "tenant_discovery_endpoint": format!("{base}/{tenant}/v2.0/.well-known/openid-configuration"),
            "api-version": "1.1",
            "metadata": [{
                "preferred_network": host,
                "preferred_cache": host,
                "aliases": [host],
            }],
        }))
        .build()
}

/// `GET /common/userrealm/{user}`: every user is a managed (password) user.
pub fn user_realm(user: &str) -> Response {
    let domain = user.split_once('@').map(|(_, d)| d).unwrap_or("azemu.onmicrosoft.com");
    Resp::new(200u16)
        .json(&json!({
            "ver": "1.0",
            "account_type": "Managed",
            "domain_name": domain,
            "cloud_instance_name": "microsoftonline.com",
            "cloud_audience_urn": "urn:federation:MicrosoftOnline",
        }))
        .build()
}

/// The page named by `verification_uri` in the device code response.
pub fn device_auth_page() -> Response {
    Resp::new(200u16)
        .header("Content-Type", "text/html; charset=utf-8")
        .body("<html><body><h1>azemu</h1><p>Device codes are approved automatically. Nothing to do here.</p></body></html>")
        .build()
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

/// `GET /{tenant}/oauth2/v2.0/authorize`: signs in the default user (or `login_hint`)
/// at once and redirects to `redirect_uri` with `code` and `state` in the query.
pub fn authorize(req: &Request, tenant: &str, v1: bool) -> Response {
    let p = Params::parse(req.query.raw());
    let Some(client_id) = p.nonempty("client_id") else {
        return entra_error(req, 400, "invalid_request", 900144, "The request must contain the following parameter: 'client_id'.");
    };
    let Some(redirect) = p.nonempty("redirect_uri") else {
        return entra_error(req, 400, "invalid_request", 900144, "The request must contain the following parameter: 'redirect_uri'.");
    };
    let state_param = p.nonempty("state").map(|s| format!("&state={}", enc(s))).unwrap_or_default();
    let sep = if redirect.contains('?') { '&' } else { '?' };
    let redirect_err = |error: &str, desc: &str| {
        Resp::new(302u16).header("Location", format!("{redirect}{sep}error={error}&error_description={}{state_param}", enc(desc))).build()
    };
    if p.nonempty("response_type") != Some("code") {
        return redirect_err("unsupported_response_type", "azemu: only response_type=code is built");
    }
    if p.nonempty("response_mode").is_some_and(|m| m != "query") {
        return redirect_err("invalid_request", "azemu: only response_mode=query is built");
    }
    let raw = if v1 { p.nonempty("resource").unwrap_or("") } else { p.nonempty("scope").unwrap_or("") };
    if let Err((e, _, m)) = parse_scope(raw, v1, p.get("scope").unwrap_or("")) {
        return redirect_err(e, &m);
    }
    let user = p.nonempty("login_hint").unwrap_or(DEFAULT_USER);
    let code = seal(
        AC_PREFIX,
        &Blob {
            c: client_id.into(),
            s: raw.into(),
            t: tenant.into(),
            u: user.into(),
            n: p.nonempty("nonce").map(str::to_string),
            x: req.ctx.time.unix_timestamp() + 600,
            v1,
        },
    );
    Resp::new(302u16)
        .header("Location", format!("{redirect}{sep}code={}{state_param}", enc(&code)))
        .header("Cache-Control", "no-store")
        .build()
}
