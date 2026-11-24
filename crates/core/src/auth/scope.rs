//! What a request is about (a [`Resource`]) and where a role assignment applies (a scope).
//!
//! A scope is an Azure Resource Manager id: `/`, `/subscriptions/{s}`, `/subscriptions/{s}/resourceGroups/{g}`, or a resource
//! below those. An assignment applies to its scope and everything below it. A data resource is named by its type and name
//! (`Microsoft.Storage/storageAccounts` + the account) and a path below it (`blobServices/default/containers/c`).
//!
//! A resource made through Resource Manager has an id (`Registry::register_resource`), and a scope covers it as Azure does. A
//! resource that was not (the emulator makes accounts on first use) has none: only an assignment whose scope **names** it covers
//! it, never a wider one (a resource group or a subscription).

/// The subscription and resource group of the made-up id that the short scope forms (`storage/acct/containers/c`) are written as for
/// a resource Resource Manager does not know.
pub const LOCAL_SUBSCRIPTION: &str = "00000000-0000-0000-0000-000000000000";
pub const LOCAL_GROUP: &str = "azemu-local";

/// A data resource a request is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    /// `Microsoft.Storage/storageAccounts`, `Microsoft.KeyVault/vaults`, ...
    pub rtype: String,
    /// The account, vault, store, namespace or topic name.
    pub name: String,
    /// The part below it, without a leading slash: `blobServices/default/containers/c`, `secrets/s`. Empty for the resource itself.
    pub sub: String,
}

impl Resource {
    pub fn new(rtype: &str, name: &str) -> Resource {
        Resource { rtype: rtype.to_string(), name: name.to_ascii_lowercase(), sub: String::new() }
    }

    /// The same resource, narrowed to a part of it.
    pub fn at(mut self, sub: impl Into<String>) -> Resource {
        self.sub = sub.into();
        self
    }

    /// Storage account.
    pub fn storage(account: &str) -> Resource {
        Resource::new("Microsoft.Storage/storageAccounts", account)
    }
    /// A blob container (a Data Lake file system is the same thing).
    pub fn container(account: &str, container: &str) -> Resource {
        Resource::storage(account).at(format!("blobServices/default/containers/{container}"))
    }
    pub fn queue(account: &str, queue: &str) -> Resource {
        Resource::storage(account).at(format!("queueServices/default/queues/{queue}"))
    }
    pub fn table(account: &str, table: &str) -> Resource {
        Resource::storage(account).at(format!("tableServices/default/tables/{table}"))
    }
    pub fn vault(vault: &str) -> Resource {
        Resource::new("Microsoft.KeyVault/vaults", vault)
    }
    pub fn config_store(store: &str) -> Resource {
        Resource::new("Microsoft.AppConfiguration/configurationStores", store)
    }
    pub fn event_grid_topic(topic: &str) -> Resource {
        Resource::new("Microsoft.EventGrid/topics", topic)
    }
    pub fn service_bus(namespace: &str) -> Resource {
        Resource::new("Microsoft.ServiceBus/namespaces", namespace)
    }
    pub fn event_hubs(namespace: &str) -> Resource {
        Resource::new("Microsoft.EventHub/namespaces", namespace)
    }
}

/// An id as it is compared: lower case, no trailing slash, `/` for the root.
pub fn normalize(id: &str) -> String {
    let t = id.trim().trim_end_matches('/').to_ascii_lowercase();
    if t.is_empty() {
        "/".to_string()
    } else if t.starts_with('/') {
        t
    } else {
        format!("/{t}")
    }
}

/// Is `inner` the same scope as `outer` or below it? Both normalized.
pub fn is_within(outer: &str, inner: &str) -> bool {
    outer == "/" || inner == outer || (inner.starts_with(outer) && inner.as_bytes().get(outer.len()) == Some(&b'/'))
}

/// The id a resource has when Resource Manager does not know it, written the way Azure writes an id.
pub fn local_id(res: &Resource) -> String {
    let (ns, ty) = res.rtype.split_once('/').unwrap_or((res.rtype.as_str(), ""));
    let mut id = format!("/subscriptions/{LOCAL_SUBSCRIPTION}/resourceGroups/{LOCAL_GROUP}/providers/{ns}/{ty}/{}", res.name);
    if !res.sub.is_empty() {
        id.push('/');
        id.push_str(&res.sub);
    }
    id
}

/// Is this scope one of the made-up ids of [`local_id`]?
pub fn is_local(scope: &str) -> bool {
    let lower = scope.to_ascii_lowercase();
    lower.starts_with(&format!("/subscriptions/{LOCAL_SUBSCRIPTION}/resourcegroups/{LOCAL_GROUP}/providers/"))
}

/// Where `scope` names the resource (`.../providers/{ns}/{type}/{name}` with this resource's type and name): the part of the scope
/// after the name, without slashes (`""` for the resource itself).
fn naming_remainder(scope: &str, res: &Resource) -> Option<String> {
    let (ns, ty) = res.rtype.split_once('/')?;
    let needle = format!("/providers/{}/{}/{}", ns.to_ascii_lowercase(), ty.to_ascii_lowercase(), res.name);
    let at = scope.find(&needle)?;
    let after = &scope[at + needle.len()..];
    if after.is_empty() {
        Some(String::new())
    } else {
        // `.../storageAccounts/acct2` is another name that starts the same: no slash, no match
        after.strip_prefix('/').map(str::to_string)
    }
}

/// Does an assignment at `scope` (normalized) apply to the resource? `known_id` is the id Resource Manager gave the resource, when it did.
pub fn covers(scope: &str, res: &Resource, known_id: Option<&str>) -> bool {
    let sub = res.sub.to_ascii_lowercase();
    if let Some(id) = known_id {
        let full = if sub.is_empty() { normalize(id) } else { format!("{}/{sub}", normalize(id)) };
        if is_within(scope, &full) {
            return true;
        }
    }
    // An assignment that names the resource covers it when Resource Manager does not know the resource, and when the scope is one of
    // the made-up ids of the short forms. A resource group or subscription that was written by hand never does for an unknown one.
    if known_id.is_none() || is_local(scope) {
        if let Some(rest) = naming_remainder(scope, res) {
            return rest.is_empty() || sub == rest || sub.starts_with(&format!("{rest}/"));
        }
    }
    false
}

/// The resource an Azure id names: the last `/providers/{namespace}/{type}/{name}` in it, and what follows. `None` when the id has no provider part.
pub fn resource_of_id(id: &str) -> Option<Resource> {
    let segs: Vec<&str> = id.split('/').filter(|s| !s.is_empty()).collect();
    let p = segs.iter().rposition(|s| s.eq_ignore_ascii_case("providers"))?;
    // a data resource is a top-level type: namespace, type, name
    let (ns, ty, name) = (segs.get(p + 1)?, segs.get(p + 2)?, segs.get(p + 3)?);
    let mut res = Resource::new(&format!("{ns}/{ty}"), name);
    let rest = segs[p + 4..].join("/");
    if !rest.is_empty() {
        res = res.at(rest);
    }
    Some(res)
}

/// Turn the short form of a scope into an id. `service/name[/part...]`:
///
/// | short form | resource |
/// |---|---|
/// | `storage/{account}`, `storage/{account}/containers/{c}`, `/queues/{q}`, `/tables/{t}` | storage account, or one container, queue or table |
/// | `keyvault/{vault}`, `keyvault/{vault}/secrets/{n}`, `/keys/{n}`, `/certificates/{n}` | key vault, or one secret, key or certificate |
/// | `appconfig/{store}` | App Configuration store |
/// | `eventgrid/{topic}` | Event Grid topic |
/// | `servicebus/{namespace}`, `/queues/{q}`, `/topics/{t}` | Service Bus namespace, queue or topic |
/// | `eventhubs/{namespace}`, `/eventhubs/{hub}` | Event Hubs namespace or one event hub |
///
/// `Err` is the text of the mistake.
pub fn parse_short(text: &str) -> Result<Resource, String> {
    let parts: Vec<&str> = text.trim().trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let usage = "a short scope is `<service>/<name>` or `<service>/<name>/<kind>/<part>`, for example storage/myaccount/containers/data (services: storage, keyvault, appconfig, eventgrid, servicebus, eventhubs)";
    let (Some(service), Some(name)) = (parts.first(), parts.get(1)) else {
        return Err(format!("'{text}' is not a scope: {usage}"));
    };
    let rest = &parts[2..];
    let (rtype, kinds): (&str, &[(&str, &str)]) = match service.to_ascii_lowercase().as_str() {
        "storage" | "blob" | "queue" | "table" | "datalake" => (
            "Microsoft.Storage/storageAccounts",
            &[
                ("containers", "blobServices/default/containers"),
                ("filesystems", "blobServices/default/containers"),
                ("queues", "queueServices/default/queues"),
                ("tables", "tableServices/default/tables"),
            ],
        ),
        "keyvault" => ("Microsoft.KeyVault/vaults", &[("secrets", "secrets"), ("keys", "keys"), ("certificates", "certificates")]),
        "appconfig" => ("Microsoft.AppConfiguration/configurationStores", &[]),
        "eventgrid" => ("Microsoft.EventGrid/topics", &[]),
        "servicebus" => ("Microsoft.ServiceBus/namespaces", &[("queues", "queues"), ("topics", "topics")]),
        "eventhubs" => ("Microsoft.EventHub/namespaces", &[("eventhubs", "eventhubs")]),
        other => return Err(format!("'{other}' is not a service a short scope can name: {usage}")),
    };
    let mut res = Resource::new(rtype, name);
    match rest {
        [] => {}
        [kind, part] => {
            let Some((_, path)) = kinds.iter().find(|(k, _)| k.eq_ignore_ascii_case(kind)) else {
                let known: Vec<&str> = kinds.iter().map(|(k, _)| *k).collect();
                return Err(format!(
                    "'{kind}' is not a part of {service}/{name}; it has: {}",
                    if known.is_empty() { "none (name the resource only)".to_string() } else { known.join(", ") }
                ));
            };
            res = res.at(format!("{path}/{part}"));
        }
        _ => return Err(format!("'{text}' has too many parts: {usage}")),
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCT: &str = "/subscriptions/s1/resourcegroups/g1/providers/microsoft.storage/storageaccounts/acct";

    #[test]
    fn a_scope_covers_itself_and_everything_below() {
        assert!(is_within("/", "/subscriptions/s1"));
        assert!(is_within("/subscriptions/s1", "/subscriptions/s1/resourcegroups/g1"));
        assert!(is_within("/subscriptions/s1", "/subscriptions/s1"));
        assert!(!is_within("/subscriptions/s1", "/subscriptions/s10"));
        assert!(!is_within("/subscriptions/s1/resourcegroups/g1", "/subscriptions/s1"));
    }

    #[test]
    fn known_resource_is_covered_from_above_and_by_narrower_scopes_for_its_parts() {
        let r = Resource::container("acct", "c1");
        assert!(covers("/subscriptions/s1", &r, Some(ACCT)));
        assert!(covers("/subscriptions/s1/resourcegroups/g1", &r, Some(ACCT)));
        assert!(covers(ACCT, &r, Some(ACCT)));
        assert!(covers(&format!("{ACCT}/blobservices/default/containers/c1"), &r, Some(ACCT)));
        assert!(!covers(&format!("{ACCT}/blobservices/default/containers/c2"), &r, Some(ACCT)));
        assert!(!covers("/subscriptions/s2", &r, Some(ACCT)));
        // a scope on one container does not cover the account itself
        assert!(!covers(&format!("{ACCT}/blobservices/default/containers/c1"), &Resource::storage("acct"), Some(ACCT)));
    }

    #[test]
    fn unknown_resource_is_covered_only_by_a_scope_that_names_it() {
        let r = Resource::container("acct", "c1");
        assert!(!covers("/subscriptions/s1", &r, None));
        assert!(!covers("/subscriptions/s1/resourcegroups/g1", &r, None));
        assert!(!covers("/", &r, None));
        assert!(covers(ACCT, &r, None));
        assert!(covers(&format!("{ACCT}/blobservices/default/containers/c1"), &r, None));
        assert!(!covers(&format!("{ACCT}/blobservices/default/containers/c2"), &r, None));
        assert!(!covers("/subscriptions/s1/resourcegroups/g1/providers/microsoft.storage/storageaccounts/acct2", &r, None));
    }

    #[test]
    fn short_forms_become_local_ids_that_name_the_resource() {
        let r = parse_short("storage/Acct/containers/Data").unwrap();
        assert_eq!(r.sub, "blobServices/default/containers/Data");
        let id = normalize(&local_id(&r));
        assert_eq!(
            id,
            "/subscriptions/00000000-0000-0000-0000-000000000000/resourcegroups/azemu-local/providers/microsoft.storage/storageaccounts/acct/blobservices/default/containers/data"
        );
        assert!(covers(&id, &Resource::container("acct", "data"), None));
        // the name of the account decides, not the subscription
        assert!(covers(&id, &Resource::container("acct", "data"), Some(ACCT)));
        assert!(parse_short("storage/acct/blobs/x").is_err());
        assert!(parse_short("nonsense/acct").is_err());
        assert!(parse_short("storage").is_err());
        assert_eq!(parse_short("keyvault/v/secrets/s1").unwrap().sub, "secrets/s1");
        assert_eq!(parse_short("servicebus/ns/queues/q").unwrap().rtype, "Microsoft.ServiceBus/namespaces");
    }
}
