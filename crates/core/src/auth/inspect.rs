//! The inspector of the authorisation registry (`docs/inspector.md`): the role assignments and role definitions that strict mode
//! (`docs/strict-auth.md`) decides with. Role assignments Resource Manager was asked for live here, not in its resources. The
//! authorization rules of Service Bus and Event Hubs are shown by those services, where they live.

use serde_json::{json, Value};

use super::roles::{Assignment, RoleDef};
use super::AuthService;
use crate::inspect::{InspectError, InspectRequest, InspectResult, Node};

const ACCOUNT: &str = "registry";

impl AuthService {
    pub(crate) fn inspect_node(&self, req: &InspectRequest) -> InspectResult {
        let auth = &self.auth;
        let segs: Vec<&str> = req.segments.iter().map(String::as_str).collect();
        match segs.as_slice() {
            [] => Ok(Node::new(req.path(&[]), "service", "auth")
                .prop("accounts", 1)
                .with_items(vec![json!({"name": ACCOUNT, "path": req.path(&[ACCOUNT])})], None)),
            [ACCOUNT] => {
                let reg = auth.registry();
                let roles = reg.roles();
                let assignments = reg.assignments();
                Ok(Node::new(req.path(&[ACCOUNT]), "registry", ACCOUNT)
                    .prop("strict", auth.is_strict())
                    .prop("checksRoles", auth.enforced_services())
                    .prop("identityService", auth.authority().is_some())
                    .prop("configFile", auth.config_file().map(|p| p.display().to_string()))
                    .child("assignments", "assignments", Some(assignments.len() as u64), req.path(&[ACCOUNT, "assignments"]))
                    .child("roles", "roles", Some(roles.len() as u64), req.path(&[ACCOUNT, "roles"])))
            }
            [ACCOUNT, "assignments"] => {
                let list = auth.registry().assignments();
                let want_principal = req.param("principal").map(str::to_ascii_lowercase);
                let rows: Vec<&Assignment> = list.iter().filter(|a| want_principal.as_ref().is_none_or(|p| &a.principal_id == p)).collect();
                let (range, next) = req.page_range(rows.len());
                let items = rows[range].iter().map(|a| assignment_row(auth, a)).collect();
                Ok(Node::new(req.path(&[ACCOUNT, "assignments"]), "assignments", "role assignments")
                    .accepts(&["principal"])
                    .prop("count", rows.len())
                    .with_items(items, next))
            }
            [ACCOUNT, "roles"] => {
                let all = auth.registry().roles();
                let want = req.param("name").map(str::to_ascii_lowercase);
                let rows: Vec<&RoleDef> =
                    all.iter().filter(|r| want.as_ref().is_none_or(|w| r.name.to_ascii_lowercase().contains(w))).collect();
                let (range, next) = req.page_range(rows.len());
                let items = rows[range].iter().map(|r| role_row(r)).collect();
                Ok(Node::new(req.path(&[ACCOUNT, "roles"]), "roles", "role definitions")
                    .accepts(&["name"])
                    .prop("count", rows.len())
                    .with_items(items, next))
            }
            _ => Err(InspectError::not_found(format!("There is no node {}.", req.own_path()))),
        }
    }
}

fn assignment_row(auth: &super::Auth, a: &Assignment) -> Value {
    json!({
        "name": a.id,
        "roleName": auth.registry().role_by_id(&a.role_id).map(|r| r.name),
        "roleId": a.role_id,
        "principalId": a.principal_id,
        "principalType": a.principal_type,
        "scope": a.scope,
        "condition": a.condition,
        "description": a.description,
        "createdOn": a.created_on,
        "origin": format!("{:?}", a.origin),
    })
}

fn role_row(r: &RoleDef) -> Value {
    json!({
        "name": r.name,
        "id": r.id,
        "builtin": r.builtin,
        "description": r.description,
        "actions": r.actions,
        "notActions": r.not_actions,
        "dataActions": r.data_actions,
        "notDataActions": r.not_data_actions,
        "assignableScopes": r.assignable_scopes,
    })
}
