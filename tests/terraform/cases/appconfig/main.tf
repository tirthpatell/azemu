# App Configuration store.

resource "azurerm_resource_group" "rg" {
  name     = "tf-ac"
  location = "eastus"
}

resource "azurerm_app_configuration" "store" {
  name                = "tf-appconfig-01"
  resource_group_name = azurerm_resource_group.rg.name
  location            = azurerm_resource_group.rg.location
  sku                 = "standard"
}

output "endpoint" {
  value = azurerm_app_configuration.store.endpoint
}

output "connection_string" {
  value     = azurerm_app_configuration.store.primary_write_key[0].connection_string
  sensitive = true
}
