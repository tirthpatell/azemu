# Resource group, storage account, container, queues, table, blob.

resource "azurerm_resource_group" "rg" {
  name     = "tf-base"
  location = "eastus"
}

resource "azurerm_storage_account" "sa" {
  name                     = "tfbaseacct01"
  resource_group_name      = azurerm_resource_group.rg.name
  location                 = azurerm_resource_group.rg.location
  account_tier             = "Standard"
  account_replication_type = "LRS"
}

# the management plane form (storage_account_id) and the data plane form (storage_account_name)
resource "azurerm_storage_container" "photos" {
  name                  = "photos"
  storage_account_id    = azurerm_storage_account.sa.id
  container_access_type = "private"
}

resource "azurerm_storage_queue" "jobs" {
  name                 = "jobs"
  storage_account_name = azurerm_storage_account.sa.name
}

resource "azurerm_storage_queue" "jobs2" {
  name               = "jobs2"
  storage_account_id = azurerm_storage_account.sa.id
}

resource "azurerm_storage_table" "people" {
  name                 = "People"
  storage_account_name = azurerm_storage_account.sa.name
}

resource "azurerm_storage_blob" "hello" {
  name                   = "hello.txt"
  storage_account_name   = azurerm_storage_account.sa.name
  storage_container_name = azurerm_storage_container.photos.name
  type                   = "Block"
  source_content         = "hello from terraform"
  content_type           = "text/plain"
}

output "account" {
  value = azurerm_storage_account.sa.name
}

output "key" {
  value     = azurerm_storage_account.sa.primary_access_key
  sensitive = true
}

output "blob_endpoint" {
  value = azurerm_storage_account.sa.primary_blob_endpoint
}

output "queue_endpoint" {
  value = azurerm_storage_account.sa.primary_queue_endpoint
}

output "table_endpoint" {
  value = azurerm_storage_account.sa.primary_table_endpoint
}
