//! Strict authorisation for Key Vault (`docs/strict-auth.md`): a bearer token is checked, then the vault's own way of deciding: Azure role-based access
//! control (the data actions of the roles) or the vault's access policies. Which one a vault uses is in the registry: a vault Resource Manager made
//! says so with `enableRbacAuthorization`; any other vault is role-based.
//!
//! What Azure answers was compared in November 2026 (`docs/strict-auth.md`, "Key Vault"): 403 `Forbidden` with the action and the resource named
//! (role-based; inner code `ForbiddenByRbac`) or the permission word named (access policies; `ForbiddenByPolicy` for a principal with an entry
//! that lacks the permission, `AccessDenied` for one with no entry); 401 for a token that is wrong.

use azemu_core::auth::{Aud, Auth, Need, Principal, Resource, TokenFailure, VaultAccess};
use azemu_core::{json_error, Request, Response};
use serde_json::json;

const PREFIX: &str = "Microsoft.KeyVault/vaults";
/// The application id of the Key Vault service in Entra, which the real service names in the "invalid audience" answer.
const RESOURCE_APP_ID: &str = "cfa8b339-82a2-471a-a3c9-0fc0be7a4093";
const REGION: &str = "westus";

/// What an operation is, for both ways of deciding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvOp {
    /// `secrets`, `keys`, `certificates`, `certificatecas`, ... the part of the action after `vaults/`, and what follows
    pub action: String,
    /// The object the request is about (`secrets/name`), or `None` for the vault.
    pub object: Option<(String, String)>,
    /// The permission kind and word of an access policy (`secrets`, `get`); `None`: nothing in the policy covers it.
    pub policy: Option<(&'static str, &'static str)>,
}

fn op(action: &str, object: Option<(&str, &str)>, policy: Option<(&'static str, &'static str)>) -> Option<KvOp> {
    Some(KvOp { action: format!("{PREFIX}/{action}"), object: object.map(|(k, n)| (k.to_string(), n.to_string())), policy })
}

/// The operation a request is. `None`: nothing is checked (random bytes, a path no route matches).
pub fn classify(req: &Request) -> Option<KvOp> {
    let segs = req.segments();
    let s: Vec<&str> = segs.iter().map(String::as_str).collect();
    let m = req.method.as_str();
    match (m, s.as_slice()) {
        // secrets
        ("GET", ["secrets"]) | ("GET", ["deletedsecrets"]) => op("secrets/readMetadata/action", None, Some(("secrets", "list"))),
        ("GET", ["secrets", n, "versions"]) => op("secrets/readMetadata/action", Some(("secrets", n)), Some(("secrets", "list"))),
        ("GET", ["secrets", n]) | ("GET", ["secrets", n, _]) | ("GET", ["deletedsecrets", n]) => {
            op("secrets/getSecret/action", Some(("secrets", n)), Some(("secrets", "get")))
        }
        ("PUT", ["secrets", n]) => op("secrets/setSecret/action", Some(("secrets", n)), Some(("secrets", "set"))),
        ("PATCH", ["secrets", n]) | ("PATCH", ["secrets", n, _]) => {
            op("secrets/update/action", Some(("secrets", n)), Some(("secrets", "set")))
        }
        ("DELETE", ["secrets", n]) => op("secrets/delete", Some(("secrets", n)), Some(("secrets", "delete"))),
        ("POST", ["deletedsecrets", n, "recover"]) => op("secrets/recover/action", Some(("secrets", n)), Some(("secrets", "recover"))),
        ("DELETE", ["deletedsecrets", n]) => op("secrets/purge/action", Some(("secrets", n)), Some(("secrets", "purge"))),
        ("POST", ["secrets", n, "backup"]) => op("secrets/backup/action", Some(("secrets", n)), Some(("secrets", "backup"))),
        ("POST", ["secrets", "restore"]) => op("secrets/restore/action", None, Some(("secrets", "restore"))),
        // keys
        ("GET", ["keys"]) | ("GET", ["deletedkeys"]) => op("keys/read", None, Some(("keys", "list"))),
        ("GET", ["keys", n, "versions"]) => op("keys/read", Some(("keys", n)), Some(("keys", "list"))),
        ("GET", ["keys", n, "rotationpolicy"]) => {
            op("keyrotationpolicies/get/action", Some(("keys", n)), Some(("keys", "getrotationpolicy")))
        }
        ("PUT", ["keys", n, "rotationpolicy"]) => {
            op("keyrotationpolicies/set/action", Some(("keys", n)), Some(("keys", "setrotationpolicy")))
        }
        ("GET", ["keys", n]) | ("GET", ["keys", n, _]) | ("GET", ["deletedkeys", n]) => {
            op("keys/read", Some(("keys", n)), Some(("keys", "get")))
        }
        ("PUT", ["keys", n]) => op("keys/import/action", Some(("keys", n)), Some(("keys", "import"))),
        ("POST", ["keys", n, "create"]) => op("keys/create/action", Some(("keys", n)), Some(("keys", "create"))),
        ("PATCH", ["keys", n]) | ("PATCH", ["keys", n, _]) => op("keys/update/action", Some(("keys", n)), Some(("keys", "update"))),
        ("DELETE", ["keys", n]) => op("keys/delete", Some(("keys", n)), Some(("keys", "delete"))),
        ("POST", ["keys", n, "backup"]) => op("keys/backup/action", Some(("keys", n)), Some(("keys", "backup"))),
        ("POST", ["keys", "restore"]) => op("keys/restore/action", None, Some(("keys", "restore"))),
        ("POST", ["keys", n, "rotate"]) => op("keys/rotate/action", Some(("keys", n)), Some(("keys", "rotate"))),
        ("POST", ["deletedkeys", n, "recover"]) => op("keys/recover/action", Some(("keys", n)), Some(("keys", "recover"))),
        ("DELETE", ["deletedkeys", n]) => op("keys/purge/action", Some(("keys", n)), Some(("keys", "purge"))),
        ("POST", ["keys", n, o]) | ("POST", ["keys", n, _, o]) => {
            let (action, word) = match o.to_ascii_lowercase().as_str() {
                "sign" => ("sign", "sign"),
                "verify" => ("verify", "verify"),
                "encrypt" => ("encrypt", "encrypt"),
                "decrypt" => ("decrypt", "decrypt"),
                "wrapkey" | "wrap" => ("wrap", "wrapkey"),
                "unwrapkey" | "unwrap" => ("unwrap", "unwrapkey"),
                "release" => ("release", "release"),
                _ => return None,
            };
            op(&format!("keys/{action}/action"), Some(("keys", n)), Some(("keys", word)))
        }
        // certificates
        ("GET", ["certificates"]) | ("GET", ["deletedcertificates"]) => op("certificates/read", None, Some(("certificates", "list"))),
        ("GET", ["certificates", "issuers"]) => op("certificatecas/read", None, Some(("certificates", "listissuers"))),
        ("GET", ["certificates", "issuers", _]) => op("certificatecas/read", None, Some(("certificates", "getissuers"))),
        ("PUT", ["certificates", "issuers", _]) | ("PATCH", ["certificates", "issuers", _]) => {
            op("certificatecas/write", None, Some(("certificates", "setissuers")))
        }
        ("DELETE", ["certificates", "issuers", _]) => op("certificatecas/delete", None, Some(("certificates", "deleteissuers"))),
        ("GET", ["certificates", "contacts"]) => op("certificatecontacts/read", None, Some(("certificates", "managecontacts"))),
        ("PUT", ["certificates", "contacts"]) | ("DELETE", ["certificates", "contacts"]) => {
            op("certificatecontacts/write", None, Some(("certificates", "managecontacts")))
        }
        ("GET", ["certificates", n, "pending"]) | ("GET", ["certificates", n, "policy"]) => {
            op("certificates/read", Some(("certificates", n)), Some(("certificates", "get")))
        }
        ("PATCH", ["certificates", n, "pending"])
        | ("PATCH", ["certificates", n, "policy"])
        | ("POST", ["certificates", n, "pending", "merge"]) => {
            op("certificates/update/action", Some(("certificates", n)), Some(("certificates", "update")))
        }
        ("GET", ["certificates", n, "versions"]) => op("certificates/read", Some(("certificates", n)), Some(("certificates", "list"))),
        ("GET", ["certificates", n]) | ("GET", ["certificates", n, _]) | ("GET", ["deletedcertificates", n]) => {
            op("certificates/read", Some(("certificates", n)), Some(("certificates", "get")))
        }
        ("POST", ["certificates", n, "create"]) => {
            op("certificates/create/action", Some(("certificates", n)), Some(("certificates", "create")))
        }
        ("POST", ["certificates", n, "import"]) => {
            op("certificates/import/action", Some(("certificates", n)), Some(("certificates", "import")))
        }
        ("PATCH", ["certificates", n]) | ("PATCH", ["certificates", n, _]) => {
            op("certificates/update/action", Some(("certificates", n)), Some(("certificates", "update")))
        }
        ("DELETE", ["certificates", n, "pending"]) | ("DELETE", ["certificates", n]) => {
            op("certificates/delete", Some(("certificates", n)), Some(("certificates", "delete")))
        }
        ("POST", ["certificates", n, "backup"]) => {
            op("certificates/backup/action", Some(("certificates", n)), Some(("certificates", "backup")))
        }
        ("POST", ["certificates", "restore"]) => op("certificates/restore/action", None, Some(("certificates", "restore"))),
        ("POST", ["deletedcertificates", n, "recover"]) => {
            op("certificates/recover/action", Some(("certificates", n)), Some(("certificates", "recover")))
        }
        ("DELETE", ["deletedcertificates", n]) => {
            op("certificates/purge/action", Some(("certificates", n)), Some(("certificates", "purge")))
        }
        _ => None,
    }
}

/// The challenge of a token that is wrong is the one of a request with no token (`auth.rs`): the same tenant and resource.
fn www_authenticate(req: &Request) -> String {
    format!(
        "Bearer authorization=\"https://login.microsoftonline.com/{}\", resource=\"{}\"",
        crate::auth::TENANT,
        crate::auth::challenge_resource(&req.ctx.endpoint.authority)
    )
}

fn unauthorized(req: &Request, message: &str) -> Response {
    let mut r = json_error::response(401, "Unauthorized", message);
    if let Ok(v) = azemu_core::http::HeaderValue::from_str(&www_authenticate(req)) {
        r.headers_mut().insert("WWW-Authenticate", v);
    }
    r
}

/// The domains a vault's own URL is under: the Azure clouds' (also Managed HSM) and the emulator's (`{vault}.vault.localhost`).
const VAULT_DOMAINS: &[&str] = &[
    "vault.azure.net",
    "vault.azure.cn",
    "vault.usgovcloudapi.net",
    "vault.microsoftazure.de",
    "managedhsm.azure.net",
    "managedhsm.azure.cn",
    "managedhsm.usgovcloudapi.net",
    "vault.azure.net.localhost",
    "vault.localhost",
];

/// The audiences a vault takes (`docs/strict-auth.md`, "Audiences"): the service (`https://vault.azure.net`), the resource the emulator's
/// challenge named (what a client asks for after the challenge), and the vault's own URL under one of [`VAULT_DOMAINS`].
fn audiences(req: &Request) -> Vec<Aud> {
    vec![
        Aud::exact("https://vault.azure.net"),
        Aud::exact(&crate::auth::challenge_resource(&req.ctx.endpoint.authority)),
        Aud::host_in(&req.account.to_ascii_lowercase(), VAULT_DOMAINS),
    ]
}

fn token_answer(req: &Request, f: &TokenFailure) -> Response {
    let message = match f {
        TokenFailure::WrongAudience(got) => format!("AKV10022: Invalid audience. Expected {RESOURCE_APP_ID}, found: {got}."),
        TokenFailure::BadSignature => "[Unknown] Error validating token: 'S2S12086'.".to_string(),
        TokenFailure::Malformed => "[MalformedToken] Error validating token: 'S2S12086'.".to_string(),
        // the texts of an expired token and of one that is not yet valid were not seen on Azure (an Entra token cannot be expired on purpose)
        TokenFailure::Expired(_) => {
            "[Unknown] Error validating token: 'IDX10223: Lifetime validation failed. The token is expired.'.".to_string()
        }
        TokenFailure::NotYetValid(_) => {
            "[Unknown] Error validating token: 'IDX10222: Lifetime validation failed. The token is not yet valid.'.".to_string()
        }
    };
    unauthorized(req, &message)
}

fn forbidden(message: String, inner: &str) -> Response {
    azemu_core::resp::Resp::new(403u16)
        .json(&json!({"error": {"code": "Forbidden", "message": message, "innererror": {"code": inner}}}))
        .build()
}

/// The vault's id as Azure writes it in the answer: lower case up to the vault name, the name of the object as it is.
fn resource_text(auth: &Auth, vault: &str, object: &Option<(String, String)>) -> String {
    let id = auth.registry().display_id(&Resource::vault(vault)).to_ascii_lowercase();
    match object {
        Some((k, n)) => format!("{id}/{k}/{n}"),
        None => id,
    }
}

/// The answer to a request that carries a bearer token and is refused; `None` when it may go on.
pub fn gate(auth: &Auth, req: &Request) -> Option<Response> {
    if !auth.applies(req) {
        return None;
    }
    let who: Principal = match auth.authenticate(req, &audiences(req)) {
        Ok(w) => w,
        Err(f) => return Some(token_answer(req, &f)),
    };
    let vault = req.account.to_ascii_lowercase();
    let op = classify(req)?;
    match auth.registry().vault_access(&vault) {
        VaultAccess::Rbac => {
            let res = match &op.object {
                Some((k, n)) => Resource::vault(&vault).at(format!("{k}/{n}")),
                None => Resource::vault(&vault),
            };
            let what = format!("{} {}", req.method, req.decoded_path());
            match auth.authorize(&what, &who, &Need::data(&op.action), &res) {
                Ok(()) => None,
                Err(_) => {
                    let message = format!(
                        "Caller is not authorized to perform action on resource.\r\nIf role assignments, deny assignments or role definitions were changed recently, please observe propagation time.\r\nCaller: {}\r\nAction: '{}'\r\nResource: '{}'\r\nAssignment: (not found)\r\nDenyAssignmentId: null\r\nDecisionReason: null \r\nVault: {vault};location={REGION}\r\n",
                        who.caller(),
                        op.action,
                        resource_text(auth, &vault, &op.object)
                    );
                    Some(forbidden(message, "ForbiddenByRbac"))
                }
            }
        }
        VaultAccess::Policies(list, _) => {
            let (kind, word) = op.policy?;
            let entry = list.iter().find(|p| p.object_id.eq_ignore_ascii_case(&who.oid));
            let allowed = entry.is_some_and(|p| {
                let words = match kind {
                    "secrets" => &p.secrets,
                    "keys" => &p.keys,
                    "certificates" => &p.certificates,
                    _ => &p.storage,
                };
                words.iter().any(|w| w.eq_ignore_ascii_case(word) || w == "all")
            });
            if allowed {
                return None;
            }
            auth.log_refusal(
                &format!("{} {}", req.method, req.decoded_path()),
                &who,
                &format!("has no access policy on vault {vault} that grants {kind} {word} (GET /_azemu/auth/keyvault/policies?vault={vault} shows the policies)"),
            );
            // Azure puts `numgroups=0;` after the object id for the calls about deleted certificates and certificate issuers only
            let caller = if req.path.contains("deletedcertificates") || req.path.contains("certificates/issuers") {
                format!("appid={};oid={};numgroups=0;iss=https://sts.windows.net/{}/", who.appid, who.oid, who.tid)
            } else {
                who.caller()
            };
            let message = format!(
                "The user, group or application '{caller}' does not have {kind} {word} permission on key vault '{vault};location={REGION}'. For help resolving this issue, please see https://go.microsoft.com/fwlink/?linkid=2125287"
            );
            Some(forbidden(message, if entry.is_some() { "ForbiddenByPolicy" } else { "AccessDenied" }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use azemu_core::auth::TestAuthority;
    use azemu_core::{Endpoint, Query, RequestCtx};

    fn req(method: &str, path: &str) -> Request {
        let (p, q) = path.split_once('?').unwrap_or((path, ""));
        Request {
            method: method.parse().unwrap(),
            path: p.to_string(),
            query: Query::parse(q),
            headers: Default::default(),
            body: Default::default(),
            account: "v1".into(),
            ctx: RequestCtx {
                request_id: "r".into(),
                time: azemu_core::time::OffsetDateTime::now_utc(),
                endpoint: Endpoint { scheme: "http", authority: "v1.vault.localhost:1".into(), path_style: false, account: "v1".into() },
            },
        }
    }

    #[test]
    fn the_actions_are_the_ones_azure_names() {
        let a = |m: &str, p: &str| classify(&req(m, p)).unwrap();
        assert_eq!(a("GET", "/secrets/s1").action, "Microsoft.KeyVault/vaults/secrets/getSecret/action");
        assert_eq!(a("GET", "/secrets").object, None);
        assert_eq!(a("GET", "/secrets/s1/versions").object, Some(("secrets".into(), "s1".into())));
        assert_eq!(a("DELETE", "/secrets/s1").action, "Microsoft.KeyVault/vaults/secrets/delete");
        assert_eq!(a("PATCH", "/secrets/s1").policy, Some(("secrets", "set")));
        assert_eq!(a("PUT", "/secrets/s1").action, "Microsoft.KeyVault/vaults/secrets/setSecret/action");
        assert_eq!(a("GET", "/deletedsecrets/s1").action, "Microsoft.KeyVault/vaults/secrets/getSecret/action");
        assert_eq!(a("POST", "/secrets/restore").object, None);
        assert_eq!(a("GET", "/keys/k").action, "Microsoft.KeyVault/vaults/keys/read");
        assert_eq!(a("POST", "/keys/k/abc123/sign").action, "Microsoft.KeyVault/vaults/keys/sign/action");
        assert_eq!(a("GET", "/certificates/issuers").policy, Some(("certificates", "listissuers")));
        assert!(classify(&req("POST", "/rng")).is_none());
    }

    #[test]
    fn a_token_that_is_wrong_is_401_with_the_challenge_and_the_text_of_its_kind() {
        let auth = Auth::strict();
        auth.set_authority(std::sync::Arc::new(TestAuthority));
        let mut r = req("GET", "/secrets/s1");
        let t = TestAuthority::token("p", "https://storage.azure.com");
        r.headers.insert("authorization", format!("Bearer {t}").parse().unwrap());
        let a = gate(&auth, &r).unwrap();
        assert_eq!(a.status(), 401);
        assert!(a.headers()["www-authenticate"].to_str().unwrap().starts_with("Bearer authorization=\"https://login.microsoftonline.com/00000000-0000-0000-0000-000000000000\", resource=\"https://vault.localhost:1\""));
        let body: serde_json::Value = serde_json::from_slice(a.body()).unwrap();
        assert_eq!(
            body["error"]["message"],
            format!("AKV10022: Invalid audience. Expected {RESOURCE_APP_ID}, found: https://storage.azure.com.")
        );
    }
}
