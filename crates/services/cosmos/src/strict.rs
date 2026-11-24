//! Strict authorisation for Cosmos DB (`docs/strict-auth.md`). Cosmos has its own role system (`sqlRoleDefinitions`, `sqlRoleAssignments`), which
//! the registry in core holds: the two built-in roles and the ones made through Resource Manager or the admin API.
//!
//! What Azure answered in November 2026 (sections 4 of the findings): a token for a call that is not available with a token at all gets 403
//! with sub-status 5300 whatever the roles; a metadata read without `readMetadata` gets 5301; an item, query, change feed or stored procedure
//! call without the action gets 5302. A token that is wrong gets 401 with 5003 (malformed), 5004 (signature) or 5008 (audience).
//! A key (`type=master`) is not touched.

use azemu_core::auth::{Aud, Auth, Principal, TokenFailure};
use azemu_core::Request;

use crate::errors::CosmosError;

const PREFIX: &str = "Microsoft.DocumentDB/databaseAccounts";
/// The application id Entra knows Cosmos DB by; it is in the list of audiences the account names.
const RESOURCE_APP_ID: &str = "a232010e-820c-4083-83bb-3ace5fc29d0b";

/// What a call needs.
#[derive(Debug, PartialEq, Eq)]
pub enum Need {
    /// Not available with a token: 5300.
    Blocked,
    /// `readMetadata` on a path (5301). `any_scope`: the answer for the list of databases, which names no resource.
    Metadata { path: String, any_scope: bool },
    /// A data action (5302), with Azure's operation and resource type numbers.
    Data { action: String, op: u32, rtype: u32, role_path: String, resource: String },
}

fn data(action: &str, op: u32, rtype: u32, role_path: String, resource: String) -> Need {
    Need::Data { action: format!("{PREFIX}/{action}"), op, rtype, role_path, resource }
}

/// The call the request is: what it needs.
pub fn classify(req: &Request) -> Need {
    let segs = req.segments();
    let s: Vec<&str> = segs.iter().map(String::as_str).collect();
    let m = req.method.as_str();
    let is_query = req.header("x-ms-documentdb-isquery").is_some_and(|v| v.eq_ignore_ascii_case("true"))
        || req.header("content-type").is_some_and(|c| c.to_ascii_lowercase().contains("application/query+json"));
    match (m, s.as_slice()) {
        ("GET" | "HEAD", []) => Need::Metadata { path: "/".into(), any_scope: false },
        ("GET", ["dbs"]) => Need::Metadata { path: "/".into(), any_scope: true },
        ("POST", ["dbs"]) if is_query => Need::Metadata { path: "/".into(), any_scope: true },
        ("GET", ["dbs", d]) => Need::Metadata { path: format!("dbs/{d}"), any_scope: false },
        ("GET", ["dbs", d, "colls"]) => Need::Metadata { path: format!("dbs/{d}"), any_scope: false },
        ("POST", ["dbs", d, "colls"]) if is_query => Need::Metadata { path: format!("dbs/{d}"), any_scope: false },
        ("GET", ["dbs", d, "colls", c]) => Need::Metadata { path: format!("dbs/{d}/colls/{c}"), any_scope: false },
        ("GET", ["dbs", d, "colls", c, "pkranges"]) => Need::Metadata { path: format!("dbs/{d}/colls/{c}"), any_scope: false },
        // items
        ("POST", ["dbs", d, "colls", c, "docs"]) => {
            let (role, res) = (format!("/dbs/{d}/colls/{c}"), format!("dbs/{d}/colls/{c}"));
            if is_query {
                data("sqlDatabases/containers/executeQuery", 15, 2, role, res)
            } else if req.header("x-ms-cosmos-is-batch-request").is_some() {
                data("sqlDatabases/containers/items/create", 0, 2, role, res)
            } else if req.header("x-ms-documentdb-is-upsert").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
                data("sqlDatabases/containers/items/upsert", 20, 2, role, res)
            } else {
                data("sqlDatabases/containers/items/create", 0, 2, role, res)
            }
        }
        ("GET", ["dbs", d, "colls", c, "docs"]) => {
            let (role, res) = (format!("/dbs/{d}/colls/{c}"), format!("dbs/{d}/colls/{c}"));
            if req.header("a-im").is_some() {
                data("sqlDatabases/containers/readChangeFeed", 3, 2, role, res)
            } else {
                data("sqlDatabases/containers/items/read", 2, 2, role, res)
            }
        }
        ("GET" | "HEAD", ["dbs", d, "colls", c, "docs", _]) => {
            data("sqlDatabases/containers/items/read", 2, 2, format!("/dbs/{d}/colls/{c}"), format!("dbs/{d}/colls/{c}"))
        }
        ("PUT", ["dbs", d, "colls", c, "docs", _]) => {
            data("sqlDatabases/containers/items/replace", 5, 2, format!("/dbs/{d}/colls/{c}"), format!("dbs/{d}/colls/{c}"))
        }
        ("PATCH", ["dbs", d, "colls", c, "docs", _]) => {
            data("sqlDatabases/containers/items/replace", 1, 2, format!("/dbs/{d}/colls/{c}"), format!("dbs/{d}/colls/{c}"))
        }
        ("DELETE", ["dbs", d, "colls", c, "docs", _]) => {
            data("sqlDatabases/containers/items/delete", 4, 2, format!("/dbs/{d}/colls/{c}"), format!("dbs/{d}/colls/{c}"))
        }
        ("POST", ["dbs", d, "colls", c, "sprocs", id]) => data(
            "sqlDatabases/containers/executeStoredProcedure",
            9,
            109,
            format!("/dbs/{d}/colls/{c}"),
            format!("dbs/{d}/colls/{c}/sprocs/{id}"),
        ),
        // everything else is not available with a token: databases and containers made, replaced and deleted, users, permissions, offers,
        // stored procedures, triggers and functions listed, made, replaced and deleted
        _ => Need::Blocked,
    }
}

/// The Entra token of a request: the `Authorization` header of a Cosmos DB request is `type=aad&ver=1.0&sig=<token>`, URL-encoded.
fn aad_token(req: &Request) -> Option<String> {
    let raw = req.header("authorization")?;
    let decoded = percent_encoding::percent_decode_str(raw).decode_utf8_lossy().into_owned();
    let mut is_aad = false;
    let mut sig = None;
    for pair in decoded.split('&') {
        match pair.split_once('=') {
            Some(("type", v)) => is_aad = v.eq_ignore_ascii_case("aad"),
            Some(("sig", v)) => sig = Some(v.to_string()),
            _ => {}
        }
    }
    if is_aad {
        sig
    } else {
        None
    }
}

fn blocked(account: &str, text: String, substatus: u32, status: u16, code: &'static str) -> CosmosError {
    let mut e = CosmosError::plain(status, code, format!("Request blocked by Auth {account} : {text}")).sub(substatus);
    e.shape = crate::errors::Shape::Plain;
    e
}

fn token_error(account: &str, f: &TokenFailure) -> CosmosError {
    match f {
        TokenFailure::BadSignature => blocked(
            account,
            "Provided token does not have a valid signature. Please ensure that the AAD token is not being modified before use.".into(),
            5004,
            401,
            "Unauthorized",
        ),
        TokenFailure::WrongAudience(_) => blocked(
            account,
            format!(
                "Provided AAD token has an invalid audience. This database account accepts tokens intended for [https://{account}.documents.azure.com, https://{account}.sql.cosmos.azure.com, https://cosmos.azure.com, https://{account}.sqlx.cosmos.azure.com, https://cosmos.azure.com, {RESOURCE_APP_ID}, https://cosmos.azure.com]."
            ),
            5008,
            401,
            "Unauthorized",
        ),
        TokenFailure::Malformed => blocked(
            account,
            "Provided token does not conform to the format specified by RFC 7519. Please ensure the token is a valid JWT token signed by AAD. Specific error: [IDX14100: JWT is not well formed, there are no dots (.).\nThe token needs to be in JWS or JWE Compact Serialization Format. (JWS): 'EncodedHeader.EncodedPayload.EncodedSignature'. (JWE): 'EncodedProtectedHeader.EncodedEncryptedKey.EncodedInitializationVector.EncodedCiphertext.EncodedAuthenticationTag'.]".into(),
            5003,
            401,
            "Unauthorized",
        ),
        // the sub-statuses and texts of a token that is expired or not valid yet were not seen on Azure (an Entra token cannot be expired on purpose)
        TokenFailure::Expired(_) => blocked(account, "Provided AAD token has expired. Please acquire a new token.".into(), 5005, 401, "Unauthorized"),
        TokenFailure::NotYetValid(_) => blocked(account, "Provided AAD token is not valid yet. Please check the clock of the machine that asked for it.".into(), 5006, 401, "Unauthorized"),
    }
}

fn forbidden(account: &str, text: String, substatus: u32) -> CosmosError {
    blocked(account, text, substatus, 403, "Forbidden")
}

/// The domains an account's own URL is under (`docs/strict-auth.md`, "Audiences"): the ones Azure listed for a refused token in November 2026
/// (`documents.azure.com`, `sql.cosmos.azure.com`, `sqlx.cosmos.azure.com`), the other clouds' document domains, and the emulator's own
/// (`{account}.documents.localhost`).
const ACCOUNT_DOMAINS: &[&str] = &[
    "documents.azure.com",
    "sql.cosmos.azure.com",
    "sqlx.cosmos.azure.com",
    "documents.azure.cn",
    "documents.azure.us",
    "documents.microsoftazure.de",
    "documents.azure.com.localhost",
    "documents.localhost",
];

/// `Some(error)` when the request carries an Entra token and is refused.
pub fn gate(auth: &Auth, req: &Request) -> Option<CosmosError> {
    if !auth.is_strict() {
        return None;
    }
    let token = aad_token(req)?;
    let account = req.account.to_ascii_lowercase();
    let auds = [Aud::exact("https://cosmos.azure.com"), Aud::host_in(&account, ACCOUNT_DOMAINS)];
    let who: Principal = match auth.authenticate_token(&token, req.ctx.time.unix_timestamp(), &auds) {
        Ok(w) => w,
        Err(f) => return Some(token_error(&account, &f)),
    };
    let what = format!("{} {}", req.method, req.decoded_path());
    match classify(req) {
        Need::Blocked => {
            auth.log_refusal(&what, &who, "may not make this call with a token at all: Cosmos DB answers 5300 for database, container, user, offer and script management whatever the roles");
            Some(forbidden(
                &account,
                format!("The given request [{} {}] cannot be authorized by AAD token in data plane. Learn more: https://aka.ms/cosmos-native-rbac.", req.method, req.decoded_path()),
                5300,
            ))
        }
        Need::Metadata { path, any_scope } => {
            let action = format!("{PREFIX}/readMetadata");
            auth.authorize_cosmos(&what, &who, &account, &action, &path).err().map(|_| {
                let text = if any_scope {
                    format!("Request for Read DatabaseAccount is blocked because principal [{}] does not have required RBAC permissions to perform action [{action}] on any scope. Learn more: https://aka.ms/cosmos-native-rbac.", who.oid)
                } else {
                    format!("Request is blocked because principal [{}] does not have required RBAC permissions to perform action [{action}] on resource [{path}]. Learn more: https://aka.ms/cosmos-native-rbac.", who.oid)
                };
                forbidden(&account, text, 5301)
            })
        }
        Need::Data { action, op, rtype, role_path, resource } => auth.authorize_cosmos(&what, &who, &account, &action, &role_path).err().map(|_| {
            let mut e = CosmosError::new(
                403,
                "Forbidden",
                format!("Request is blocked because principal [{}] does not have the required RBAC permissions to perform action [{action}] with OperationType [{op}] and ResourceType [{rtype}] on resource [{resource}]. Learn more: https://aka.ms/cosmos-native-rbac", who.oid),
            )
            .sub(5302);
            e.tail = Some(format!(
                ", Request URI: /apps/{}/services/{}/partitions/{}/replicas/{}p, RequestStats: , SDK: Microsoft.Azure.Documents.Common/2.14.0",
                uuid::Uuid::new_v4(),
                uuid::Uuid::new_v4(),
                uuid::Uuid::new_v4(),
                "133"
            ));
            e
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use azemu_core::{Endpoint, Query, RequestCtx};

    fn req(method: &str, path: &str, headers: &[(&str, &str)]) -> Request {
        let mut h = azemu_core::http::HeaderMap::new();
        for (k, v) in headers {
            h.insert(azemu_core::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        Request {
            method: method.parse().unwrap(),
            path: path.into(),
            query: Query::default(),
            headers: h,
            body: Default::default(),
            account: "acct".into(),
            ctx: RequestCtx {
                request_id: "r".into(),
                time: azemu_core::time::OffsetDateTime::now_utc(),
                endpoint: Endpoint {
                    scheme: "http",
                    authority: "acct.documents.localhost".into(),
                    path_style: false,
                    account: "acct".into(),
                },
            },
        }
    }

    #[test]
    fn what_a_call_needs() {
        assert_eq!(classify(&req("GET", "/", &[])), Need::Metadata { path: "/".into(), any_scope: false });
        assert_eq!(classify(&req("GET", "/dbs", &[])), Need::Metadata { path: "/".into(), any_scope: true });
        assert_eq!(classify(&req("POST", "/dbs", &[])), Need::Blocked);
        assert_eq!(classify(&req("DELETE", "/dbs/d", &[])), Need::Blocked);
        assert_eq!(classify(&req("POST", "/dbs/d/colls", &[])), Need::Blocked);
        assert_eq!(classify(&req("GET", "/dbs/d/colls/c/sprocs", &[])), Need::Blocked);
        assert!(matches!(classify(&req("POST", "/dbs/d/colls/c/docs", &[("x-ms-documentdb-isquery", "true")])), Need::Data { op: 15, .. }));
        assert!(matches!(
            classify(&req("POST", "/dbs/d/colls/c/docs", &[("x-ms-documentdb-is-upsert", "True")])),
            Need::Data { op: 20, .. }
        ));
        assert!(matches!(classify(&req("PATCH", "/dbs/d/colls/c/docs/i", &[])), Need::Data { op: 1, .. }));
        assert!(matches!(classify(&req("GET", "/dbs/d/colls/c/docs", &[("a-im", "Incremental feed")])), Need::Data { op: 3, .. }));
        assert!(matches!(classify(&req("POST", "/dbs/d/colls/c/sprocs/p", &[])), Need::Data { rtype: 109, .. }));
    }

    #[test]
    fn the_token_is_read_out_of_the_url_encoded_header() {
        let r = req("GET", "/", &[("authorization", "type%3daad%26ver%3d1.0%26sig%3dabc.def.ghi")]);
        assert_eq!(aad_token(&r).as_deref(), Some("abc.def.ghi"));
        let r = req("GET", "/", &[("authorization", "type%3Dmaster%26ver%3D1.0%26sig%3Dxyz")]);
        assert_eq!(aad_token(&r), None);
    }
}
