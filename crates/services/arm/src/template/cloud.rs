//! `environment()`: what a template learns about the cloud it runs in. Shape and public-cloud values: Microsoft Learn,
//! `azure-resource-manager/templates/template-functions-deployment#environment`.
//!
//! A template builds host names from these values (`format('https://{0}.blob.{1}', name, environment().suffixes.storage)`),
//! so the suffixes of the services the emulator runs point at the emulator, as `/metadata/endpoints` does: the storage
//! suffix is `core.localhost:{port}` (the Blob host of an account is `{account}.blob.core.localhost:{port}`) and the Key Vault
//! suffix `.vault.localhost:{port}`. Everything the emulator has no service for keeps the public-cloud value.

use serde_json::{json, Value};

/// `authority` is the host and port the client used (`management.localhost:4577`).
pub fn environment(authority: &str) -> Value {
    // `management.localhost:4577` -> `localhost:4577`
    let rest = match authority.split_once('.') {
        Some((_, r)) if !r.is_empty() => r.to_string(),
        _ => {
            let port = authority.rsplit_once(':').map(|(_, p)| format!(":{p}")).unwrap_or_default();
            format!("localhost{port}")
        }
    };
    json!({
        "name": "AzureCloud",
        "gallery": "https://gallery.azure.com/",
        "graph": "https://graph.windows.net/",
        "portal": "https://portal.azure.com",
        "graphAudience": "https://graph.windows.net/",
        "activeDirectoryDataLake": "https://datalake.azure.net/",
        "batch": "https://batch.core.windows.net/",
        "media": "https://rest.media.azure.net",
        "sqlManagement": "https://management.core.windows.net:8443/",
        "vmImageAliasDoc": "https://raw.githubusercontent.com/Azure/azure-rest-api-specs/master/arm-compute/quickstart-templates/aliases.json",
        "resourceManager": format!("https://management.{rest}/"),
        "authentication": {
            "loginEndpoint": format!("https://login.{rest}/"),
            "audiences": ["https://management.core.windows.net/", "https://management.azure.com/"],
            "tenant": "common",
            "identityProvider": "AAD"
        },
        "suffixes": {
            "acrLoginServer": ".azurecr.io",
            "azureDatalakeAnalyticsCatalogAndJob": "azuredatalakeanalytics.net",
            "azureDatalakeStoreFileSystem": "azuredatalakestore.net",
            "azureFrontDoorEndpointSuffix": "azurefd.net",
            "keyvaultDns": format!(".vault.{rest}"),
            "sqlServerHostname": ".database.windows.net",
            "storage": format!("core.{rest}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_at_the_emulator_where_it_has_a_service() {
        let e = environment("management.localhost:4577");
        assert_eq!(e["resourceManager"], "https://management.localhost:4577/");
        assert_eq!(e["suffixes"]["storage"], "core.localhost:4577");
        assert_eq!(e["suffixes"]["keyvaultDns"], ".vault.localhost:4577");
        assert_eq!(e["authentication"]["loginEndpoint"], "https://login.localhost:4577/");
        // no service behind these: the public cloud's values
        assert_eq!(e["suffixes"]["sqlServerHostname"], ".database.windows.net");
        assert_eq!(e["name"], "AzureCloud");
        let plain = environment("localhost");
        assert_eq!(plain["suffixes"]["storage"], "core.localhost");
    }
}
