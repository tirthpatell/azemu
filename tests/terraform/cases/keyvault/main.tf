# Key Vault.

data "azurerm_client_config" "current" {}

resource "azurerm_resource_group" "rg" {
  name     = "tf-kv"
  location = "eastus"
}

resource "azurerm_key_vault" "kv" {
  name                = "tfkv-vault-01"
  resource_group_name = azurerm_resource_group.rg.name
  location            = azurerm_resource_group.rg.location
  tenant_id           = data.azurerm_client_config.current.tenant_id
  sku_name            = "standard"

  access_policy {
    tenant_id          = data.azurerm_client_config.current.tenant_id
    object_id          = data.azurerm_client_config.current.object_id
    secret_permissions = ["Get", "Set", "List", "Delete", "Purge"]
  }
}

output "vault_uri" {
  value = azurerm_key_vault.kv.vault_uri
}
