# Cosmos DB: account, SQL database, SQL container.

resource "azurerm_resource_group" "rg" {
  name     = "tf-cosmos"
  location = "eastus"
}

resource "azurerm_cosmosdb_account" "db" {
  name                = "tf-cosmos-01"
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

resource "azurerm_cosmosdb_sql_database" "app" {
  name                = "appdb"
  resource_group_name = azurerm_resource_group.rg.name
  account_name        = azurerm_cosmosdb_account.db.name
  throughput          = 400
}

resource "azurerm_cosmosdb_sql_container" "items" {
  name                  = "items"
  resource_group_name   = azurerm_resource_group.rg.name
  account_name          = azurerm_cosmosdb_account.db.name
  database_name         = azurerm_cosmosdb_sql_database.app.name
  partition_key_paths   = ["/pk"]
  partition_key_version = 2
}

output "endpoint" {
  value = azurerm_cosmosdb_account.db.endpoint
}

output "key" {
  value     = azurerm_cosmosdb_account.db.primary_key
  sensitive = true
}
