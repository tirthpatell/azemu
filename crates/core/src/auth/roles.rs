//! Role definitions and role assignments, and the question "may this principal do this to that resource".
//!
//! Azure RBAC as documented: an assignment gives a principal a role definition at a scope; it applies to that scope and everything
//! below; what a principal may do is the **union** over all the assignments that apply; there are no deny assignments. A definition
//! lists `dataActions` and `notDataActions` (and `actions` / `notActions`, which only a few storage calls look at, see
//! [`Need::Control`]); `*` matches any run of characters; a `not` list takes away from the list of its own definition only.
//! Conditions (ABAC) on an assignment are stored and not evaluated.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::builtin::BUILTIN_ROLES;
use super::scope::{covers, normalize, Resource};
use super::wildcard::permitted;

/// A role definition: a built-in one, or a custom role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleDef {
    /// The GUID, lower case.
    pub id: String,
    pub name: String,
    pub description: String,
    pub builtin: bool,
    pub actions: Vec<String>,
    pub not_actions: Vec<String>,
    pub data_actions: Vec<String>,
    pub not_data_actions: Vec<String>,
    pub assignable_scopes: Vec<String>,
    pub created_on: String,
    pub updated_on: String,
}

/// Who made an assignment or a role: it decides what is saved with `--data-dir` (what Resource Manager keeps itself, or the
/// auth file puts back at start, is not saved a second time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    /// A Resource Manager request; the registry is the only store, so it is saved.
    Arm,
    /// Stored by Resource Manager as a resource of its own and put in the registry again when ARM loads its state.
    ArmStored,
    /// The admin API; saved.
    Api,
    /// `--auth-config`; put back from the file at start and after a reset.
    File,
}

impl Origin {
    pub fn saved(self) -> bool {
        matches!(self, Origin::Arm | Origin::Api)
    }
}

/// A role assignment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    /// The name: a GUID.
    pub id: String,
    /// The scope as it was given (an Azure id).
    pub scope: String,
    /// The GUID of the role definition.
    pub role_id: String,
    /// The object id of the principal, lower case.
    pub principal_id: String,
    pub principal_type: String,
    pub condition: Option<String>,
    pub condition_version: Option<String>,
    pub description: Option<String>,
    pub created_on: String,
    pub updated_on: String,
    pub origin: Origin,
}

/// What one requirement is made of. A leaf is one action name; storage calls need one of several, or two together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    /// A data action, looked up in `dataActions` / `notDataActions`.
    Data(String),
    /// An action looked up in `actions` / `notActions`. Microsoft lists a few storage calls with an action name and not a data action
    /// (List Containers, Get User Delegation Key, Get Blob Service Properties, ...). Nothing else asks for it, so a role such as
    /// Contributor, which carries `actions` only, never reads a blob.
    Control(String),
    Any(Vec<Need>),
    All(Vec<Need>),
}

impl Need {
    pub fn data(action: &str) -> Need {
        Need::Data(action.to_string())
    }
    pub fn control(action: &str) -> Need {
        Need::Control(action.to_string())
    }
    pub fn any(of: Vec<Need>) -> Need {
        Need::Any(of)
    }
    pub fn all(of: Vec<Need>) -> Need {
        Need::All(of)
    }

    fn leaves<'a>(&'a self, out: &mut Vec<&'a Need>) {
        match self {
            Need::Data(_) | Need::Control(_) => out.push(self),
            Need::Any(v) | Need::All(v) => v.iter().for_each(|n| n.leaves(out)),
        }
    }

    fn satisfied(&self, has: &dyn Fn(&Need) -> bool) -> bool {
        match self {
            Need::Data(_) | Need::Control(_) => has(self),
            Need::Any(v) => v.iter().any(|n| n.satisfied(has)),
            Need::All(v) => v.iter().all(|n| n.satisfied(has)),
        }
    }

    /// The action names, in words: `a`, `a or b`, `a and b`.
    pub fn describe(&self) -> String {
        match self {
            Need::Data(a) | Need::Control(a) => a.clone(),
            Need::Any(v) => v.iter().map(Need::describe).collect::<Vec<_>>().join(" or "),
            Need::All(v) => v.iter().map(Need::describe).collect::<Vec<_>>().join(" and "),
        }
    }

    /// The name of the first action, for a message that names one.
    pub fn first_action(&self) -> String {
        match self {
            Need::Data(a) | Need::Control(a) => a.clone(),
            Need::Any(v) | Need::All(v) => v.first().map(Need::first_action).unwrap_or_default(),
        }
    }
}

/// One assignment of the principal that was looked at.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Looked {
    pub assignment: String,
    pub scope: String,
    pub role: String,
    /// The assignment's scope applies to the resource.
    pub applies: bool,
    /// The actions asked for that this role grants (only when it applies).
    pub grants: Vec<String>,
}

/// The answer to "may it".
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub allowed: bool,
    /// The resource, as an Azure id (the one Resource Manager gave it, or the made-up one of an account it does not know).
    pub resource: String,
    /// What was asked for, in words.
    pub needs: String,
    /// Every assignment of the principal, and what it did.
    pub looked_at: Vec<Looked>,
    pub reason: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AssignError {
    UnknownRole(String),
    /// The same principal, role and scope already have an assignment, under another name.
    Exists,
    /// The name is taken by an assignment of another principal, role or scope.
    UpdateNotPermitted,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RoleError {
    /// A built-in role.
    ReadOnly,
    /// A role with that name exists.
    NameTaken,
    NotFound,
    /// Assignments still use it.
    InUse,
}

fn now_text() -> String {
    crate::time::iso8601_7(crate::time::OffsetDateTime::now_utc())
}

fn builtin_defs() -> &'static Vec<RoleDef> {
    static DEFS: std::sync::OnceLock<Vec<RoleDef>> = std::sync::OnceLock::new();
    DEFS.get_or_init(|| {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let mut out: Vec<RoleDef> = BUILTIN_ROLES
            .iter()
            .map(|b| RoleDef {
                id: b.id.to_ascii_lowercase(),
                name: b.name.to_string(),
                description: b.description.to_string(),
                builtin: true,
                actions: v(b.actions),
                not_actions: v(b.not_actions),
                data_actions: v(b.data_actions),
                not_data_actions: v(b.not_data_actions),
                assignable_scopes: vec!["/".to_string()],
                created_on: "2015-02-02T21:55:09.8806423Z".to_string(),
                updated_on: "2015-02-02T21:55:09.8806423Z".to_string(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    })
}

/// The built-in definitions (name order).
pub fn builtin_roles() -> &'static [RoleDef] {
    builtin_defs()
}

/// The pieces the registry keeps. Public to this crate so the admin service can save and load them.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Saved {
    pub roles: Vec<RoleDef>,
    pub assignments: Vec<Assignment>,
    pub cosmos_roles: Vec<(String, RoleDef)>,
    pub cosmos_assignments: Vec<(String, CosmosAssignment)>,
    pub vaults: Vec<(String, VaultAccess)>,
}

/// An assignment of Cosmos DB's own role system (`sqlRoleAssignments`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CosmosAssignment {
    pub id: String,
    pub role_id: String,
    pub principal_id: String,
    /// The scope as given: the account's id, or the id with `/dbs/{db}` or `/dbs/{db}/colls/{c}` after it; or just that path.
    pub scope: String,
    pub origin: Origin,
}

/// What Key Vault checks for a vault.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VaultAccess {
    /// Azure role-based access control (the default, and every vault Resource Manager does not know).
    Rbac,
    /// Access policies.
    Policies(Vec<VaultPolicy>, Origin),
}

/// One access policy entry: the permission words (`get`, `list`, ...) per kind of object, lower case.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct VaultPolicy {
    pub object_id: String,
    pub secrets: Vec<String>,
    pub keys: Vec<String>,
    pub certificates: Vec<String>,
    pub storage: Vec<String>,
}

#[derive(Default)]
struct Inner {
    roles: BTreeMap<String, RoleDef>,
    /// key: normalized scope, then the name
    assignments: BTreeMap<(String, String), Assignment>,
    resources: BTreeMap<(String, String), String>,
    cosmos_roles: BTreeMap<(String, String), RoleDef>,
    cosmos_assignments: BTreeMap<(String, String), CosmosAssignment>,
    vaults: BTreeMap<String, VaultAccess>,
}

/// The registry. Cheap to read, never held across an `.await`.
#[derive(Default)]
pub struct Registry {
    inner: parking_lot::RwLock<Inner>,
    /// Raised on every change, for saving.
    generation: std::sync::atomic::AtomicU64,
}

impl Registry {
    pub fn new() -> Registry {
        Registry::default()
    }

    fn bump(&self) {
        self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::SeqCst)
    }

    // -- role definitions ------------------------------------------------------------------------------

    /// A built-in or custom definition by GUID (case-insensitive).
    pub fn role_by_id(&self, id: &str) -> Option<RoleDef> {
        let id = id.trim().to_ascii_lowercase();
        builtin_defs().iter().find(|r| r.id == id).cloned().or_else(|| self.inner.read().roles.get(&id).cloned())
    }

    /// By name, case-insensitive.
    pub fn role_by_name(&self, name: &str) -> Option<RoleDef> {
        builtin_defs()
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(name.trim()))
            .cloned()
            .or_else(|| self.inner.read().roles.values().find(|r| r.name.eq_ignore_ascii_case(name.trim())).cloned())
    }

    /// A GUID, a full definition id (`.../roleDefinitions/{guid}`) or a name.
    pub fn find_role(&self, name_or_id: &str) -> Option<RoleDef> {
        let t = name_or_id.trim();
        let last = t.rsplit('/').next().unwrap_or(t);
        self.role_by_id(last).or_else(|| self.role_by_name(t))
    }

    /// Built-in roles first (by name), then custom ones.
    pub fn roles(&self) -> Vec<RoleDef> {
        let mut v = builtin_defs().clone();
        v.extend(self.inner.read().roles.values().cloned());
        v
    }

    /// Create or replace a custom role. `Ok(true)` when it is new.
    pub fn put_role(&self, mut def: RoleDef) -> Result<(RoleDef, bool), RoleError> {
        def.id = def.id.to_ascii_lowercase();
        if builtin_defs().iter().any(|r| r.id == def.id) {
            return Err(RoleError::ReadOnly);
        }
        let mut g = self.inner.write();
        if g.roles.values().any(|r| r.id != def.id && r.name.eq_ignore_ascii_case(&def.name))
            || builtin_defs().iter().any(|r| r.name.eq_ignore_ascii_case(&def.name))
        {
            return Err(RoleError::NameTaken);
        }
        let now = now_text();
        let new = match g.roles.get(&def.id) {
            Some(old) => {
                def.created_on = old.created_on.clone();
                false
            }
            None => {
                def.created_on = now.clone();
                true
            }
        };
        def.updated_on = now;
        def.builtin = false;
        g.roles.insert(def.id.clone(), def.clone());
        drop(g);
        self.bump();
        Ok((def, new))
    }

    pub fn delete_role(&self, id: &str) -> Result<RoleDef, RoleError> {
        let id = id.trim().to_ascii_lowercase();
        if builtin_defs().iter().any(|r| r.id == id) {
            return Err(RoleError::ReadOnly);
        }
        let mut g = self.inner.write();
        if !g.roles.contains_key(&id) {
            return Err(RoleError::NotFound);
        }
        if g.assignments.values().any(|a| a.role_id == id) {
            return Err(RoleError::InUse);
        }
        let def = g.roles.remove(&id).ok_or(RoleError::NotFound)?;
        drop(g);
        self.bump();
        Ok(def)
    }

    // -- role assignments ------------------------------------------------------------------------------

    /// Create an assignment, or give the same one again (same name, principal, role and scope: `Ok(.., false)`).
    pub fn put_assignment(&self, mut a: Assignment) -> Result<(Assignment, bool), AssignError> {
        a.id = a.id.to_ascii_lowercase();
        a.principal_id = a.principal_id.to_ascii_lowercase();
        let Some(role) = self.role_by_id(&a.role_id) else {
            return Err(AssignError::UnknownRole(a.role_id.clone()));
        };
        a.role_id = role.id;
        let key = (normalize(&a.scope), a.id.clone());
        let mut g = self.inner.write();
        if let Some(old) = g.assignments.get(&key) {
            if old.role_id != a.role_id || old.principal_id != a.principal_id {
                return Err(AssignError::UpdateNotPermitted);
            }
            let mut merged = old.clone();
            merged.condition = a.condition;
            merged.condition_version = a.condition_version;
            merged.description = a.description;
            merged.updated_on = now_text();
            g.assignments.insert(key, merged.clone());
            drop(g);
            self.bump();
            return Ok((merged, false));
        }
        if g.assignments.iter().any(|((s, _), o)| *s == key.0 && o.role_id == a.role_id && o.principal_id == a.principal_id) {
            return Err(AssignError::Exists);
        }
        let now = now_text();
        a.created_on = now.clone();
        a.updated_on = now;
        g.assignments.insert(key, a.clone());
        drop(g);
        self.bump();
        Ok((a, true))
    }

    pub fn get_assignment(&self, scope: &str, id: &str) -> Option<Assignment> {
        self.inner.read().assignments.get(&(normalize(scope), id.to_ascii_lowercase())).cloned()
    }

    /// Every assignment, in scope then name order.
    pub fn assignments(&self) -> Vec<Assignment> {
        self.inner.read().assignments.values().cloned().collect()
    }

    pub fn delete_assignment(&self, scope: &str, id: &str) -> Option<Assignment> {
        let removed = self.inner.write().assignments.remove(&(normalize(scope), id.to_ascii_lowercase()));
        if removed.is_some() {
            self.bump();
        }
        removed
    }

    /// Remove an assignment by name wherever it is.
    pub fn delete_assignment_by_name(&self, id: &str) -> Option<Assignment> {
        let id = id.to_ascii_lowercase();
        let mut g = self.inner.write();
        let key = g.assignments.keys().find(|(_, n)| *n == id).cloned()?;
        let removed = g.assignments.remove(&key);
        drop(g);
        self.bump();
        removed
    }

    // -- the ids Resource Manager gave to data resources ----------------------------------------------------

    /// `rtype` is `Microsoft.Storage/storageAccounts`; `id` the Azure id of the resource.
    pub fn register_resource(&self, rtype: &str, name: &str, id: &str) {
        self.inner.write().resources.insert((rtype.to_ascii_lowercase(), name.to_ascii_lowercase()), id.to_string());
    }

    pub fn unregister_resource(&self, rtype: &str, name: &str) {
        self.inner.write().resources.remove(&(rtype.to_ascii_lowercase(), name.to_ascii_lowercase()));
    }

    /// The id Resource Manager gave the resource itself (not the part below it).
    pub fn resource_id(&self, rtype: &str, name: &str) -> Option<String> {
        self.inner.read().resources.get(&(rtype.to_ascii_lowercase(), name.to_ascii_lowercase())).cloned()
    }

    /// The id of the resource with the part below it, as Azure writes it: Resource Manager's, or the made-up one.
    pub fn display_id(&self, res: &Resource) -> String {
        match self.resource_id(&res.rtype, &res.name) {
            Some(id) if res.sub.is_empty() => id,
            Some(id) => format!("{id}/{}", res.sub),
            None => super::scope::local_id(res),
        }
    }

    // -- the question ----------------------------------------------------------------------------------

    /// May `principal` do what `need` says to `res`?
    pub fn may(&self, principal: &str, need: &Need, res: &Resource) -> bool {
        self.check(principal, need, res).allowed
    }

    /// The decision, with every assignment of the principal that was looked at.
    pub fn check(&self, principal: &str, need: &Need, res: &Resource) -> Decision {
        let principal = principal.to_ascii_lowercase();
        let known = self.resource_id(&res.rtype, &res.name);
        let g = self.inner.read();
        let mut leaves = Vec::new();
        need.leaves(&mut leaves);
        let mut looked = Vec::new();
        // what each applying assignment grants, per leaf (by position)
        let mut granted: Vec<bool> = vec![false; leaves.len()];
        for ((scope, _), a) in g.assignments.iter() {
            if a.principal_id != principal {
                continue;
            }
            let role = builtin_defs().iter().find(|r| r.id == a.role_id).or_else(|| g.roles.get(&a.role_id));
            let Some(role) = role else { continue };
            let applies = covers(scope, res, known.as_deref());
            let mut grants = Vec::new();
            if applies {
                for (i, leaf) in leaves.iter().enumerate() {
                    let ok = match leaf {
                        Need::Data(act) => permitted(&role.data_actions, &role.not_data_actions, act),
                        Need::Control(act) => permitted(&role.actions, &role.not_actions, act),
                        _ => false,
                    };
                    if ok {
                        granted[i] = true;
                        grants.push(match leaf {
                            Need::Data(a) | Need::Control(a) => a.clone(),
                            _ => String::new(),
                        });
                    }
                }
            }
            looked.push(Looked { assignment: a.id.clone(), scope: a.scope.clone(), role: role.name.clone(), applies, grants });
        }
        drop(g);
        let has = |n: &Need| leaves.iter().position(|l| std::ptr::eq(*l, n)).is_some_and(|i| granted[i]);
        let allowed = need.satisfied(&has);
        let resource = self.display_id(res);
        let reason = if allowed {
            "an assignment grants it".to_string()
        } else if looked.is_empty() {
            format!("the principal has no role assignment at all, so none grants {}", need.describe())
        } else if !looked.iter().any(|l| l.applies) {
            format!("the principal has {} role assignment(s), but none applies to {resource}", looked.len())
        } else {
            format!("no role assignment that applies to {resource} grants {}", need.describe())
        };
        Decision { allowed, resource, needs: need.describe(), looked_at: looked, reason }
    }

    // -- Cosmos DB's own roles ------------------------------------------------------------------------------

    pub fn cosmos_role(&self, account: &str, id: &str) -> Option<RoleDef> {
        let id = id.trim().to_ascii_lowercase();
        cosmos_builtin()
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .or_else(|| self.inner.read().cosmos_roles.get(&(account.to_ascii_lowercase(), id)).cloned())
    }

    /// By GUID, by name (`Cosmos DB Built-in Data Reader`, `Reader`, `Contributor`) or by full id.
    pub fn find_cosmos_role(&self, account: &str, name_or_id: &str) -> Option<RoleDef> {
        let t = name_or_id.trim();
        let last = t.rsplit('/').next().unwrap_or(t);
        if let Some(r) = self.cosmos_role(account, last) {
            return Some(r);
        }
        let short = t.to_ascii_lowercase();
        let short = short.trim_start_matches("cosmos db built-in data ").to_string();
        cosmos_builtin()
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(t) || r.name.to_ascii_lowercase().trim_start_matches("cosmos db built-in data ") == short)
            .cloned()
            .or_else(|| {
                self.inner
                    .read()
                    .cosmos_roles
                    .iter()
                    .find(|((a, _), r)| a.eq_ignore_ascii_case(account) && r.name.eq_ignore_ascii_case(t))
                    .map(|(_, r)| r.clone())
            })
    }

    pub fn cosmos_roles(&self, account: &str) -> Vec<RoleDef> {
        let mut v = cosmos_builtin().clone();
        v.extend(self.inner.read().cosmos_roles.iter().filter(|((a, _), _)| a.eq_ignore_ascii_case(account)).map(|(_, r)| r.clone()));
        v
    }

    pub fn put_cosmos_role(&self, account: &str, mut def: RoleDef, origin: Origin) {
        def.id = def.id.to_ascii_lowercase();
        def.builtin = false;
        let _ = origin;
        self.inner.write().cosmos_roles.insert((account.to_ascii_lowercase(), def.id.clone()), def);
        self.bump();
    }

    pub fn delete_cosmos_role(&self, account: &str, id: &str) -> bool {
        let r = self.inner.write().cosmos_roles.remove(&(account.to_ascii_lowercase(), id.to_ascii_lowercase())).is_some();
        if r {
            self.bump();
        }
        r
    }

    pub fn put_cosmos_assignment(&self, account: &str, mut a: CosmosAssignment) {
        a.id = a.id.to_ascii_lowercase();
        a.principal_id = a.principal_id.to_ascii_lowercase();
        a.role_id = a.role_id.rsplit('/').next().unwrap_or("").to_ascii_lowercase();
        self.inner.write().cosmos_assignments.insert((account.to_ascii_lowercase(), a.id.clone()), a);
        self.bump();
    }

    pub fn delete_cosmos_assignment(&self, account: &str, id: &str) -> Option<CosmosAssignment> {
        let r = self.inner.write().cosmos_assignments.remove(&(account.to_ascii_lowercase(), id.to_ascii_lowercase()));
        if r.is_some() {
            self.bump();
        }
        r
    }

    pub fn cosmos_assignments(&self, account: &str) -> Vec<CosmosAssignment> {
        self.inner.read().cosmos_assignments.iter().filter(|((a, _), _)| a.eq_ignore_ascii_case(account)).map(|(_, v)| v.clone()).collect()
    }

    /// Forget the Cosmos roles and assignments Resource Manager stores, for one account (it was deleted).
    pub fn drop_cosmos_account(&self, account: &str, origin: Origin) {
        let acct = account.to_ascii_lowercase();
        let mut g = self.inner.write();
        g.cosmos_assignments.retain(|(a, _), v| !(*a == acct && v.origin == origin));
        if origin == Origin::ArmStored {
            g.cosmos_roles.retain(|(a, _), _| *a != acct);
        }
        drop(g);
        self.bump();
    }

    /// May the principal do `action` (a Cosmos data action such as `Microsoft.DocumentDB/databaseAccounts/readMetadata`) on `path`
    /// (`/`, `/dbs/{db}`, `/dbs/{db}/colls/{c}`) of the account?
    pub fn cosmos_check(&self, account: &str, principal: &str, action: &str, path: &str) -> Decision {
        let principal = principal.to_ascii_lowercase();
        let acct = account.to_ascii_lowercase();
        // an account is a host name; a database and a container are names, and `/dbs/App` is not `/dbs/app` (the path keeps its case)
        let path = cosmos_normalize(path);
        let g = self.inner.read();
        let mut looked = Vec::new();
        let mut allowed = false;
        for ((a, _), asg) in g.cosmos_assignments.iter() {
            if *a != acct || asg.principal_id != principal {
                continue;
            }
            let role =
                cosmos_builtin().iter().find(|r| r.id == asg.role_id).or_else(|| g.cosmos_roles.get(&(acct.clone(), asg.role_id.clone())));
            let Some(role) = role else { continue };
            let scope = cosmos_path_of_scope(&asg.scope);
            let applies = super::scope::is_within(&scope, &path);
            let grants = applies && permitted(&role.data_actions, &role.not_data_actions, action);
            allowed |= grants;
            looked.push(Looked {
                assignment: asg.id.clone(),
                scope: asg.scope.clone(),
                role: role.name.clone(),
                applies,
                grants: if grants { vec![action.to_string()] } else { vec![] },
            });
        }
        let reason = if allowed {
            "a Cosmos DB role assignment grants it".to_string()
        } else if looked.is_empty() {
            format!("the principal has no Cosmos DB role assignment on account {account}")
        } else {
            format!("no Cosmos DB role assignment of the principal that applies to {path} grants {action}")
        };
        Decision { allowed, resource: format!("{account}{path}"), needs: action.to_string(), looked_at: looked, reason }
    }

    // -- Key Vault access ------------------------------------------------------------------------------

    pub fn vault_access(&self, vault: &str) -> VaultAccess {
        self.inner.read().vaults.get(&vault.to_ascii_lowercase()).cloned().unwrap_or(VaultAccess::Rbac)
    }

    pub fn set_vault_access(&self, vault: &str, access: VaultAccess) {
        self.inner.write().vaults.insert(vault.to_ascii_lowercase(), access);
        self.bump();
    }

    pub fn clear_vault_access(&self, vault: &str) {
        self.inner.write().vaults.remove(&vault.to_ascii_lowercase());
        self.bump();
    }

    // -- state ---------------------------------------------------------------------------------------------

    /// Forget everything: assignments, custom roles, resource ids, vault access.
    pub fn clear(&self) {
        *self.inner.write() = Inner::default();
        self.bump();
    }

    /// Resource Manager's state is gone: forget what it put here (resource ids, vault access, Cosmos DB roles and assignments it stores).
    pub fn clear_arm_stored(&self) {
        let mut g = self.inner.write();
        g.resources.clear();
        g.cosmos_roles.clear();
        g.cosmos_assignments.retain(|_, a| a.origin != Origin::ArmStored);
        g.vaults.retain(|_, v| !matches!(v, VaultAccess::Policies(_, Origin::ArmStored)) && !matches!(v, VaultAccess::Rbac));
        drop(g);
        self.bump();
    }

    pub(crate) fn snapshot(&self) -> Saved {
        let g = self.inner.read();
        Saved {
            roles: g.roles.values().cloned().collect(),
            assignments: g.assignments.values().filter(|a| a.origin.saved()).cloned().collect(),
            cosmos_roles: Vec::new(),
            cosmos_assignments: g
                .cosmos_assignments
                .iter()
                .filter(|(_, a)| a.origin.saved())
                .map(|((acct, _), a)| (acct.clone(), a.clone()))
                .collect(),
            vaults: g
                .vaults
                .iter()
                .filter(|(_, v)| matches!(v, VaultAccess::Policies(_, o) if o.saved()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }

    pub(crate) fn restore(&self, s: Saved) {
        let mut g = self.inner.write();
        for r in s.roles {
            g.roles.insert(r.id.clone(), r);
        }
        for a in s.assignments {
            g.assignments.insert((normalize(&a.scope), a.id.clone()), a);
        }
        for (acct, r) in s.cosmos_roles {
            g.cosmos_roles.insert((acct, r.id.clone()), r);
        }
        for (acct, a) in s.cosmos_assignments {
            g.cosmos_assignments.insert((acct, a.id.clone()), a);
        }
        for (k, v) in s.vaults {
            g.vaults.insert(k, v);
        }
        drop(g);
        self.bump();
    }
}

/// The two definitions every Cosmos DB account has (`Cosmos DB Built-in Data Reader` and `... Contributor`), as the Cosmos DB documentation
/// lists them ("Configure role-based access control with Microsoft Entra ID for your Azure Cosmos DB account").
pub fn cosmos_builtin() -> &'static Vec<RoleDef> {
    static DEFS: std::sync::OnceLock<Vec<RoleDef>> = std::sync::OnceLock::new();
    DEFS.get_or_init(|| {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let mk = |id: &str, name: &str, da: &[&str]| RoleDef {
            id: id.to_string(),
            name: name.to_string(),
            description: String::new(),
            builtin: true,
            actions: vec![],
            not_actions: vec![],
            data_actions: v(da),
            not_data_actions: vec![],
            assignable_scopes: vec![],
            created_on: String::new(),
            updated_on: String::new(),
        };
        vec![
            mk(
                "00000000-0000-0000-0000-000000000001",
                "Cosmos DB Built-in Data Reader",
                &[
                    "Microsoft.DocumentDB/databaseAccounts/readMetadata",
                    "Microsoft.DocumentDB/databaseAccounts/sqlDatabases/containers/executeQuery",
                    "Microsoft.DocumentDB/databaseAccounts/sqlDatabases/containers/readChangeFeed",
                    "Microsoft.DocumentDB/databaseAccounts/sqlDatabases/containers/items/read",
                ],
            ),
            mk(
                "00000000-0000-0000-0000-000000000002",
                "Cosmos DB Built-in Data Contributor",
                &[
                    "Microsoft.DocumentDB/databaseAccounts/readMetadata",
                    "Microsoft.DocumentDB/databaseAccounts/sqlDatabases/containers/*",
                    "Microsoft.DocumentDB/databaseAccounts/sqlDatabases/containers/items/*",
                ],
            ),
        ]
    })
}

/// A Cosmos DB path as it is compared: no trailing slash, `/` for the root, and the case it has (names in Cosmos DB are case-sensitive).
fn cosmos_normalize(path: &str) -> String {
    let t = path.trim().trim_end_matches('/');
    if t.is_empty() {
        "/".to_string()
    } else if t.starts_with('/') {
        t.to_string()
    } else {
        format!("/{t}")
    }
}

/// `/`, `/dbs/{db}` or `/dbs/{db}/colls/{c}` out of a scope that may be the account's whole Azure id with that path after it. Only the part that
/// finds `/dbs/` is read without regard to case; the names after it keep theirs.
pub fn cosmos_path_of_scope(scope: &str) -> String {
    let scope = scope.trim();
    // a lower case copy has the same length (ASCII only), so a position in it is a position in the scope
    match scope.to_ascii_lowercase().find("/dbs/") {
        Some(i) => scope[i..].trim_end_matches('/').to_string(),
        None => "/".to_string(),
    }
}

#[cfg(test)]
#[path = "roles_tests.rs"]
mod tests;
