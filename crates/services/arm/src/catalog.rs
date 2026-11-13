//! Static data: regions and the list of resource providers ARM reports.

use serde_json::{json, Value};

/// name, display name, geography, geography group, latitude, longitude, physical location
const REGIONS: &[(&str, &str, &str, &str, &str, &str, &str)] = &[
    ("eastus", "East US", "United States", "US", "37.3719", "-79.8164", "Virginia"),
    ("eastus2", "East US 2", "United States", "US", "36.6681", "-78.3889", "Virginia"),
    ("southcentralus", "South Central US", "United States", "US", "27.9783", "-98.3716", "Texas"),
    ("westus2", "West US 2", "United States", "US", "47.233", "-119.852", "Washington"),
    ("westus3", "West US 3", "United States", "US", "33.448376", "-112.074036", "Phoenix"),
    ("centralus", "Central US", "United States", "US", "41.5908", "-93.6208", "Iowa"),
    ("northcentralus", "North Central US", "United States", "US", "41.8819", "-87.6278", "Illinois"),
    ("westus", "West US", "United States", "US", "37.783", "-122.417", "California"),
    ("canadacentral", "Canada Central", "Canada", "Canada", "43.653", "-79.383", "Toronto"),
    ("brazilsouth", "Brazil South", "Brazil", "South America", "-23.55", "-46.633", "Sao Paulo State"),
    ("northeurope", "North Europe", "Europe", "Europe", "53.3478", "-6.2597", "Ireland"),
    ("westeurope", "West Europe", "Europe", "Europe", "52.3667", "4.9", "Netherlands"),
    ("uksouth", "UK South", "United Kingdom", "Europe", "50.941", "-0.799", "London"),
    ("francecentral", "France Central", "France", "Europe", "46.3772", "2.373", "Paris"),
    ("germanywestcentral", "Germany West Central", "Germany", "Europe", "50.1109", "8.6821", "Frankfurt"),
    ("swedencentral", "Sweden Central", "Sweden", "Europe", "60.67488", "17.14127", "Gavle"),
    ("switzerlandnorth", "Switzerland North", "Switzerland", "Europe", "47.451542", "8.564572", "Zurich"),
    ("norwayeast", "Norway East", "Norway", "Europe", "59.913868", "10.752245", "Norway"),
    ("uaenorth", "UAE North", "UAE", "Middle East", "25.266666", "55.316666", "Dubai"),
    ("southafricanorth", "South Africa North", "South Africa", "Africa", "-25.731340", "28.218370", "Johannesburg"),
    ("centralindia", "Central India", "India", "Asia Pacific", "18.5822", "73.9197", "Pune"),
    ("southeastasia", "Southeast Asia", "Asia Pacific", "Asia Pacific", "1.283", "103.833", "Singapore"),
    ("eastasia", "East Asia", "Asia Pacific", "Asia Pacific", "22.267", "114.188", "Hong Kong"),
    ("japaneast", "Japan East", "Japan", "Asia Pacific", "35.68", "139.77", "Tokyo, Saitama"),
    ("japanwest", "Japan West", "Japan", "Asia Pacific", "34.6939", "135.5022", "Osaka"),
    ("koreacentral", "Korea Central", "Korea", "Asia Pacific", "37.5665", "126.9780", "Seoul"),
    ("australiaeast", "Australia East", "Australia", "Asia Pacific", "-33.86", "151.2094", "New South Wales"),
];

pub fn region_names() -> Vec<&'static str> {
    REGIONS.iter().map(|r| r.0).collect()
}

/// The display name of a region (`eastus` gives `East US`), when the emulator knows it.
pub fn display_name(location: &str) -> Option<&'static str> {
    REGIONS.iter().find(|r| r.0.eq_ignore_ascii_case(location)).map(|r| r.1)
}

pub fn locations(sub: &str) -> Vec<Value> {
    REGIONS
        .iter()
        .map(|(name, display, geo, group, lat, lon, phys)| {
            json!({
                "id": format!("/subscriptions/{sub}/locations/{name}"),
                "name": name,
                "type": "Region",
                "displayName": display,
                "regionalDisplayName": format!("({group}) {display}"),
                "metadata": {
                    "regionType": "Physical",
                    "regionCategory": "Recommended",
                    "geography": geo,
                    "geographyGroup": group,
                    "longitude": lon,
                    "latitude": lat,
                    "physicalLocation": phys,
                }
            })
        })
        .collect()
}

/// namespace, resource types (relative names), newest API version we know
const PROVIDERS: &[(&str, &[&str], &str)] = &[
    ("Microsoft.Resources", &["subscriptions", "resourceGroups", "deployments", "tags", "resources", "providers"], "2025-04-01"),
    (
        "Microsoft.Storage",
        &[
            "storageAccounts",
            "storageAccounts/blobServices",
            "storageAccounts/blobServices/containers",
            "storageAccounts/queueServices",
            "storageAccounts/queueServices/queues",
            "storageAccounts/tableServices",
            "storageAccounts/tableServices/tables",
            "storageAccounts/fileServices",
            "checkNameAvailability",
            "skus",
        ],
        "2025-01-01",
    ),
    (
        "Microsoft.ServiceBus",
        &[
            "namespaces",
            "namespaces/queues",
            "namespaces/topics",
            "namespaces/topics/subscriptions",
            "namespaces/topics/subscriptions/rules",
            "namespaces/authorizationRules",
            "namespaces/queues/authorizationRules",
            "namespaces/topics/authorizationRules",
            "checkNameAvailability",
        ],
        "2024-01-01",
    ),
    (
        "Microsoft.EventHub",
        &[
            "namespaces",
            "namespaces/eventhubs",
            "namespaces/eventhubs/consumergroups",
            "namespaces/authorizationRules",
            "namespaces/eventhubs/authorizationRules",
            "checkNameAvailability",
        ],
        "2024-01-01",
    ),
    ("Microsoft.KeyVault", &["vaults", "checkNameAvailability", "deletedVaults"], "2024-11-01"),
    (
        "Microsoft.DocumentDB",
        &[
            "databaseAccounts",
            "databaseAccounts/sqlDatabases",
            "databaseAccounts/sqlDatabases/containers",
            "databaseAccounts/sqlDatabases/throughputSettings",
        ],
        "2025-04-15",
    ),
    ("Microsoft.AppConfiguration", &["configurationStores", "checkNameAvailability"], "2024-06-01"),
    ("Microsoft.EventGrid", &["topics", "eventSubscriptions", "systemTopics", "domains", "namespaces"], "2025-02-15"),
    ("Microsoft.Compute", &["virtualMachines", "disks", "availabilitySets"], "2024-07-01"),
    ("Microsoft.Network", &["virtualNetworks", "networkSecurityGroups", "publicIPAddresses", "networkInterfaces"], "2024-05-01"),
    ("Microsoft.Web", &["sites", "serverfarms"], "2024-04-01"),
    ("Microsoft.Authorization", &["roleAssignments", "roleDefinitions", "locks", "policyAssignments"], "2022-04-01"),
    ("Microsoft.ManagedIdentity", &["userAssignedIdentities"], "2023-01-31"),
    ("Microsoft.Insights", &["components", "diagnosticSettings"], "2020-02-02"),
    ("Microsoft.OperationalInsights", &["workspaces"], "2023-09-01"),
    ("Microsoft.Sql", &["servers", "servers/databases"], "2023-08-01-preview"),
    ("Microsoft.ContainerService", &["managedClusters"], "2024-05-01"),
    ("Microsoft.ContainerRegistry", &["registries"], "2023-07-01"),
    ("Microsoft.Cache", &["redis"], "2024-03-01"),
];

pub fn namespaces() -> Vec<&'static str> {
    PROVIDERS.iter().map(|p| p.0).collect()
}

/// The ARM provider object. `sub` is `None` for the tenant-level listing.
pub fn provider_json(sub: Option<&str>, ns: &str) -> Option<Value> {
    let (name, types, api) = PROVIDERS.iter().find(|p| p.0.eq_ignore_ascii_case(ns))?;
    let id = match sub {
        Some(s) => format!("/subscriptions/{s}/providers/{name}"),
        None => format!("/providers/{name}"),
    };
    let regions: Vec<&str> = region_names();
    let resource_types: Vec<Value> = types
        .iter()
        .map(|t| {
            json!({
                "resourceType": t,
                "locations": regions.clone(),
                "apiVersions": [api],
                "capabilities": "None",
            })
        })
        .collect();
    Some(json!({
        "id": id,
        "namespace": name,
        "registrationState": "Registered",
        "registrationPolicy": "RegistrationRequired",
        "resourceTypes": resource_types,
    }))
}

pub fn canonical_namespace(ns: &str) -> Option<&'static str> {
    PROVIDERS.iter().find(|p| p.0.eq_ignore_ascii_case(ns)).map(|p| p.0)
}
