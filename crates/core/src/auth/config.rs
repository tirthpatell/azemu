//! The JSON that the admin API and the `--auth-config` file share: assignments, custom roles, Cosmos DB assignments, vault policies.
//!
//! ```json
//! {
//!   "assignments": [
//!     {"clientId": "<client id>", "role": "Storage Blob Data Reader", "scope": "storage/myaccount/containers/data"},
//!     {"principalId": "<object id>", "role": "Key Vault Secrets User", "scope": "/subscriptions/.../vaults/v"},
//!     {"managedIdentity": true, "role": "Storage Queue Data Contributor", "scope": "storage/myaccount"}
//!   ],
//!   "roles": [{"name": "Blob lister", "dataActions": ["Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read"]}],
//!   "cosmosAssignments": [{"account": "myaccount", "clientId": "<client id>", "role": "Contributor", "scope": "/dbs/app"}],
//!   "vaults": [{"vault": "myvault", "policies": [{"clientId": "<client id>", "secrets": ["get", "list"]}]}]
//! }
//! ```

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::roles::{Assignment, CosmosAssignment, Origin, RoleDef, VaultAccess, VaultPolicy};
use super::scope::{self, Resource};
use super::Auth;

/// A mistake in a request or in the file: an Azure-style code and the text.
#[derive(Debug)]
pub struct Problem {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

fn bad(code: &'static str, message: impl Into<String>) -> Problem {
    Problem { status: 400, code, message: message.into() }
}

/// A GUID derived from text, stable: the same assignment given twice has the same name.
pub fn derived_guid(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    h.update(b"azemu-auth-v1");
    for p in parts {
        h.update([0u8]);
        h.update(p.as_bytes());
    }
    let d = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(b).to_string()
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn strings(v: &Value, k: &str) -> Vec<String> {
    v.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

/// The object id a request names: `principalId` (or `principal`), `clientId` (resolved through the identity service) or `managedIdentity`.
pub fn principal_of(auth: &Auth, v: &Value) -> Result<String, Problem> {
    if let Some(p) = str_of(v, "principalId").or_else(|| str_of(v, "principal")).or_else(|| str_of(v, "objectId")) {
        if p.eq_ignore_ascii_case("managed-identity") {
            return managed_identity_oid(auth);
        }
        return Ok(p.to_ascii_lowercase());
    }
    if let Some(c) = str_of(v, "clientId") {
        return auth.object_id_of_client(c).ok_or_else(|| {
            bad("NoIdentityService", "a client id cannot be resolved here: there is no identity service. Give principalId instead.")
        });
    }
    if v.get("managedIdentity").and_then(Value::as_bool) == Some(true) {
        return managed_identity_oid(auth);
    }
    Err(bad("MissingPrincipal", "name the principal: principalId (an object id), clientId (a client id) or managedIdentity: true"))
}

fn managed_identity_oid(auth: &Auth) -> Result<String, Problem> {
    auth.authority()
        .map(|a| a.managed_identity().1)
        .ok_or_else(|| bad("NoIdentityService", "there is no identity service to say what the managed identity is"))
}

/// A scope given as an Azure id or as a short form, as the id it is stored under.
pub fn scope_of(auth: &Auth, text: &str) -> Result<String, Problem> {
    let t = text.trim();
    if t.starts_with('/') {
        return Ok(t.trim_end_matches('/').to_string()).map(|s| if s.is_empty() { "/".to_string() } else { s });
    }
    let res = scope::parse_short(t).map_err(|m| bad("InvalidScope", m))?;
    Ok(auth.registry().display_id(&res))
}

/// Create an assignment from the JSON of a request or of the file.
pub fn put_assignment(auth: &Auth, v: &Value, origin: Origin) -> Result<(Assignment, bool), Problem> {
    let principal = principal_of(auth, v)?;
    let role_text = str_of(v, "role")
        .or_else(|| str_of(v, "roleDefinitionId"))
        .ok_or_else(|| bad("MissingRole", "name the role: role (a name or an id)"))?;
    let role = auth.registry().find_role(role_text).ok_or_else(|| {
        bad(
            "RoleDefinitionDoesNotExist",
            format!("The specified role definition '{role_text}' does not exist. GET /_azemu/auth/roles lists them."),
        )
    })?;
    let scope = scope_of(
        auth,
        str_of(v, "scope")
            .ok_or_else(|| bad("MissingScope", "name the scope: an Azure id, or a short form such as storage/myaccount/containers/data"))?,
    )?;
    let name =
        str_of(v, "name").map(str::to_ascii_lowercase).unwrap_or_else(|| derived_guid(&[&principal, &role.id, &scope::normalize(&scope)]));
    let a = Assignment {
        id: name,
        scope,
        role_id: role.id,
        principal_id: principal,
        principal_type: str_of(v, "principalType").unwrap_or("ServicePrincipal").to_string(),
        condition: str_of(v, "condition").map(str::to_string),
        condition_version: str_of(v, "conditionVersion").map(str::to_string),
        description: str_of(v, "description").map(str::to_string),
        created_on: String::new(),
        updated_on: String::new(),
        origin,
    };
    auth.registry().put_assignment(a).map_err(|e| match e {
        super::roles::AssignError::UnknownRole(r) => {
            bad("RoleDefinitionDoesNotExist", format!("The specified role definition '{r}' does not exist."))
        }
        super::roles::AssignError::Exists => {
            Problem { status: 409, code: "RoleAssignmentExists", message: "The role assignment already exists.".into() }
        }
        super::roles::AssignError::UpdateNotPermitted => Problem {
            status: 409,
            code: "RoleAssignmentUpdateNotPermitted",
            message: "Tenant ID, application ID, principal ID, and scope are not allowed to be updated.".into(),
        },
    })
}

pub fn put_role(auth: &Auth, v: &Value) -> Result<(RoleDef, bool), Problem> {
    let name = str_of(v, "name").or_else(|| str_of(v, "roleName")).ok_or_else(|| bad("MissingName", "name the role: name"))?;
    let id = str_of(v, "id").map(str::to_ascii_lowercase).unwrap_or_else(|| derived_guid(&["role", &name.to_ascii_lowercase()]));
    let def = RoleDef {
        id,
        name: name.to_string(),
        description: str_of(v, "description").unwrap_or("").to_string(),
        builtin: false,
        actions: strings(v, "actions"),
        not_actions: strings(v, "notActions"),
        data_actions: strings(v, "dataActions"),
        not_data_actions: strings(v, "notDataActions"),
        assignable_scopes: {
            let s = strings(v, "assignableScopes");
            if s.is_empty() {
                vec!["/".into()]
            } else {
                s
            }
        },
        created_on: String::new(),
        updated_on: String::new(),
    };
    auth.registry().put_role(def).map_err(|e| match e {
        super::roles::RoleError::ReadOnly => {
            Problem { status: 409, code: "BuiltInRoleReadOnly", message: "A built-in role cannot be changed.".into() }
        }
        super::roles::RoleError::NameTaken => Problem {
            status: 409,
            code: "RoleDefinitionWithSameNameExists",
            message: "A role definition with the same name already exists.".into(),
        },
        _ => bad("InvalidRole", "the role could not be stored"),
    })
}

pub fn put_cosmos_assignment(auth: &Auth, v: &Value, origin: Origin) -> Result<CosmosAssignment, Problem> {
    let account = str_of(v, "account").ok_or_else(|| bad("MissingAccount", "name the Cosmos DB account: account"))?;
    let principal = principal_of(auth, v)?;
    let role_text = str_of(v, "role").ok_or_else(|| bad("MissingRole", "name the role: role (Reader, Contributor, a name or an id)"))?;
    let role = auth
        .registry()
        .find_cosmos_role(account, role_text)
        .ok_or_else(|| bad("RoleDefinitionDoesNotExist", format!("The Cosmos DB role '{role_text}' does not exist on {account}.")))?;
    let scope = str_of(v, "scope").unwrap_or("/").to_string();
    let a = CosmosAssignment {
        id: str_of(v, "name")
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| derived_guid(&["cosmos", account, &principal, &role.id, &scope])),
        role_id: role.id,
        principal_id: principal,
        scope,
        origin,
    };
    auth.registry().put_cosmos_assignment(account, a.clone());
    Ok(a)
}

fn policy_of(auth: &Auth, v: &Value) -> Result<VaultPolicy, Problem> {
    let low = |k: &str| strings(v, k).into_iter().map(|s| s.to_ascii_lowercase()).collect::<Vec<_>>();
    Ok(VaultPolicy {
        object_id: principal_of(auth, v)?,
        secrets: low("secrets"),
        keys: low("keys"),
        certificates: low("certificates"),
        storage: low("storage"),
    })
}

pub fn put_vault_policies(auth: &Auth, v: &Value, origin: Origin) -> Result<String, Problem> {
    let vault = str_of(v, "vault").ok_or_else(|| bad("MissingVault", "name the vault: vault"))?;
    let mut list = Vec::new();
    for p in v.get("policies").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
        list.push(policy_of(auth, p)?);
    }
    auth.registry().set_vault_access(vault, VaultAccess::Policies(list, origin));
    Ok(vault.to_string())
}

/// What the `--auth-config` file holds, applied.
pub fn apply_file(auth: &Auth, doc: &Value) -> Result<usize, String> {
    let mut n = 0;
    for (i, r) in doc.get("roles").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]).iter().enumerate() {
        put_role(auth, r).map_err(|p| format!("roles[{i}]: {}", p.message))?;
        n += 1;
    }
    for (i, a) in doc.get("assignments").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]).iter().enumerate() {
        match put_assignment(auth, a, Origin::File) {
            Ok(_) => {}
            // the same assignment twice is not a mistake
            Err(p) if p.code == "RoleAssignmentExists" => {}
            Err(p) => return Err(format!("assignments[{i}]: {}", p.message)),
        }
        n += 1;
    }
    for (i, a) in doc.get("cosmosAssignments").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]).iter().enumerate() {
        put_cosmos_assignment(auth, a, Origin::File).map_err(|p| format!("cosmosAssignments[{i}]: {}", p.message))?;
        n += 1;
    }
    for (i, a) in doc.get("vaults").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]).iter().enumerate() {
        put_vault_policies(auth, a, Origin::File).map_err(|p| format!("vaults[{i}]: {}", p.message))?;
        n += 1;
    }
    if let Some(obj) = doc.as_object() {
        for k in obj.keys() {
            if !["assignments", "roles", "cosmosAssignments", "vaults"].contains(&k.as_str()) {
                return Err(format!("unknown key '{k}' (known: assignments, roles, cosmosAssignments, vaults)"));
            }
        }
    } else {
        return Err("the file must hold one JSON object".to_string());
    }
    Ok(n)
}

/// Read the file and apply it. The message names the file.
pub fn load_file(auth: &Auth, path: &std::path::Path) -> Result<usize, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let doc: Value = serde_json::from_slice(&bytes).map_err(|e| format!("{}: not JSON: {e}", path.display()))?;
    apply_file(auth, &doc).map_err(|e| format!("{}: {e}", path.display()))
}

// -- how things look to a client -------------------------------------------------------------------------------

/// The id a role definition has in Azure's answers.
pub fn role_definition_id(sub: Option<&str>, id: &str) -> String {
    match sub {
        Some(s) => format!("/subscriptions/{s}/providers/Microsoft.Authorization/roleDefinitions/{id}"),
        None => format!("/providers/Microsoft.Authorization/roleDefinitions/{id}"),
    }
}

/// The subscription an Azure id is in, when it names one.
pub fn subscription_of(id: &str) -> Option<String> {
    let mut it = id.split('/').filter(|s| !s.is_empty());
    match (it.next(), it.next()) {
        (Some(a), Some(b)) if a.eq_ignore_ascii_case("subscriptions") => Some(b.to_string()),
        _ => None,
    }
}

/// A role assignment as Resource Manager shows it.
pub fn assignment_json(a: &Assignment) -> Value {
    let sub = subscription_of(&a.scope);
    json!({
        "id": format!("{}/providers/Microsoft.Authorization/roleAssignments/{}", a.scope.trim_end_matches('/'), a.id),
        "type": "Microsoft.Authorization/roleAssignments",
        "name": a.id,
        "properties": {
            "roleDefinitionId": role_definition_id(sub.as_deref(), &a.role_id),
            "principalId": a.principal_id,
            "principalType": a.principal_type,
            "scope": a.scope,
            "condition": a.condition,
            "conditionVersion": a.condition_version,
            "description": a.description,
            "createdOn": a.created_on,
            "updatedOn": a.updated_on,
            "createdBy": null,
            "updatedBy": null,
            "delegatedManagedIdentityResourceId": null
        }
    })
}

/// A role definition as Resource Manager shows it. `sub`: the subscription whose path the id carries.
pub fn role_json(r: &RoleDef, sub: Option<&str>) -> Value {
    json!({
        "id": role_definition_id(sub, &r.id),
        "type": "Microsoft.Authorization/roleDefinitions",
        "name": r.id,
        "properties": {
            "roleName": r.name,
            "type": if r.builtin { "BuiltInRole" } else { "CustomRole" },
            "description": r.description,
            "assignableScopes": r.assignable_scopes,
            "permissions": [{
                "actions": r.actions,
                "notActions": r.not_actions,
                "dataActions": r.data_actions,
                "notDataActions": r.not_data_actions
            }],
            "createdOn": r.created_on,
            "updatedOn": r.updated_on,
            "createdBy": null,
            "updatedBy": null
        }
    })
}

/// The part of an Azure id that is a data resource (`scope::resource_of_id`) or a short form, for a question to the registry.
pub fn resource_of_text(text: &str) -> Result<Resource, String> {
    let t = text.trim();
    if t.starts_with('/') {
        scope::resource_of_id(t)
            .ok_or_else(|| format!("'{t}' names no resource of a service (it needs /providers/<namespace>/<type>/<name>)"))
    } else {
        scope::parse_short(t)
    }
}
