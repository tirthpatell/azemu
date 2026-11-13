//! Managed identity endpoints: App Service style (`IDENTITY_ENDPOINT`), IMDS
//! (Azure VM style) and the helper `GET /_azemu/identity/token`.
//!
//! The Azure Identity libraries do not look at the token endpoint host for these
//! (they only need the environment variables), which makes this the easiest way to
//! point an application at the emulator without a code change.

use azemu_core::resp::Resp;
use azemu_core::{json_error, Request, Response};
use serde_json::json;

use crate::jwt::{self, AccessToken, ClientAuth};
use crate::util::{default_tenant, derive_guid, tenant_guid, Params};
use crate::IdentityService;

const APP_SERVICE_API_VERSION: &str = "2019-08-01";

/// Token lifetime in seconds: 1 hour, or `?expires_in=N` (1 second to 7 days).
pub fn lifetime(p: &Params) -> i64 {
    p.nonempty("expires_in").and_then(|v| v.parse::<i64>().ok()).filter(|n| *n > 0).map(|n| n.min(7 * 86400)).unwrap_or(3600)
}

/// Which managed identity a request names. The emulator has no registry of
/// identities: every client id, object id or resource id is "known" and maps to a
/// stable principal.
pub struct Identity {
    pub client_id: String,
    pub oid: Option<String>,
}

pub fn resolve_identity(client_id: Option<&str>, object_id: Option<&str>, res_id: Option<&str>) -> Identity {
    if let Some(c) = client_id {
        Identity { client_id: c.to_ascii_lowercase(), oid: None }
    } else if let Some(o) = object_id {
        Identity { client_id: derive_guid("mi-client-from-object", &o.to_ascii_lowercase()), oid: Some(o.to_ascii_lowercase()) }
    } else if let Some(r) = res_id {
        Identity { client_id: derive_guid("mi-client-from-resource", &r.to_ascii_lowercase()), oid: None }
    } else {
        Identity { client_id: derive_guid("mi-client", "system-assigned"), oid: None }
    }
}

async fn mint(
    svc: &IdentityService,
    req: &Request,
    id: &Identity,
    tenant_id: &str,
    resource: &str,
    lifetime: i64,
) -> Result<(String, i64), String> {
    let key = svc.signing_key().await?;
    let iat = req.ctx.time.unix_timestamp();
    let claims = jwt::access_claims(&AccessToken {
        tenant_id,
        aud: resource,
        client_id: &id.client_id,
        auth: ClientAuth::Assertion,
        user: None,
        scp: None,
        oid: id.oid.as_deref(),
        iat,
        lifetime,
    });
    Ok((jwt::sign(&key, &claims)?, iat))
}

fn app_service_error(req: &Request, status: u16, message: &str) -> Response {
    Resp::new(status).json(&json!({"statusCode": status, "message": message, "correlationId": req.ctx.request_id})).build()
}

fn imds_error(status: u16, error: &str, description: &str) -> Response {
    Resp::new(status).json(&json!({"error": error, "error_description": description})).build()
}

/// `GET /_azemu/identity/msi/token?resource=…&api-version=2019-08-01`
pub async fn app_service(svc: &IdentityService, req: &Request) -> Response {
    if req.method != "GET" {
        return json_error::response(405, "MethodNotAllowed", "The managed identity endpoint only accepts GET.");
    }
    // The real value is a secret that the platform puts in IDENTITY_HEADER. Any
    // non-empty value is accepted here.
    if req.header("x-identity-header").map(str::trim).filter(|v| !v.is_empty()).is_none() {
        return app_service_error(
            req,
            400,
            "Required header X-IDENTITY-HEADER is missing. Set it to the value of the IDENTITY_HEADER environment variable.",
        );
    }
    let p = Params::parse(req.query.raw());
    match p.nonempty("api-version") {
        None => return app_service_error(req, 400, "The 'api-version' query parameter is required."),
        Some(v) if v != APP_SERVICE_API_VERSION => {
            return app_service_error(
                req,
                400,
                &format!("The api-version '{v}' is not supported. The supported version is '{APP_SERVICE_API_VERSION}'."),
            );
        }
        _ => {}
    }
    let Some(resource) = p.nonempty("resource") else {
        return app_service_error(req, 400, "The 'resource' query parameter is required.");
    };
    let client_id = p.nonempty("client_id");
    let object_id = p.nonempty("principal_id").or_else(|| p.nonempty("object_id"));
    let res_id = p.nonempty("mi_res_id");
    if [client_id, object_id, res_id].iter().flatten().count() > 1 {
        return app_service_error(req, 400, "Only one of client_id, principal_id, object_id and mi_res_id can be specified.");
    }
    let id = resolve_identity(client_id, object_id, res_id);
    let life = lifetime(&p);
    match mint(svc, req, &id, &default_tenant(), resource, life).await {
        Ok((token, iat)) => Resp::new(200u16)
            .json(&json!({
                "access_token": token,
                "expires_on": (iat + life).to_string(),
                "resource": resource,
                "token_type": "Bearer",
                "client_id": id.client_id,
            }))
            .build(),
        Err(e) => app_service_error(req, 500, &format!("An unexpected error occurred while fetching the token: {e}")),
    }
}

/// `GET /metadata/identity/oauth2/token?api-version=2018-02-01&resource=…` with `Metadata: true`.
///
/// azure-identity probes IMDS with this same request and no `Metadata` header; the
/// 400 answer (any HTTP answer) tells it IMDS is there.
pub async fn imds(svc: &IdentityService, req: &Request) -> Response {
    if req.method != "GET" {
        return imds_error(405, "bad_request_102", "Only GET is supported.");
    }
    if req.header("metadata") != Some("true") {
        return imds_error(400, "bad_request_102", "Required metadata header not specified");
    }
    let p = Params::parse(req.query.raw());
    if p.nonempty("api-version").is_none() {
        return imds_error(400, "invalid_request", "Required query variable 'api-version' is missing");
    }
    let Some(resource) = p.nonempty("resource") else {
        return imds_error(400, "invalid_request", "Required query variable 'resource' is missing");
    };
    let client_id = p.nonempty("client_id");
    let object_id = p.nonempty("object_id");
    let res_id = p.nonempty("msi_res_id").or_else(|| p.nonempty("mi_res_id"));
    if [client_id, object_id, res_id].iter().flatten().count() > 1 {
        return imds_error(400, "invalid_request", "Only one of client_id, object_id and msi_res_id can be specified.");
    }
    let id = resolve_identity(client_id, object_id, res_id);
    let life = lifetime(&p);
    match mint(svc, req, &id, &default_tenant(), resource, life).await {
        Ok((token, iat)) => Resp::new(200u16)
            .json(&json!({
                "access_token": token,
                "client_id": id.client_id,
                "expires_in": life.to_string(),
                "expires_on": (iat + life).to_string(),
                "ext_expires_in": life.to_string(),
                "not_before": iat.to_string(),
                "resource": resource,
                "token_type": "Bearer",
            }))
            .build(),
        Err(e) => imds_error(500, "unknown", &format!("Failed to retrieve token: {e}")),
    }
}

/// `GET /_azemu/identity/token?resource=…` (or `scope=`): a token for scripts and curl.
/// Optional: `client_id`, `tenant`, `expires_in`.
pub async fn helper(svc: &IdentityService, req: &Request) -> Response {
    if req.method != "GET" {
        return json_error::response(405, "MethodNotAllowed", "Use GET.");
    }
    let p = Params::parse(req.query.raw());
    let Some(wanted) = p.nonempty("resource").or_else(|| p.nonempty("scope")) else {
        return json_error::response(400, "MissingParameter", "Pass resource=<resource> or scope=<resource>/.default.");
    };
    // `https://storage.azure.com/.default` and `https://storage.azure.com` both mean the same resource.
    let first = wanted.split_whitespace().next().unwrap_or(wanted);
    let aud = first.strip_suffix("/.default").unwrap_or(first);
    let tenant_id = p.nonempty("tenant").map(tenant_guid).unwrap_or_else(default_tenant);
    let id = resolve_identity(p.nonempty("client_id"), None, None);
    let life = lifetime(&p);
    match mint(svc, req, &id, &tenant_id, aud, life).await {
        Ok((token, iat)) => Resp::new(200u16)
            .json(&json!({
                "access_token": token,
                "token_type": "Bearer",
                "expires_in": life,
                "expires_on": iat + life,
                "resource": aud,
                "client_id": id.client_id,
                "tenant_id": tenant_id,
            }))
            .build(),
        Err(e) => json_error::response(500, "InternalError", &e),
    }
}
