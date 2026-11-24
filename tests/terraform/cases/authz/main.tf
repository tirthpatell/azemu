# Role assignments (Storage, Resource Manager scopes, Cosmos DB's own roles): what `--strict-auth` decides by. The emulator stores and serves them
# whether the mode is on or not; `azurerm_role_assignment` looks the role up by name, makes the assignment under a GUID and reads it back.

locals {
  principal = "aaaaaaaa-0000-4000-8000-000000000099"
}

resource "azurerm_resource_group" "rg" {
  name     = "tf-authz"
  location = "eastus"
}

resource "azurerm_storage_account" "sa" {
  name                     = "tfauthzacct01"
  resource_group_name      = azurerm_resource_group.rg.name
  location                 = azurerm_resource_group.rg.location
  account_tier             = "Standard"
  account_replication_type = "LRS"
}

resource "azurerm_storage_container" "data" {
  name                  = "data"
  storage_account_id    = azurerm_storage_account.sa.id
  container_access_type = "private"
}

# at the account, by role name
resource "azurerm_role_assignment" "account" {
  scope                = azurerm_storage_account.sa.id
  role_definition_name = "Storage Blob Data Contributor"
  principal_id         = local.principal
  principal_type       = "ServicePrincipal"
}

# at one container
resource "azurerm_role_assignment" "container" {
  scope                = azurerm_storage_container.data.resource_manager_id
  role_definition_name = "Storage Blob Data Reader"
  principal_id         = local.principal
  principal_type       = "ServicePrincipal"
}

# at the resource group, by the role's id
data "azurerm_role_definition" "reader" {
  name  = "Reader"
  scope = azurerm_resource_group.rg.id
}

resource "azurerm_role_assignment" "group" {
  scope              = azurerm_resource_group.rg.id
  role_definition_id = data.azurerm_role_definition.reader.id
  principal_id       = local.principal
  principal_type     = "ServicePrincipal"
}

resource "azurerm_cosmosdb_account" "db" {
  name                = "tf-authz-cosmos-01"
  resource_group_name = azurerm_resource_group.rg.name
  location            = azurerm_resource_group.rg.location
  offer_type          = "Standard"
  kind                = "GlobalDocumentDB"

  consistency_policy {
    consistency_level = "Session"
  }

  geo_location {
    location          = azurerm_resource_group.rg.location
    failover_priority = 0
  }
}

# Cosmos DB's own role system: the built-in Data Contributor, at the account
resource "azurerm_cosmosdb_sql_role_assignment" "contributor" {
  resource_group_name = azurerm_resource_group.rg.name
  account_name        = azurerm_cosmosdb_account.db.name
  role_definition_id  = "${azurerm_cosmosdb_account.db.id}/sqlRoleDefinitions/00000000-0000-0000-0000-000000000002"
  principal_id        = local.principal
  scope               = azurerm_cosmosdb_account.db.id
}

output "principal" {
  value = local.principal
}

output "account" {
  value = azurerm_storage_account.sa.name
}

output "group_id" {
  value = azurerm_resource_group.rg.id
}

output "cosmos_account" {
  value = azurerm_cosmosdb_account.db.name
}
