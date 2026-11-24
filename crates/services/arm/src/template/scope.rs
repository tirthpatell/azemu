//! Resource id and scope functions that need nothing from the stored resources: `extensionResourceId`,
//! `tenantResourceId`, `managementGroupResourceId`, `pickZones`, `deployer`, `managementGroup`.
//! Source: Microsoft Learn, `azure-resource-manager/templates/template-functions-resource` and
//! `template-functions-deployment`, `template-functions-scope`.

use serde_json::{json, Value};

use super::{err, s, Env, EvalError};

type R = Result<Value, EvalError>;

/// `Microsoft.Authorization/locks`, `a`, `b` -> `/providers/Microsoft.Authorization/locks/a/b`; the number of names must
/// match the number of type segments, as it does for `resourceId`.
fn tail(fname: &str, ty: &str, names: &[String]) -> Result<String, EvalError> {
    let (ns, types) = ty
        .split_once('/')
        .filter(|(n, t)| n.contains('.') && !t.is_empty())
        .ok_or_else(|| EvalError::Msg(format!("{fname}: '{ty}' is not a resource type such as 'Microsoft.Authorization/locks'.")))?;
    let segs: Vec<&str> = types.split('/').collect();
    if names.len() != segs.len() {
        return err(format!("{fname}: the resource type '{ty}' needs {} name segment(s) but {} were given.", segs.len(), names.len()));
    }
    let mut out = format!("/providers/{ns}");
    for (t, n) in segs.iter().zip(names) {
        out.push_str(&format!("/{t}/{n}"));
    }
    Ok(out)
}

pub fn call(name: &str, lname: &str, v: &[Value], env: &Env<'_>) -> Option<R> {
    let strs = || -> Vec<String> { v.iter().map(s).collect() };
    Some(match lname {
        "extensionresourceid" => {
            if v.len() < 3 {
                return Some(err(format!("The function '{name}' needs at least 3 argument(s).")));
            }
            let a = strs();
            let base = a[0].trim_end_matches('/');
            if !base.starts_with('/') {
                return Some(err(format!("{name}: the first argument must be the full resource id of the resource that is extended.")));
            }
            tail(name, &a[1], &a[2..]).map(|t| Value::String(format!("{base}{t}")))
        }
        "tenantresourceid" => {
            if v.len() < 2 {
                return Some(err(format!("The function '{name}' needs at least 2 argument(s).")));
            }
            let a = strs();
            tail(name, &a[0], &a[1..]).map(Value::String)
        }
        "managementgroupresourceid" => {
            if v.len() < 3 {
                return Some(err(format!(
                    "{name}: needs a management group id, a resource type and a name; the emulator has no current management group to take the id from."
                )));
            }
            let a = strs();
            tail(name, &a[1], &a[2..]).map(|t| Value::String(format!("/providers/Microsoft.Management/managementGroups/{}{t}", a[0])))
        }
        // The emulator has no availability zone data: every resource type and region answers "no zones", which is the
        // documented answer for a type or region without zone support (template-functions-resource#pickzones).
        "pickzones" => {
            if v.len() < 3 {
                return Some(err(format!("The function '{name}' needs at least 3 argument(s).")));
            }
            let n = v.get(3).and_then(Value::as_i64).unwrap_or(1);
            if !(1..=3).contains(&n) {
                return Some(err("pickZones: numberOfZones must be from 1 to 3."));
            }
            Ok(json!([]))
        }
        "deployer" => Ok(json!({"objectId": "00000000-0000-0000-0000-000000000000", "tenantId": env.tenant, "userPrincipalName": "azemu@localhost"})),
        "managementgroup" => err("The template function 'managementGroup' can only be used in a management group deployment; the emulator deploys to resource groups and subscriptions."),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_and_tenant_ids() {
        assert_eq!(
            tail("f", "Microsoft.Authorization/locks", &["l".into()]).ok(),
            Some("/providers/Microsoft.Authorization/locks/l".to_string())
        );
        assert!(tail("f", "Microsoft.Authorization/locks", &[]).is_err());
        assert!(tail("f", "locks", &["a".into()]).is_err());
    }
}
