//! Microsoft.AppConfiguration/configurationStores. A store is created in the App
//! Configuration emulator on first use; deleting one removes its settings.

use azemu_core::{async_trait, Response};
use serde_json::{json, Value};

use super::{bump_generation, generation};
use crate::backends::Backends;
use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::provider::{ActionTarget, Provider, Statuses};
use crate::state::Stored;
use crate::util;

pub struct AppConfig {
    pub b: Backends,
}

/// Microsoft Learn, "Naming rules and restrictions": 5 to 50 letters, digits and hyphens; no more than two hyphens in a row; not
/// starting or ending with a hyphen.
fn valid_store_name(n: &str) -> bool {
    (5..=50).contains(&n.len())
        && !n.starts_with('-')
        && !n.ends_with('-')
        && !n.contains("---")
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

const KEYS: &[(&str, &str, bool)] = &[
    ("Primary", "primary", false),
    ("Secondary", "secondary", false),
    ("Primary Read Only", "primaryro", true),
    ("Secondary Read Only", "secondaryro", true),
];

impl AppConfig {
    fn key_json(&self, cx: &Cx<'_>, res: &Stored, label: &str, tag: &str, read_only: bool) -> Value {
        let g = generation(res, tag);
        let seed = format!("appconfig/{}/{tag}", res.id.to_ascii_lowercase());
        let secret = util::derive_key(&seed, g);
        let id = format!("{}-l0-s0:{}", util::short_hash(&format!("{seed}/{g}/id"), 4), util::short_hash(&format!("{seed}/{g}/id2"), 20));
        let endpoint = cx.data_url("https", res.name(), "azconfig");
        json!({
            "id": id,
            "name": label,
            "value": secret,
            "connectionString": format!("Endpoint={endpoint};Id={id};Secret={secret}"),
            "lastModified": azemu_core::time::iso8601_7(res.created),
            "readOnly": read_only,
        })
    }

    fn keys_response(&self, cx: &Cx<'_>, res: &Stored) -> Response {
        let items: Vec<Value> = KEYS.iter().map(|(label, tag, ro)| self.key_json(cx, res, label, tag, *ro)).collect();
        util::json_ok(200, &json!({ "value": items }))
    }
}

#[async_trait]
impl Provider for AppConfig {
    fn namespace(&self) -> &'static str {
        "Microsoft.AppConfiguration"
    }

    fn types(&self) -> &'static [&'static str] {
        &["configurationStores"]
    }

    fn statuses(&self, _type_path: &str) -> Statuses {
        Statuses { create: 201, update: 200, delete: 200, delete_missing: 204 }
    }

    fn validate_name(&self, _type_path: &str, name: &str) -> ArmResult<()> {
        if !valid_store_name(name) {
            return Err(ArmError::bad_request(
                "InvalidResourceName",
                format!("The configuration store name '{name}' is invalid. Use 5 to 50 letters, numbers and hyphens (no more than two in a row, none at the start or end)."),
            ));
        }
        Ok(())
    }

    async fn put(&self, cx: &Cx<'_>, res: &mut Stored, existed: bool, _children: &mut Vec<Stored>) -> ArmResult<()> {
        let sku = res.body.get("sku").and_then(|s| s.get("name")).and_then(Value::as_str).unwrap_or("free").to_string();
        res.body["sku"] = json!({"name": sku});
        let now = cx.now_iso();
        let standard = !sku.eq_ignore_ascii_case("free");
        let p = util::props_mut(&mut res.body);
        if !existed {
            p.insert("creationDate".into(), json!(now));
        }
        // Azure leaves `encryption` out unless a customer key is set; an empty object from the
        // client would show up as a block Terraform wants to remove.
        if p.get("encryption")
            .and_then(Value::as_object)
            .map(|e| {
                e.is_empty() || e.get("keyVaultProperties").map(|k| k.as_object().map(|o| o.is_empty()).unwrap_or(true)).unwrap_or(true)
            })
            .unwrap_or(false)
        {
            p.remove("encryption");
        }
        util::set_default(p, "privateEndpointConnections", json!([]));
        util::set_default(p, "disableLocalAuth", json!(false));
        util::set_default(p, "softDeleteRetentionInDays", json!(if standard { 7 } else { 0 }));
        util::set_default(p, "enablePurgeProtection", json!(false));
        Ok(())
    }

    async fn delete(&self, _cx: &Cx<'_>, res: &Stored, _force: bool) -> ArmResult<()> {
        if let Some(a) = &self.b.appconfig {
            a.delete_store(res.name());
        }
        Ok(())
    }

    fn render(&self, cx: &Cx<'_>, res: &Stored) -> Value {
        let mut v = res.body.clone();
        util::props_mut(&mut v).insert("endpoint".into(), json!(cx.data_url("https", res.name(), "azconfig")));
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
        let a = action.to_ascii_lowercase();
        // A deleted store is gone for good (no soft delete), so purging one has nothing to do.
        if type_path.eq_ignore_ascii_case("locations/deletedConfigurationStores") && a == "purge" {
            return Ok(Some(cx.accepted()));
        }
        match target {
            ActionTarget::Provider if a == "checknameavailability" => {
                let name = body.get("name").and_then(Value::as_str).unwrap_or("");
                let v = if !valid_store_name(name) {
                    util::name_availability(false, "Invalid", &format!("The name '{name}' is invalid."))
                } else if cx
                    .inner
                    .state
                    .lock()
                    .resources
                    .values()
                    .any(|r| r.ns == "Microsoft.AppConfiguration" && r.name().eq_ignore_ascii_case(name))
                {
                    util::name_availability(false, "AlreadyExists", &format!("The name '{name}' is already in use."))
                } else {
                    util::name_availability(true, "", "")
                };
                Ok(Some(util::json_ok(200, &v)))
            }
            ActionTarget::Resource(res) if a == "listkeys" => Ok(Some(self.keys_response(cx, res))),
            ActionTarget::Resource(res) if a == "regeneratekey" => {
                let id = body.get("id").and_then(Value::as_str).unwrap_or("");
                // Find which key has this id (it changes with the generation).
                for (label, tag, ro) in KEYS {
                    let j = self.key_json(cx, res, label, tag, *ro);
                    if j["id"].as_str() == Some(id) {
                        bump_generation(cx, &res.id, tag);
                        let fresh =
                            cx.inner.state.lock().resources.get(&crate::state::key(&res.id)).cloned().unwrap_or_else(|| res.clone());
                        return Ok(Some(util::json_ok(200, &self.key_json(cx, &fresh, label, tag, *ro))));
                    }
                }
                Err(ArmError::bad_request("InvalidKeyId", format!("The key id '{id}' does not belong to this store.")))
            }
            _ => Ok(None),
        }
    }
}
