# Azure Files: a share made through Resource Manager (storage_account_id), one made through the data plane (storage_account_name, with the
# account key), and a directory and a file in the second. Resource Manager lists both.

resource "azurerm_resource_group" "rg" {
  name     = "tf-shares"
  location = "eastus"
}

resource "azurerm_storage_account" "sa" {
  name                     = "tfsharesacct01"
  resource_group_name      = azurerm_resource_group.rg.name
  location                 = azurerm_resource_group.rg.location
  account_tier             = "Standard"
  account_replication_type = "LRS"
}

# the management plane form
resource "azurerm_storage_share" "arm" {
  name               = "viaarm"
  storage_account_id = azurerm_storage_account.sa.id
  quota              = 5
  access_tier        = "Hot"
  metadata = {
    owner = "terraform"
  }
}

# the data plane form
resource "azurerm_storage_share" "data" {
  name                 = "viadata"
  storage_account_name = azurerm_storage_account.sa.name
  quota                = 7
}

resource "azurerm_storage_share_directory" "dir" {
  name             = "docs"
  storage_share_id = azurerm_storage_share.data.url
}

resource "azurerm_storage_share_file" "file" {
  name             = "hello.txt"
  storage_share_id = azurerm_storage_share.data.url
  path             = azurerm_storage_share_directory.dir.name
  source           = "${path.module}/hello.txt"
}

output "account" {
  value = azurerm_storage_account.sa.name
}

output "account_id" {
  value = azurerm_storage_account.sa.id
}

output "key" {
  value     = azurerm_storage_account.sa.primary_access_key
  sensitive = true
}

output "file_endpoint" {
  value = azurerm_storage_account.sa.primary_file_endpoint
}
