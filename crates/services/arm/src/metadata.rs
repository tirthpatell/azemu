//! `GET /metadata/endpoints?api-version=2022-09-01`: the cloud metadata document that
//! the azurerm Terraform provider (`metadata_host`) and the Azure CLI read to find the
//! other endpoints. Every endpoint it names is an emulator address built from the host
//! the client used, so the login endpoint is the identity service (`login.localhost`).

use azemu_core::Request;
use azemu_core::Response;
use serde_json::json;

use crate::error::{ArmError, ArmResult};
use crate::util;

pub fn handle(req: &Request) -> ArmResult<Response> {
    if req.method != azemu_core::http::Method::GET {
        return Err(ArmError::new(405, "MethodNotAllowed", "Only GET is allowed on /metadata/endpoints."));
    }
    let authority = &req.ctx.endpoint.authority;
    // `management.localhost:4577` -> `localhost:4577`
    let rest = match authority.split_once('.') {
        Some((_, r)) if !r.is_empty() => r.to_string(),
        _ => {
            let port = authority.rsplit_once(':').map(|(_, p)| format!(":{p}")).unwrap_or_default();
            format!("localhost{port}")
        }
    };
    let arm = format!("https://management.{rest}/");
    let login = format!("https://login.{rest}/");
    let storage_suffix = format!("core.{rest}");
    Ok(util::json_ok(
        200,
        &json!({
            "portal": format!("https://portal.{rest}"),
            "authentication": {
                "loginEndpoint": login,
                "audiences": ["https://management.core.windows.net/", "https://management.azure.com/"],
                "tenant": "common",
                "identityProvider": "AAD"
            },
            "media": "https://rest.media.azure.net",
            "graphAudience": "https://graph.microsoft.com/",
            "graph": "https://graph.microsoft.com/",
            "name": "AzEmu",
            "suffixes": {
                "acrLoginServer": format!("azurecr.{rest}"),
                "keyVaultDns": format!("vault.{rest}"),
                "sqlServerHostname": format!("database.{rest}"),
                "storage": storage_suffix,
                "mhsmDns": format!("managedhsm.{rest}"),
                "attestationEndpoint": format!("attest.{rest}")
            },
            "batch": "https://batch.core.windows.net/",
            "resourceManager": arm,
            "vmImageAliasDoc": "https://raw.githubusercontent.com/Azure/azure-rest-api-specs/main/arm-compute/quickstart-templates/aliases.json",
            "activeDirectoryDataLake": "https://datalake.azure.net/",
            "sqlManagement": "https://management.core.windows.net:8443/",
            "microsoftGraphResourceId": "https://graph.microsoft.com/",
            "appInsightsResourceId": "https://api.applicationinsights.io",
            "appInsightsTelemetryChannelResourceId": "https://dc.applicationinsights.azure.com/v2/track",
            "attestationResourceId": "https://attest.azure.net",
            "synapseAnalyticsResourceId": "https://dev.azuresynapse.net",
            "logAnalyticsResourceId": "https://api.loganalytics.io",
            "ossrDbmsResourceId": "https://ossrdbms-aad.database.windows.net",
            "gallery": "https://gallery.azure.com/"
        }),
    ))
}
