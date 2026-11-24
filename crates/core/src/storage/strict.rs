//! What Azure Storage answers in strict mode (`docs/strict-auth.md`) to a bearer token that is wrong and to a role that is missing,
//! for Blob, Queue, Table and Data Lake. Checked against real Azure in November 2026 (`docs/strict-auth.md`, "Storage").

use crate::auth::{Aud, Auth, Need, Principal, Resource, TokenFailure};
use crate::resp::Resp;
use crate::storage::credentials::is_date;
use crate::storage::{error, error_with_details};
use crate::{Request, Response};

/// The four services that share these answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Blob,
    Queue,
    Table,
    DataLake,
}

impl Kind {
    /// The first `x-ms-version` that takes OAuth tokens at all.
    const OAUTH_FROM: &'static str = "2017-11-09";

    /// The first `x-ms-version` that answers a bad token with 401 and a challenge (older ones: 403 `AuthenticationFailed`).
    fn challenge_from(self) -> &'static str {
        match self {
            Kind::Blob | Kind::Queue => "2019-12-12",
            Kind::Table => "2020-12-06",
            Kind::DataLake => Self::OAUTH_FROM,
        }
    }
}

/// The audiences Storage accepts for an account (`docs/strict-auth.md`, "Audiences"): the service's own (`https://storage.azure.com`, which is
/// what the SDKs ask for) and the account's own URL, `https://{account}.{service}.{domain}`. Azure took both on a Blob endpoint
/// (November 2026). The account's URL has to be the whole host of that service: its Azure domain (the public cloud and the sovereign ones), or
/// the emulator's own (`{account}.blob.core.localhost`, `{account}.blob.localhost`, and the Azure name followed by `.localhost`). Only Blob
/// was tried; the other three services take the same form for their own endpoint.
pub fn audiences(kind: Kind, account: &str) -> Vec<Aud> {
    let services: &[&str] = match kind {
        Kind::Blob => &["blob"],
        Kind::Queue => &["queue"],
        Kind::Table => &["table"],
        // a Data Lake file system is a container of the same account: its Blob URL names the same resource
        Kind::DataLake => &["dfs", "blob"],
    };
    let mut domains: Vec<String> = Vec::new();
    for s in services {
        for cloud in ["core.windows.net", "core.chinacloudapi.cn", "core.usgovcloudapi.net", "core.cloudapi.de"] {
            domains.push(format!("{s}.{cloud}"));
        }
        domains.push(format!("{s}.core.windows.net.localhost"));
        domains.push(format!("{s}.core.localhost"));
        domains.push(format!("{s}.localhost"));
    }
    if kind == Kind::Table {
        // the Table API of Cosmos DB
        domains.push("table.cosmos.azure.com".to_string());
    }
    let domains: Vec<&str> = domains.iter().map(String::as_str).collect();
    vec![Aud::exact("https://storage.azure.com"), Aud::host_in(account, &domains)]
}

const MISMATCH: &str = "This request is not authorized to perform this operation using this permission.";
const FAILURE: &str = "This request is not authorized to perform this operation.";

fn version_of(req: &Request) -> Option<&str> {
    req.header("x-ms-version").map(str::trim).filter(|v| is_date(v))
}

/// A bearer token needs `x-ms-version` 2017-11-09 or newer. Older (or none): 403 `AuthenticationFailed`, no `x-ms-error-code` and no
/// `x-ms-version` in the answer (what Azure sends). `None` when the version is fine.
pub fn old_version_refusal(req: &Request, kind: Kind) -> Option<Response> {
    if version_of(req).is_some_and(|v| v >= Kind::OAUTH_FROM) {
        return None;
    }
    let msg = "Server failed to authenticate the request. Make sure the value of Authorization header is formed correctly including the signature.";
    let detail = "Authentication scheme Bearer is not supported in this version.";
    Some(match kind {
        Kind::Table => table_json(403, "AuthenticationFailed", msg, req, false),
        Kind::DataLake => dfs_json(403, "AuthenticationFailed", msg, Some(detail), req, false),
        _ => {
            let mut r = error_with_details(&req.ctx, 403, "AuthenticationFailed", msg, &[("AuthenticationErrorDetail", detail)]);
            r.headers_mut().remove("x-ms-error-code");
            r
        }
    })
}

fn detail_of(f: &TokenFailure) -> &'static str {
    match f {
        TokenFailure::WrongAudience(_) => "Audience validation failed. Audience did not match.",
        TokenFailure::BadSignature => "Signature validation failed. Signature verification failed.",
        TokenFailure::Malformed => "The access token was missing or malformed.",
        // the texts of an expired token and of one that is not valid yet were not seen on Azure (an Entra token cannot be expired on purpose)
        TokenFailure::Expired(_) => "Lifetime validation failed. The token is expired.",
        TokenFailure::NotYetValid(_) => "Lifetime validation failed. The token is not yet valid.",
    }
}

fn table_json(status: u16, code: &str, message: &str, req: &Request, error_header: bool) -> Response {
    let body = serde_json::json!({"odata.error": {"code": code, "message": {"lang": "en-US", "value": format!("{message}\nRequestId:{}\nTime:{}", req.ctx.request_id, crate::time::iso8601_7(req.ctx.time))}}});
    let mut r = Resp::new(status)
        .header("Content-Type", "application/json;odata=nometadata;streaming=true;charset=utf-8")
        .body(serde_json::to_vec(&body).unwrap_or_default());
    if error_header {
        r = r.header("x-ms-error-code", code);
    }
    r.build()
}

fn dfs_json(status: u16, code: &str, message: &str, detail: Option<&str>, req: &Request, error_header: bool) -> Response {
    let mut err = serde_json::json!({"code": code, "message": format!("{message}\nRequestId:{}\nTime:{}", req.ctx.request_id, crate::time::iso8601_7(req.ctx.time))});
    if let Some(d) = detail {
        err["detail"] = serde_json::json!({"AuthenticationErrorDetail": d});
    }
    let mut r = Resp::new(status)
        .header("Content-Type", "application/json;charset=utf-8")
        .body(serde_json::to_vec(&serde_json::json!({"error": err})).unwrap_or_default());
    if error_header {
        r = r.header("x-ms-error-code", code);
    }
    r.build()
}

/// The answer to a token that is missing a thing: wrong audience, bad or expired signature, malformed.
pub fn token_refusal(req: &Request, kind: Kind, failure: &TokenFailure, tenant: &str) -> Response {
    if let Some(r) = old_version_refusal(req, kind) {
        return r;
    }
    let detail = detail_of(failure);
    let challenge = req.header("x-ms-version").map(str::trim).is_some_and(|v| v >= kind.challenge_from());
    if challenge {
        let msg = "Server failed to authenticate the request. Please refer to the information in the www-authenticate header.";
        let www = format!(
            "Bearer authorization_uri=https://login.microsoftonline.com/{tenant}/oauth2/authorize resource_id=https://storage.azure.com"
        );
        let mut r = match kind {
            Kind::Table => table_json(401, "InvalidAuthenticationInfo", msg, req, false),
            Kind::DataLake => dfs_json(401, "InvalidAuthenticationInfo", msg, Some(detail), req, true),
            _ => error_with_details(&req.ctx, 401, "InvalidAuthenticationInfo", msg, &[("AuthenticationErrorDetail", detail)]),
        };
        if let Ok(v) = http::HeaderValue::from_str(&www) {
            r.headers_mut().insert(http::header::WWW_AUTHENTICATE, v);
        }
        return r;
    }
    let msg = "Server failed to authenticate the request. Make sure the value of Authorization header is formed correctly including the signature.";
    match kind {
        Kind::Table => table_json(403, "AuthenticationFailed", msg, req, false),
        _ => error_with_details(&req.ctx, 403, "AuthenticationFailed", msg, &[("AuthenticationErrorDetail", detail)]),
    }
}

/// Which code a missing permission is told: `AuthorizationFailure` (a shorter text) for creating a container or a queue, else
/// `AuthorizationPermissionMismatch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Mismatch,
    Failure,
}

pub fn permission_refusal(req: &Request, kind: Kind, which: Refusal) -> Response {
    match (kind, which) {
        (Kind::Table, _) => table_json(403, "AuthorizationPermissionMismatch", MISMATCH, req, false),
        (Kind::DataLake, _) => dfs_json(403, "AuthorizationPermissionMismatch", MISMATCH, None, req, true),
        (_, Refusal::Mismatch) => error(&req.ctx, 403, "AuthorizationPermissionMismatch", MISMATCH),
        (_, Refusal::Failure) => error(&req.ctx, 403, "AuthorizationFailure", FAILURE),
    }
}

/// The whole check for a request that carries a bearer token: the token, then the role. `None`: go on. `Some(answer)`: the answer, without
/// the common headers (the caller adds them with `storage::finish`).
pub fn check(auth: &Auth, req: &Request, kind: Kind, account: &str, need: &Need, res: &Resource, which: Refusal) -> Option<Response> {
    let who = match authenticate(auth, req, kind, account) {
        Ok(w) => w,
        Err(r) => return Some(r),
    };
    authorize(auth, req, kind, &who, need, res, which)
}

/// The token of the request, checked.
#[allow(clippy::result_large_err)] // the error is the finished answer
pub fn authenticate(auth: &Auth, req: &Request, kind: Kind, account: &str) -> Result<Principal, Response> {
    if let Some(r) = old_version_refusal(req, kind) {
        return Err(r);
    }
    auth.authenticate(req, &audiences(kind, account)).map_err(|f| {
        let tenant = auth.authority().map(|a| a.default_tenant()).unwrap_or_else(|| "common".to_string());
        token_refusal(req, kind, &f, &tenant)
    })
}

/// The role of a principal for a need.
pub fn authorize(auth: &Auth, req: &Request, kind: Kind, who: &Principal, need: &Need, res: &Resource, which: Refusal) -> Option<Response> {
    let what = format!("{} {}", req.method, req.decoded_path());
    match auth.authorize(&what, who, need, res) {
        Ok(()) => None,
        Err(_) => Some(permission_refusal(req, kind, which)),
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use crate::auth::TestAuthority;
    use crate::request::{Endpoint, Query, RequestCtx};
    use std::sync::Arc;

    fn req(version: Option<&str>, token: &str) -> Request {
        let mut h = http::HeaderMap::new();
        h.insert("authorization", format!("Bearer {token}").parse().unwrap());
        if let Some(v) = version {
            h.insert("x-ms-version", v.parse().unwrap());
        }
        Request {
            method: http::Method::GET,
            path: "/c/b".into(),
            query: Query::default(),
            headers: h,
            body: Default::default(),
            account: "acct".into(),
            ctx: RequestCtx {
                request_id: "rid".into(),
                time: crate::time::OffsetDateTime::now_utc(),
                endpoint: Endpoint { scheme: "http", authority: "acct.blob.localhost".into(), path_style: false, account: "acct".into() },
            },
        }
    }

    fn strict() -> Auth {
        let a = Auth::strict();
        a.set_authority(Arc::new(TestAuthority));
        a
    }

    fn body(r: &Response) -> String {
        String::from_utf8_lossy(r.body()).to_string()
    }

    #[test]
    fn a_wrong_audience_is_401_with_the_challenge_from_2019_12_12_and_403_before() {
        let a = strict();
        let t = TestAuthority::token("p1", "https://vault.azure.net");
        let r = authenticate(&a, &req(Some("2024-11-04"), &t), Kind::Blob, "acct").unwrap_err();
        assert_eq!(r.status(), 401);
        assert_eq!(r.headers()["x-ms-error-code"], "InvalidAuthenticationInfo");
        assert!(r.headers()["www-authenticate"].to_str().unwrap().starts_with(
            "Bearer authorization_uri=https://login.microsoftonline.com/t-0/oauth2/authorize resource_id=https://storage.azure.com"
        ));
        assert!(
            body(&r).contains("<AuthenticationErrorDetail>Audience validation failed. Audience did not match.</AuthenticationErrorDetail>")
        );
        let r = authenticate(&a, &req(Some("2019-02-02"), &t), Kind::Blob, "acct").unwrap_err();
        assert_eq!(r.status(), 403);
        assert_eq!(r.headers()["x-ms-error-code"], "AuthenticationFailed");
        assert!(r.headers().get("www-authenticate").is_none());
    }

    #[test]
    fn a_version_before_oauth_has_no_error_code_header() {
        let a = strict();
        let t = TestAuthority::token("p1", "https://storage.azure.com");
        let r = authenticate(&a, &req(Some("2017-04-17"), &t), Kind::Queue, "acct").unwrap_err();
        assert_eq!(r.status(), 403);
        assert!(r.headers().get("x-ms-error-code").is_none());
        assert!(body(&r).contains("Authentication scheme Bearer is not supported in this version."));
        assert!(authenticate(&a, &req(None, &t), Kind::Queue, "acct").is_err());
        assert!(authenticate(&a, &req(Some("2017-11-09"), &t), Kind::Queue, "acct").is_ok());
    }

    #[test]
    fn table_and_data_lake_have_their_own_json() {
        let a = strict();
        let t = TestAuthority::token("p1", "https://vault.azure.net");
        let r = authenticate(&a, &req(Some("2021-08-06"), &t), Kind::Table, "acct").unwrap_err();
        assert_eq!(r.status(), 401);
        assert!(r.headers().get("x-ms-error-code").is_none());
        let v: serde_json::Value = serde_json::from_slice(r.body()).unwrap();
        assert_eq!(v["odata.error"]["code"], "InvalidAuthenticationInfo");
        assert!(!body(&r).contains("Audience"));
        let r = authenticate(&a, &req(Some("2024-11-04"), &t), Kind::DataLake, "acct").unwrap_err();
        let v: serde_json::Value = serde_json::from_slice(r.body()).unwrap();
        assert_eq!(v["error"]["detail"]["AuthenticationErrorDetail"], "Audience validation failed. Audience did not match.");
        // the Python Table SDK sends 2019-02-02: the 403 form
        let r = authenticate(&a, &req(Some("2019-02-02"), &t), Kind::Table, "acct").unwrap_err();
        assert_eq!(r.status(), 403);
    }

    #[test]
    fn the_account_url_is_an_audience_too() {
        let a = strict();
        let t = TestAuthority::token("p1", "https://acct.blob.core.windows.net");
        assert!(authenticate(&a, &req(Some("2024-11-04"), &t), Kind::Blob, "acct").is_ok());
        let t = TestAuthority::token("p1", "https://other.blob.core.windows.net");
        assert!(authenticate(&a, &req(Some("2024-11-04"), &t), Kind::Blob, "acct").is_err());
    }

    #[test]
    fn a_missing_permission_has_the_shape_of_each_endpoint() {
        let r = req(Some("2024-11-04"), "x");
        let m = permission_refusal(&r, Kind::Blob, Refusal::Mismatch);
        assert_eq!((m.status().as_u16(), m.headers()["x-ms-error-code"].to_str().unwrap()), (403, "AuthorizationPermissionMismatch"));
        assert!(body(&m).contains("This request is not authorized to perform this operation using this permission.\nRequestId:rid\nTime:"));
        let f = permission_refusal(&r, Kind::Queue, Refusal::Failure);
        assert_eq!(f.headers()["x-ms-error-code"], "AuthorizationFailure");
        assert!(body(&f).contains("<Message>This request is not authorized to perform this operation.\nRequestId:"));
        let t = permission_refusal(&r, Kind::Table, Refusal::Failure);
        assert!(t.headers().get("x-ms-error-code").is_none());
        assert_eq!(t.headers()["content-type"], "application/json;odata=nometadata;streaming=true;charset=utf-8");
        assert!(body(&t).contains("\"odata.error\""));
        let d = permission_refusal(&r, Kind::DataLake, Refusal::Failure);
        assert_eq!(d.headers()["content-type"], "application/json;charset=utf-8");
        assert_eq!(d.headers()["x-ms-error-code"], "AuthorizationPermissionMismatch");
    }
}
