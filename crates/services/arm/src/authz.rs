//! `Microsoft.Authorization/roleAssignments` and `roleDefinitions` at every scope, kept in the registry of `azemu_core::auth`; and the
//! hooks that tell the registry which data resources Resource Manager made (so that a scope covers them as Azure's does).
//!
//! These are answered whatever the mode is: a Terraform run must not depend on `--strict-auth`. They are *enforced* only in strict mode.
//! Resource Manager itself is not checked in strict mode: any token may call it.
//!
//! Error codes and texts that were not seen from Azure (the management plane was not probed) are marked in `docs/services/arm.md`.

use azemu_core::auth::config::{assignment_json, role_json, subscription_of};
use azemu_core::auth::{AssignError, Assignment, Origin, RoleDef, RoleError};
use azemu_core::Response;
use serde_json::{json, Value};

use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::ids::parse_scope;
use crate::state::{key, Stored};
use crate::util;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Assignments,
    Definitions,
}

/// Where `providers/Microsoft.Authorization/roleAssignments|roleDefinitions` is in a path: the index of `providers`, and which.
pub fn find(segs: &[String]) -> Option<(usize, Kind)> {
    (0..segs.len().saturating_sub(2)).rev().find_map(|i| {
        if !segs[i].eq_ignore_ascii_case("providers") || !segs[i + 1].eq_ignore_ascii_case("Microsoft.Authorization") {
            return None;
        }
        match segs[i + 2].to_ascii_lowercase().as_str() {
            "roleassignments" => Some((i, Kind::Assignments)),
            "roledefinitions" => Some((i, Kind::Definitions)),
            _ => None,
        }
    })
}

fn invalid(what: &str, value: &str, path: &str) -> ArmError {
    ArmError::bad_request(
        "InvalidRequestContent",
        format!("The request content was invalid and could not be deserialized: 'Error converting value \"{value}\" to type '{what}'. Path '{path}', line 1, position 0.'."),
    )
}

fn is_guid(s: &str) -> bool {
    s.len() == 36 && uuid::Uuid::parse_str(s).is_ok()
}

/// The scope in front of `providers/Microsoft.Authorization`, as an id; it must exist: the subscription is implicit, a resource group must
/// have been made, and so must the resource a scope names (the top-level one; the parts below it, a secret or a container, are not looked up).
fn check_scope(cx: &Cx<'_>, scope_segs: &[String]) -> ArmResult<String> {
    if scope_segs.is_empty() {
        return Ok("/".to_string());
    }
    let id = format!("/{}", scope_segs.join("/"));
    let sc = parse_scope(scope_segs);
    let (Some(sub), rest) = (&sc.sub, &sc.rest) else {
        // a management group or something else that is not under a subscription: accepted as it is
        return Ok(id);
    };
    let st = cx.inner.state.lock();
    if let Some(rg) = &sc.rg {
        let Some(g) = st.group(sub, rg) else {
            return Err(ArmError::group_not_found(rg));
        };
        let rg_name = g.name.clone();
        if rest.len() >= 4 && rest[0].eq_ignore_ascii_case("providers") {
            let top = format!("/subscriptions/{sub}/resourceGroups/{rg_name}/providers/{}/{}/{}", rest[1], rest[2], rest[3]);
            if !st.resources.contains_key(&key(&top)) {
                return Err(ArmError::resource_not_found(&format!("{}/{}", rest[1], rest[2]), &rest[3], rg));
            }
        }
    }
    Ok(id)
}

fn sub_of(scope: &str, cx: &Cx<'_>) -> Option<String> {
    subscription_of(scope).or_else(|| {
        cx.req.segments().get(1).filter(|_| cx.req.segments().first().is_some_and(|s| s.eq_ignore_ascii_case("subscriptions"))).cloned()
    })
}

pub fn handle(cx: &Cx<'_>, at: usize, kind: Kind) -> ArmResult<Response> {
    let segs = cx.req.segments();
    let scope = check_scope(cx, &segs[..at])?;
    let tail = &segs[at + 3..];
    let method = cx.req.method.as_str();
    match kind {
        Kind::Assignments => assignments(cx, &scope, tail, method),
        Kind::Definitions => definitions(cx, &scope, tail, method),
    }
}

fn assignments(cx: &Cx<'_>, scope: &str, tail: &[String], method: &str) -> ArmResult<Response> {
    let reg = cx.inner.auth.registry();
    match (method, tail) {
        ("GET", []) => {
            let filter = cx.req.query.get("$filter").unwrap_or("").to_string();
            let lower = filter.to_ascii_lowercase();
            let norm = azemu_core::auth::scope::normalize(scope);
            let principal = filter_arg(&filter, "principalId eq '").or_else(|| filter_arg(&filter, "assignedTo('"));
            let at_scope = lower.contains("atscope()");
            let list: Vec<Value> = reg
                .assignments()
                .iter()
                .filter(|a| {
                    let s = azemu_core::auth::scope::normalize(&a.scope);
                    if principal.is_some() {
                        // at, above or below the scope
                        azemu_core::auth::scope::is_within(&norm, &s) || azemu_core::auth::scope::is_within(&s, &norm)
                    } else if at_scope {
                        // at or above
                        azemu_core::auth::scope::is_within(&s, &norm)
                    } else {
                        // at or below
                        azemu_core::auth::scope::is_within(&norm, &s)
                    }
                })
                .filter(|a| principal.as_ref().is_none_or(|p| p.eq_ignore_ascii_case(&a.principal_id)))
                .map(|a| fix_definition_id(assignment_json(a), cx))
                .collect();
            Ok(util::list(list))
        }
        ("PUT", [name]) => {
            if !is_guid(name) {
                return Err(ArmError::bad_request(
                    "InvalidRoleAssignmentName",
                    format!("The role assignment name '{name}' is not valid: it must be a GUID."),
                ));
            }
            let body = util::body_object(&cx.req.body)?;
            let props = body.get("properties").cloned().unwrap_or(Value::Null);
            let role_text = props.get("roleDefinitionId").and_then(Value::as_str).unwrap_or("");
            let principal = props.get("principalId").and_then(Value::as_str).unwrap_or("");
            if principal.is_empty() {
                return Err(ArmError::bad_request(
                    "InvalidRequestContent",
                    "The request content was invalid and could not be deserialized: 'Required property 'principalId' not found in JSON.'.",
                ));
            }
            if !is_guid(principal) {
                return Err(invalid("System.Guid", principal, "properties.principalId"));
            }
            let Some(role) = reg.find_role(role_text).filter(|_| !role_text.is_empty()) else {
                return Err(ArmError::bad_request(
                    "RoleDefinitionDoesNotExist",
                    format!(
                        "The specified role definition with ID '{}' does not exist.",
                        role_text.rsplit('/').next().unwrap_or(role_text)
                    ),
                ));
            };
            let s = |k: &str| props.get(k).and_then(Value::as_str).map(str::to_string);
            let a = Assignment {
                id: name.to_ascii_lowercase(),
                scope: scope.to_string(),
                role_id: role.id,
                principal_id: principal.to_ascii_lowercase(),
                principal_type: s("principalType").unwrap_or_else(|| "ServicePrincipal".to_string()),
                condition: s("condition"),
                condition_version: s("conditionVersion"),
                description: s("description"),
                created_on: String::new(),
                updated_on: String::new(),
                origin: Origin::Arm,
            };
            match reg.put_assignment(a) {
                Ok((a, created)) => Ok(util::json_ok(if created { 201 } else { 200 }, &fix_definition_id(assignment_json(&a), cx))),
                Err(AssignError::Exists) => Err(ArmError::conflict("RoleAssignmentExists", "The role assignment already exists.")),
                Err(AssignError::UpdateNotPermitted) => Err(ArmError::conflict(
                    "RoleAssignmentUpdateNotPermitted",
                    "Tenant ID, application ID, principal ID, and scope are not allowed to be updated.",
                )),
                Err(AssignError::UnknownRole(r)) => Err(ArmError::bad_request(
                    "RoleDefinitionDoesNotExist",
                    format!("The specified role definition with ID '{r}' does not exist."),
                )),
            }
        }
        ("GET", [name]) => match reg.get_assignment(scope, name) {
            Some(a) => Ok(util::json_ok(200, &fix_definition_id(assignment_json(&a), cx))),
            None => Err(ArmError::not_found("RoleAssignmentNotFound", format!("The role assignment '{name}' could not be found."))),
        },
        ("DELETE", [name]) => match reg.delete_assignment(scope, name) {
            Some(a) => Ok(util::json_ok(200, &fix_definition_id(assignment_json(&a), cx))),
            None => Ok(util::empty(204)),
        },
        _ => Err(ArmError::new(405, "MethodNotAllowed", format!("The method {method} is not allowed on this resource."))),
    }
}

/// Azure writes the definition id with the subscription of the request.
fn fix_definition_id(mut v: Value, cx: &Cx<'_>) -> Value {
    let sub = sub_of(v["properties"]["scope"].as_str().unwrap_or(""), cx);
    if let (Some(sub), Some(id)) =
        (sub, v["properties"]["roleDefinitionId"].as_str().and_then(|r| r.rsplit('/').next()).map(str::to_string))
    {
        v["properties"]["roleDefinitionId"] = json!(format!("/subscriptions/{sub}/providers/Microsoft.Authorization/roleDefinitions/{id}"));
    }
    v
}

/// The text after `prefix` up to the next `'`.
fn filter_arg(filter: &str, prefix: &str) -> Option<String> {
    let lower = filter.to_ascii_lowercase();
    let at = lower.find(&prefix.to_ascii_lowercase())? + prefix.len();
    let rest = &filter[at..];
    Some(rest[..rest.find('\'')?].to_string())
}

fn definitions(cx: &Cx<'_>, scope: &str, tail: &[String], method: &str) -> ArmResult<Response> {
    let reg = cx.inner.auth.registry();
    let sub = sub_of(scope, cx);
    let view = |r: &RoleDef| role_json(r, sub.as_deref());
    match (method, tail) {
        ("GET", []) => {
            let filter = cx.req.query.get("$filter").unwrap_or("").to_string();
            let name = filter_arg(&filter, "roleName eq '");
            let custom_only = filter.to_ascii_lowercase().contains("type eq 'customrole'");
            let list: Vec<Value> = reg
                .roles()
                .iter()
                .filter(|r| name.as_ref().is_none_or(|n| r.name.eq_ignore_ascii_case(n)))
                .filter(|r| !custom_only || !r.builtin)
                .map(view)
                .collect();
            Ok(util::list(list))
        }
        ("GET", [id]) => match reg.role_by_id(id) {
            Some(r) => Ok(util::json_ok(200, &view(&r))),
            None => Err(ArmError::not_found(
                "RoleDefinitionDoesNotExist",
                format!("The specified role definition with ID '{id}' does not exist."),
            )),
        },
        ("PUT", [id]) => {
            if !is_guid(id) {
                return Err(ArmError::bad_request(
                    "InvalidRoleDefinitionName",
                    format!("The role definition name '{id}' is not valid: it must be a GUID."),
                ));
            }
            let body = util::body_object(&cx.req.body)?;
            let p = body.get("properties").cloned().unwrap_or(Value::Null);
            let name = p.get("roleName").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() {
                return Err(ArmError::bad_request(
                    "InvalidRequestContent",
                    "The request content was invalid and could not be deserialized: 'Required property 'roleName' not found in JSON.'.",
                ));
            }
            let perm = p.get("permissions").and_then(Value::as_array).and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
            let list = |v: &Value, k: &str| {
                v.get(k)
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default()
            };
            let def = RoleDef {
                id: id.to_ascii_lowercase(),
                name: name.to_string(),
                description: p.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
                builtin: false,
                actions: list(&perm, "actions"),
                not_actions: list(&perm, "notActions"),
                data_actions: list(&perm, "dataActions"),
                not_data_actions: list(&perm, "notDataActions"),
                assignable_scopes: list(&p, "assignableScopes"),
                created_on: String::new(),
                updated_on: String::new(),
            };
            match reg.put_role(def) {
                Ok((r, created)) => Ok(util::json_ok(if created { 201 } else { 200 }, &view(&r))),
                Err(RoleError::ReadOnly) => Err(ArmError::bad_request("BuiltInRoleReadOnly", "A built-in role cannot be changed.")),
                Err(RoleError::NameTaken) => {
                    Err(ArmError::conflict("RoleDefinitionWithSameNameExists", "A role definition with the same name already exists."))
                }
                Err(_) => Err(ArmError::bad_request("InvalidRequestContent", "The role definition could not be stored.")),
            }
        }
        ("DELETE", [id]) => match reg.delete_role(id) {
            Ok(r) => Ok(util::json_ok(200, &view(&r))),
            Err(RoleError::NotFound) => Ok(util::empty(204)),
            Err(RoleError::InUse) => Err(ArmError::bad_request(
                "RoleDefinitionHasAssignments",
                "There are existing role assignments referencing role (code: RoleDefinitionHasAssignments)",
            )),
            Err(_) => Err(ArmError::bad_request("BuiltInRoleReadOnly", "A built-in role cannot be deleted.")),
        },
        _ => Err(ArmError::new(405, "MethodNotAllowed", format!("The method {method} is not allowed on this resource."))),
    }
}

// -- what the registry learns about the resources Resource Manager stores ------------------------------------------

/// The data resources a scope can name: `(namespace, top-level type)` as the registry spells them.
const DATA_TYPES: &[(&str, &str)] = &[
    ("Microsoft.Storage", "storageAccounts"),
    ("Microsoft.KeyVault", "vaults"),
    ("Microsoft.AppConfiguration", "configurationStores"),
    ("Microsoft.EventGrid", "topics"),
    ("Microsoft.ServiceBus", "namespaces"),
    ("Microsoft.EventHub", "namespaces"),
    ("Microsoft.DocumentDB", "databaseAccounts"),
];

fn is_data_type(s: &Stored) -> bool {
    s.types.len() == 1 && DATA_TYPES.iter().any(|(ns, t)| s.ns.eq_ignore_ascii_case(ns) && s.types[0].eq_ignore_ascii_case(t))
}

/// A resource was stored: tell the registry its id; for a vault, how it is accessed; for a Cosmos DB role, the role.
pub fn on_put(auth: &azemu_core::auth::Auth, s: &Stored) {
    let reg = auth.registry();
    if is_data_type(s) {
        reg.register_resource(&format!("{}/{}", s.ns, s.types[0]), s.name(), &s.id);
        if s.ns.eq_ignore_ascii_case("Microsoft.KeyVault") {
            vault_access(auth, s);
        }
        return;
    }
    if s.ns.eq_ignore_ascii_case("Microsoft.DocumentDB") && s.types.len() == 2 {
        let account = s.name_at(0);
        let p = &s.body["properties"];
        match s.types[1].to_ascii_lowercase().as_str() {
            "sqlroledefinitions" => {
                let perm = p["permissions"].as_array().and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
                let list = |k: &str| -> Vec<String> {
                    perm[k].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
                };
                reg.put_cosmos_role(
                    account,
                    RoleDef {
                        id: s.name().to_string(),
                        name: p["roleName"].as_str().unwrap_or("").to_string(),
                        description: String::new(),
                        builtin: false,
                        actions: vec![],
                        not_actions: vec![],
                        data_actions: list("dataActions"),
                        not_data_actions: list("notDataActions"),
                        assignable_scopes: vec![],
                        created_on: String::new(),
                        updated_on: String::new(),
                    },
                    Origin::ArmStored,
                );
            }
            "sqlroleassignments" => {
                reg.put_cosmos_assignment(
                    account,
                    azemu_core::auth::CosmosAssignment {
                        id: s.name().to_string(),
                        role_id: p["roleDefinitionId"].as_str().unwrap_or("").to_string(),
                        principal_id: p["principalId"].as_str().unwrap_or("").to_string(),
                        scope: p["scope"].as_str().unwrap_or("").to_string(),
                        origin: Origin::ArmStored,
                    },
                );
            }
            _ => {}
        }
    }
}

/// `enableRbacAuthorization` and `accessPolicies` of a vault: which of the two kinds of vault it is, and the policies.
fn vault_access(auth: &azemu_core::auth::Auth, s: &Stored) {
    use azemu_core::auth::{VaultAccess, VaultPolicy};
    let p = &s.body["properties"];
    let rbac = p["enableRbacAuthorization"].as_bool().unwrap_or(false);
    if rbac {
        auth.registry().set_vault_access(s.name(), VaultAccess::Rbac);
        return;
    }
    let words = |v: &Value| -> Vec<String> {
        v.as_array().map(|a| a.iter().filter_map(Value::as_str).map(|w| w.to_ascii_lowercase()).collect()).unwrap_or_default()
    };
    let policies: Vec<VaultPolicy> = p["accessPolicies"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| VaultPolicy {
                    object_id: e["objectId"].as_str().unwrap_or("").to_ascii_lowercase(),
                    secrets: words(&e["permissions"]["secrets"]),
                    keys: words(&e["permissions"]["keys"]),
                    certificates: words(&e["permissions"]["certificates"]),
                    storage: words(&e["permissions"]["storage"]),
                })
                .collect()
        })
        .unwrap_or_default();
    auth.registry().set_vault_access(s.name(), VaultAccess::Policies(policies, Origin::ArmStored));
}

pub fn on_delete(auth: &azemu_core::auth::Auth, s: &Stored) {
    let reg = auth.registry();
    if is_data_type(s) {
        reg.unregister_resource(&format!("{}/{}", s.ns, s.types[0]), s.name());
        if s.ns.eq_ignore_ascii_case("Microsoft.KeyVault") {
            reg.clear_vault_access(s.name());
        }
        if s.ns.eq_ignore_ascii_case("Microsoft.DocumentDB") {
            reg.drop_cosmos_account(s.name(), Origin::ArmStored);
        }
    } else if s.ns.eq_ignore_ascii_case("Microsoft.DocumentDB") && s.types.len() == 2 {
        match s.types[1].to_ascii_lowercase().as_str() {
            "sqlroledefinitions" => {
                reg.delete_cosmos_role(s.name_at(0), s.name());
            }
            "sqlroleassignments" => {
                reg.delete_cosmos_assignment(s.name_at(0), s.name());
            }
            _ => {}
        }
    }
}

/// What Resource Manager stores is gone (a reset): forget what it told the registry.
pub fn on_reset(auth: &azemu_core::auth::Auth) {
    auth.registry().clear_arm_stored();
}

/// State was read from the data directory: tell the registry about every resource in it.
pub fn on_load(auth: &azemu_core::auth::Auth, st: &crate::state::State) {
    for s in st.resources.values() {
        on_put(auth, s);
    }
}
