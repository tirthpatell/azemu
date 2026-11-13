//! Microsoft.KeyVault/vaults. A vault is created in the Key Vault emulator on first use,
//! so creating one only records it; deleting it removes everything it holds.

use azemu_core::{async_trait, Response};
use serde_json::{json, Value};

use crate::backends::Backends;
use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::provider::{ActionTarget, Provider, Statuses};
use crate::state::{key, Stored};
use crate::util;

pub struct KeyVault {
    pub b: Backends,
}

fn valid_vault_name(n: &str) -> bool {
    let b = n.as_bytes();
    (3..=24).contains(&b.len())
        && b[0].is_ascii_alphabetic()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !n.contains("--")
}

#[async_trait]
impl Provider for KeyVault {
    fn namespace(&self) -> &'static str {
        "Microsoft.KeyVault"
    }

    fn types(&self) -> &'static [&'static str] {
        &["vaults", "vaults/accessPolicies"]
    }

    fn statuses(&self, type_path: &str) -> Statuses {
        if type_path == "vaults" {
            Statuses { create: 200, update: 200, delete: 200, delete_missing: 204 }
        } else {
            Statuses { create: 200, update: 200, delete: 200, delete_missing: 200 }
        }
    }

    fn global_name(&self, type_path: &str) -> bool {
        type_path == "vaults"
    }

    fn requires_location(&self, type_path: &str) -> bool {
        type_path == "vaults"
    }

    fn validate_name(&self, type_path: &str, name: &str) -> ArmResult<()> {
        if type_path == "vaults" && !valid_vault_name(name) {
            return Err(ArmError::bad_request(
                "VaultNameNotValid",
                format!("The vault name '{name}' is invalid. Vault names must be 3-24 characters long, begin with a letter, end with a letter or digit, and contain only alphanumeric characters or hyphens (no two in a row)."),
            ));
        }
        Ok(())
    }

    async fn put(&self, cx: &Cx<'_>, res: &mut Stored, _existed: bool, _children: &mut Vec<Stored>) -> ArmResult<()> {
        if res.type_path() == "vaults/accessPolicies" {
            // PUT .../accessPolicies/{add|replace|remove}: change the policies of the vault.
            let op = res.name().to_ascii_lowercase();
            if !matches!(op.as_str(), "add" | "replace" | "remove") {
                return Err(ArmError::bad_request("InvalidOperationKind", "The access policy operation must be add, replace or remove."));
            }
            let given =
                res.body.get("properties").and_then(|p| p.get("accessPolicies")).and_then(Value::as_array).cloned().unwrap_or_default();
            let vault_key = key(&crate::ids::resource_id(&res.sub, &res.rg, &res.ns, &res.types[..1], &res.names[..1]));
            let mut st = cx.inner.state.lock();
            let Some(v) = st.resources.get_mut(&vault_key) else {
                return Err(ArmError::not_found("ResourceNotFound", "The vault does not exist."));
            };
            let current =
                util::props_mut(&mut v.body).entry("accessPolicies").or_insert_with(|| json!([])).as_array().cloned().unwrap_or_default();
            let same = |a: &Value, b: &Value| a.get("objectId") == b.get("objectId") && a.get("applicationId") == b.get("applicationId");
            let mut next = current.clone();
            match op.as_str() {
                "add" => {
                    for g in &given {
                        next.retain(|c| !same(c, g));
                        next.push(g.clone());
                    }
                }
                "replace" => next = given.clone(),
                _ => {
                    for g in &given {
                        next.retain(|c| !same(c, g));
                    }
                }
            }
            util::props_mut(&mut v.body).insert("accessPolicies".into(), json!(next));
            let p = util::props_mut(&mut res.body);
            p.insert("accessPolicies".into(), json!(next));
            return Ok(());
        }
        let tenant = crate::basics::default_tenant();
        let p = util::props_mut(&mut res.body);
        util::set_default(p, "tenantId", json!(tenant));
        util::set_default(p, "accessPolicies", json!([]));
        util::set_default(p, "enabledForDeployment", json!(false));
        util::set_default(p, "enabledForDiskEncryption", json!(false));
        util::set_default(p, "enabledForTemplateDeployment", json!(false));
        util::set_default(p, "enableSoftDelete", json!(true));
        util::set_default(p, "softDeleteRetentionInDays", json!(90));
        util::set_default(p, "enableRbacAuthorization", json!(false));
        util::set_default(p, "publicNetworkAccess", json!("Enabled"));
        util::set_default(
            p,
            "networkAcls",
            json!({"bypass": "AzureServices", "defaultAction": "Allow", "ipRules": [], "virtualNetworkRules": []}),
        );
        let sku = p.get("sku").cloned();
        if sku.is_none() {
            p.insert("sku".into(), json!({"family": "A", "name": "standard"}));
        }
        Ok(())
    }

    async fn delete(&self, _cx: &Cx<'_>, res: &Stored, _force: bool) -> ArmResult<()> {
        if res.type_path() == "vaults" {
            if let Some(kv) = &self.b.keyvault {
                kv.delete_vault(res.name());
            }
        }
        Ok(())
    }

    fn render(&self, cx: &Cx<'_>, res: &Stored) -> Value {
        let mut v = res.body.clone();
        if res.type_path() == "vaults" {
            util::props_mut(&mut v).insert("vaultUri".into(), json!(format!("{}/", cx.data_url("https", res.name(), "vault"))));
        }
        v
    }

    async fn action(
        &self,
        cx: &Cx<'_>,
        target: ActionTarget<'_>,
        type_path: &str,
        action: &str,
        body: &Value,
    ) -> ArmResult<Option<Response>> {
        // A deleted vault is gone for good (no soft delete), so purging one has nothing to do.
        if type_path.eq_ignore_ascii_case("locations/deletedVaults") && action.eq_ignore_ascii_case("purge") {
            return Ok(Some(cx.accepted()));
        }
        if let ActionTarget::Provider = target {
            if action.eq_ignore_ascii_case("checkNameAvailability") {
                let name = body.get("name").and_then(Value::as_str).unwrap_or("");
                let v = if !valid_vault_name(name) {
                    util::name_availability(false, "AccountNameInvalid", &format!("The vault name '{name}' is invalid."))
                } else if cx
                    .inner
                    .state
                    .lock()
                    .resources
                    .values()
                    .any(|r| r.ns == "Microsoft.KeyVault" && r.types.len() == 1 && r.name().eq_ignore_ascii_case(name))
                {
                    util::name_availability(false, "AlreadyExists", &format!("The vault name '{name}' is already in use."))
                } else {
                    util::name_availability(true, "", "")
                };
                return Ok(Some(util::json_ok(200, &v)));
            }
        }
        Ok(None)
    }
}
