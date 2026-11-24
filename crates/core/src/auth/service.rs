//! The admin API of strict mode, `/_azemu/auth`, and what is saved with `--data-dir`.
//!
//! | Request | What it does |
//! |---|---|
//! | `GET /_azemu/auth` | whether the mode is on, which services check roles, counts |
//! | `GET /_azemu/auth/assignments[?principal=&role=&scope=]` | the role assignments |
//! | `PUT /_azemu/auth/assignments` | give a role (body: see `config.rs`); the same JSON as one entry of `assignments` in the file |
//! | `DELETE /_azemu/auth/assignments?name=<guid>` (or `principal=`, `role=`, `scope=`; `all=true` for every one) | take roles away |
//! | `GET /_azemu/auth/roles[?name=]`, `PUT`, `DELETE ?id=` | role definitions; a custom role by `PUT` |
//! | `GET /_azemu/auth/check?principal=&action=&scope=[&kind=control]` | what the registry decides and which assignments it looked at |
//! | `GET/PUT/DELETE /_azemu/auth/cosmos/assignments` | Cosmos DB's own role assignments |
//! | `GET/PUT/DELETE /_azemu/auth/keyvault/policies` | access policies for a vault Resource Manager did not make |

use std::path::Path;

use serde_json::{json, Value};

use super::config::{self, Problem};
use super::roles::{Need, Origin, VaultAccess};
use super::scope;
use super::Auth;
use crate::resp::Resp;
use crate::{async_trait, json_error, Mount, Request, Response, Service};

const PREFIX: &str = "/_azemu/auth";
const FILE: &str = "auth.state";
const KIND: &str = "auth";
const VERSION: u32 = 1;

pub struct AuthService {
    pub(super) auth: Auth,
}

impl AuthService {
    pub fn new(auth: Auth) -> AuthService {
        AuthService { auth }
    }
}

fn problem(p: Problem) -> Response {
    json_error::response(p.status, p.code, &p.message)
}

fn ok(v: &Value) -> Response {
    Resp::new(200u16).json(v).build()
}

#[allow(clippy::result_large_err)] // the error is the finished answer
fn body_json(req: &Request) -> Result<Value, Response> {
    serde_json::from_slice(&req.body).map_err(|e| json_error::response(400, "InvalidRequestBody", &format!("The body is not JSON: {e}")))
}

fn assignment_view(auth: &Auth, a: &super::roles::Assignment) -> Value {
    let mut v = config::assignment_json(a);
    if let Some(r) = auth.registry().role_by_id(&a.role_id) {
        v["properties"]["roleName"] = json!(r.name);
    }
    v
}

#[async_trait]
impl Service for AuthService {
    fn name(&self) -> &'static str {
        "auth"
    }

    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::PathPrefix(PREFIX)]
    }

    fn max_body_bytes(&self, _m: &http::Method, _p: &str) -> Option<usize> {
        Some(1024 * 1024)
    }

    fn strict_auth_on(&self) -> bool {
        self.auth.is_strict()
    }

    fn inspect(&self, req: &crate::inspect::InspectRequest) -> crate::inspect::InspectResult {
        self.inspect_node(req)
    }

    /// The admin API itself is not checked in strict mode: it can grant anything to anyone (`docs/strict-auth.md`).
    fn enforces_strict_auth(&self) -> bool {
        true
    }

    async fn handle(&self, req: Request) -> Response {
        let segs = req.segments();
        let rest: Vec<&str> = segs.iter().skip(2).map(String::as_str).collect(); // _azemu, auth, ...
        let m = req.method.as_str();
        let auth = &self.auth;
        match (m, rest.as_slice()) {
            ("GET" | "HEAD", []) => ok(&json!({
                "strict": auth.is_strict(),
                "checksRoles": auth.enforced_services(),
                "identityService": auth.authority().is_some(),
                "configFile": auth.config_file().map(|p| p.display().to_string()),
                "assignments": auth.registry().assignments().len(),
                "customRoles": auth.registry().roles().iter().filter(|r| !r.builtin).count(),
                "endpoints": ["assignments", "roles", "check", "cosmos/assignments", "keyvault/policies"],
            })),
            ("GET" | "HEAD", ["assignments"]) => {
                let want_principal = match req.query.get("principal").or_else(|| req.query.get("principalId")) {
                    Some(p) => Some(p.to_ascii_lowercase()),
                    None => match req.query.get("clientId") {
                        Some(c) => auth.object_id_of_client(c),
                        None => None,
                    },
                };
                let want_scope = req.query.get("scope").map(|s| config::scope_of(auth, s)).transpose();
                let want_scope = match want_scope {
                    Ok(s) => s.map(|s| scope::normalize(&s)),
                    Err(p) => return problem(p),
                };
                let want_role = req.query.get("role").and_then(|r| auth.registry().find_role(r)).map(|r| r.id);
                if req.query.get("role").is_some() && want_role.is_none() {
                    return ok(&json!({"value": []}));
                }
                let list: Vec<Value> = auth
                    .registry()
                    .assignments()
                    .iter()
                    .filter(|a| want_principal.as_ref().is_none_or(|p| *p == a.principal_id))
                    .filter(|a| want_role.as_ref().is_none_or(|r| *r == a.role_id))
                    .filter(|a| want_scope.as_ref().is_none_or(|s| scope::is_within(s, &scope::normalize(&a.scope))))
                    .map(|a| assignment_view(auth, a))
                    .collect();
                ok(&json!({"value": list}))
            }
            ("PUT" | "POST", ["assignments"]) => {
                let v = match body_json(&req) {
                    Ok(v) => v,
                    Err(r) => return r,
                };
                match config::put_assignment(auth, &v, Origin::Api) {
                    Ok((a, created)) => Resp::new(if created { 201u16 } else { 200u16 }).json(&assignment_view(auth, &a)).build(),
                    Err(p) => problem(p),
                }
            }
            ("DELETE", ["assignments"]) => {
                let q = &req.query;
                if let Some(name) = q.get("name") {
                    return match auth.registry().delete_assignment_by_name(name) {
                        Some(a) => ok(&assignment_view(auth, &a)),
                        None => json_error::response(404, "RoleAssignmentNotFound", &format!("No role assignment named '{name}'.")),
                    };
                }
                let all = q.get("all").is_some_and(|v| v.eq_ignore_ascii_case("true"));
                if !all && !(q.has("principal") || q.has("clientId") || q.has("role") || q.has("scope")) {
                    return json_error::response(
                        400,
                        "MissingParameter",
                        "Say which: name=<guid>, or principal= / clientId= / role= / scope= to filter, or all=true for every assignment.",
                    );
                }
                let principal =
                    q.get("principal").map(str::to_ascii_lowercase).or_else(|| q.get("clientId").and_then(|c| auth.object_id_of_client(c)));
                let role = q.get("role").and_then(|r| auth.registry().find_role(r)).map(|r| r.id);
                let scope_text = match q.get("scope").map(|s| config::scope_of(auth, s)).transpose() {
                    Ok(s) => s.map(|s| scope::normalize(&s)),
                    Err(p) => return problem(p),
                };
                let mut removed = Vec::new();
                for a in auth.registry().assignments() {
                    let hit = principal.as_ref().is_none_or(|p| *p == a.principal_id)
                        && role.as_ref().is_none_or(|r| *r == a.role_id)
                        && scope_text.as_ref().is_none_or(|s| *s == scope::normalize(&a.scope));
                    if hit && (all || principal.is_some() || role.is_some() || scope_text.is_some()) {
                        if let Some(r) = auth.registry().delete_assignment(&a.scope, &a.id) {
                            removed.push(assignment_view(auth, &r));
                        }
                    }
                }
                ok(&json!({"removed": removed}))
            }
            ("GET" | "HEAD", ["roles"]) => {
                let name = req.query.get("name");
                let list: Vec<Value> = auth
                    .registry()
                    .roles()
                    .iter()
                    .filter(|r| name.is_none_or(|n| r.name.eq_ignore_ascii_case(n)))
                    .map(|r| config::role_json(r, None))
                    .collect();
                ok(&json!({"value": list}))
            }
            ("PUT" | "POST", ["roles"]) => {
                let v = match body_json(&req) {
                    Ok(v) => v,
                    Err(r) => return r,
                };
                match config::put_role(auth, &v) {
                    Ok((r, created)) => Resp::new(if created { 201u16 } else { 200u16 }).json(&config::role_json(&r, None)).build(),
                    Err(p) => problem(p),
                }
            }
            ("DELETE", ["roles"]) => {
                let Some(id) = req.query.get("id") else {
                    return json_error::response(400, "MissingParameter", "Say which role: id=<guid>.");
                };
                match auth.registry().delete_role(id) {
                    Ok(r) => ok(&config::role_json(&r, None)),
                    Err(super::roles::RoleError::ReadOnly) => {
                        json_error::response(409, "BuiltInRoleReadOnly", "A built-in role cannot be deleted.")
                    }
                    Err(super::roles::RoleError::InUse) => json_error::response(
                        400,
                        "RoleDefinitionHasAssignments",
                        "There are existing role assignments referencing the role.",
                    ),
                    Err(_) => json_error::response(404, "RoleDefinitionNotFound", &format!("No custom role '{id}'.")),
                }
            }
            ("GET" | "HEAD", ["check"]) => self.check(&req),
            ("GET" | "HEAD", ["cosmos", "assignments"]) => {
                let Some(account) = req.query.get("account") else {
                    return json_error::response(400, "MissingParameter", "Say which account: account=<name>.");
                };
                let list: Vec<Value> = auth
                    .registry()
                    .cosmos_assignments(account)
                    .iter()
                    .map(|a| json!({"name": a.id, "roleDefinitionId": a.role_id, "principalId": a.principal_id, "scope": a.scope}))
                    .collect();
                ok(&json!({"value": list}))
            }
            ("PUT" | "POST", ["cosmos", "assignments"]) => {
                let v = match body_json(&req) {
                    Ok(v) => v,
                    Err(r) => return r,
                };
                match config::put_cosmos_assignment(auth, &v, Origin::Api) {
                    Ok(a) => Resp::new(200u16)
                        .json(&json!({"name": a.id, "roleDefinitionId": a.role_id, "principalId": a.principal_id, "scope": a.scope}))
                        .build(),
                    Err(p) => problem(p),
                }
            }
            ("DELETE", ["cosmos", "assignments"]) => {
                let (Some(account), Some(name)) = (req.query.get("account"), req.query.get("name")) else {
                    return json_error::response(400, "MissingParameter", "Say which: account=<name>&name=<guid>.");
                };
                match auth.registry().delete_cosmos_assignment(account, name) {
                    Some(_) => ok(&json!({})),
                    None => json_error::response(
                        404,
                        "RoleAssignmentNotFound",
                        &format!("No Cosmos DB role assignment named '{name}' on {account}."),
                    ),
                }
            }
            ("GET" | "HEAD", ["keyvault", "policies"]) => {
                let Some(vault) = req.query.get("vault") else {
                    return json_error::response(400, "MissingParameter", "Say which vault: vault=<name>.");
                };
                match auth.registry().vault_access(vault) {
                    VaultAccess::Rbac => ok(&json!({"vault": vault, "mode": "rbac"})),
                    VaultAccess::Policies(p, _) => ok(&json!({"vault": vault, "mode": "policies", "policies": p})),
                }
            }
            ("PUT" | "POST", ["keyvault", "policies"]) => {
                let v = match body_json(&req) {
                    Ok(v) => v,
                    Err(r) => return r,
                };
                match config::put_vault_policies(auth, &v, Origin::Api) {
                    Ok(vault) => ok(&json!({"vault": vault, "mode": "policies"})),
                    Err(p) => problem(p),
                }
            }
            ("DELETE", ["keyvault", "policies"]) => {
                let Some(vault) = req.query.get("vault") else {
                    return json_error::response(400, "MissingParameter", "Say which vault: vault=<name>.");
                };
                auth.registry().clear_vault_access(vault);
                ok(&json!({"vault": vault, "mode": "rbac"}))
            }
            _ => json_error::response(
                404,
                "NotFound",
                &format!("Unknown path {}. GET /_azemu/auth lists the endpoints.", req.decoded_path()),
            ),
        }
    }

    fn reset(&self) {
        self.auth.registry().clear();
        if let Some(path) = self.auth.config_file() {
            if let Err(e) = config::load_file(&self.auth, path) {
                tracing::error!("the auth config file could not be applied after a reset (it worked at start): {e}");
            }
        }
    }

    fn persist_generation(&self) -> Option<u64> {
        Some(self.auth.registry().generation())
    }

    fn save(&self, dir: &Path) -> std::io::Result<()> {
        crate::persist::save_file(dir, FILE, KIND, VERSION, &self.auth.registry().snapshot())
    }

    fn load(&self, dir: &Path) -> std::io::Result<()> {
        if let Some((_, saved)) = crate::persist::load_file::<super::roles::Saved>(dir, FILE, KIND, VERSION)? {
            self.auth.registry().restore(saved);
        }
        Ok(())
    }

    fn started(&self) -> Result<(), String> {
        if let Some(path) = self.auth.config_file() {
            let n = config::load_file(&self.auth, path)?;
            tracing::info!(file = %path.display(), entries = n, "auth config file applied");
        }
        Ok(())
    }
}

impl AuthService {
    fn check(&self, req: &Request) -> Response {
        let auth = &self.auth;
        let q = &req.query;
        let principal = match q.get("principal").or_else(|| q.get("principalId")) {
            Some(p) => p.to_ascii_lowercase(),
            None => match q.get("clientId") {
                Some(c) => match auth.object_id_of_client(c) {
                    Some(o) => o,
                    None => {
                        return json_error::response(
                            400,
                            "NoIdentityService",
                            "A client id cannot be resolved: there is no identity service.",
                        )
                    }
                },
                None => return json_error::response(400, "MissingParameter", "Say who: principal=<object id> (or clientId=<client id>)."),
            },
        };
        let Some(action) = q.get("action") else {
            return json_error::response(
                400,
                "MissingParameter",
                "Say what: action=<data action>, for example Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read.",
            );
        };
        if let Some(account) = q.get("cosmosAccount") {
            let d = auth.registry().cosmos_check(account, &principal, action, q.get("scope").unwrap_or("/"));
            return ok(&serde_json::to_value(&d).unwrap_or(Value::Null));
        }
        let Some(scope_text) = q.get("scope") else {
            return json_error::response(
                400,
                "MissingParameter",
                "Say where: scope=<Azure id, or a short form such as storage/myaccount/containers/data>.",
            );
        };
        let res = match config::resource_of_text(scope_text) {
            Ok(r) => r,
            Err(m) => return json_error::response(400, "InvalidScope", &m),
        };
        let need =
            if q.get("kind").is_some_and(|k| k.eq_ignore_ascii_case("control")) { Need::control(action) } else { Need::data(action) };
        let d = auth.registry().check(&principal, &need, &res);
        ok(&serde_json::to_value(&d).unwrap_or(Value::Null))
    }
}
