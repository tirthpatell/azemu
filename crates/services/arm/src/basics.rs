//! Subscriptions, tenants, locations and the provider list.

use azemu_core::Response;
use serde_json::{json, Value};

use crate::catalog;
use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::util;

/// The tenant tokens from the identity service carry (same derivation as azemu-identity).
pub fn default_tenant() -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"tenant");
    h.update([0u8]);
    h.update(b"azemu-default");
    let d = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(b).to_string()
}

pub const DEFAULT_SUBSCRIPTION: &str = "00000000-0000-0000-0000-000000000000";

fn subscription_json(sub: &str) -> Value {
    json!({
        "id": format!("/subscriptions/{sub}"),
        "authorizationSource": "RoleBased",
        "managedByTenants": [],
        "subscriptionId": sub,
        "tenantId": default_tenant(),
        "displayName": if sub == DEFAULT_SUBSCRIPTION { "azemu subscription".to_string() } else { format!("azemu subscription {sub}") },
        "state": "Enabled",
        "subscriptionPolicies": {
            "locationPlacementId": "Internal_2014-09-01",
            "quotaId": "Internal_2014-09-01",
            "spendingLimit": "Off"
        }
    })
}

pub fn list_subscriptions(cx: &Cx<'_>) -> ArmResult<Response> {
    let mut subs = vec![DEFAULT_SUBSCRIPTION.to_string()];
    for s in cx.inner.state.lock().subs.iter() {
        if !subs.iter().any(|x| x.eq_ignore_ascii_case(s)) {
            subs.push(s.clone());
        }
    }
    Ok(util::list(subs.iter().map(|s| subscription_json(s)).collect()))
}

pub fn get_subscription(sub: &str) -> ArmResult<Response> {
    Ok(util::json_ok(200, &subscription_json(sub)))
}

pub fn list_tenants() -> ArmResult<Response> {
    let t = default_tenant();
    Ok(util::list(vec![json!({
        "id": format!("/tenants/{t}"),
        "tenantId": t,
        "countryCode": "US",
        "displayName": "azemu",
        "domains": ["azemu.onmicrosoft.com"],
        "tenantCategory": "Home",
        "defaultDomain": "azemu.onmicrosoft.com",
        "tenantType": "AAD"
    })]))
}

pub fn list_locations(sub: &str) -> ArmResult<Response> {
    Ok(util::list(catalog::locations(sub)))
}

pub fn list_providers(sub: Option<&str>) -> ArmResult<Response> {
    Ok(util::list(catalog::namespaces().iter().filter_map(|ns| catalog::provider_json(sub, ns)).collect()))
}

pub fn get_provider(sub: Option<&str>, ns_raw: &str, _lc: &str) -> ArmResult<Response> {
    match catalog::provider_json(sub, ns_raw) {
        Some(v) => Ok(util::json_ok(200, &v)),
        None => Err(ArmError::bad_request("InvalidResourceNamespace", format!("The resource namespace '{ns_raw}' is invalid."))),
    }
}
