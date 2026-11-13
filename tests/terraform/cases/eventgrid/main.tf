# Event Grid: a storage account raises events; one subscription on the account itself (the extension form) and one
# on a system topic, both delivering to Storage queues.

resource "azurerm_resource_group" "rg" {
  name     = "tf-eg"
  location = "eastus"
}

resource "azurerm_storage_account" "sa" {
  name                     = "tfegacct01"
  resource_group_name      = azurerm_resource_group.rg.name
  location                 = azurerm_resource_group.rg.location
  account_tier             = "Standard"
  account_replication_type = "LRS"
}

resource "azurerm_storage_container" "inbox" {
  name               = "inbox"
  storage_account_id = azurerm_storage_account.sa.id
}

resource "azurerm_storage_queue" "direct" {
  name               = "direct"
  storage_account_id = azurerm_storage_account.sa.id
}

resource "azurerm_storage_queue" "viasystem" {
  name               = "viasystem"
  storage_account_id = azurerm_storage_account.sa.id
}

resource "azurerm_eventgrid_event_subscription" "direct" {
  name  = "to-direct"
  scope = azurerm_storage_account.sa.id

  included_event_types = ["Microsoft.Storage.BlobCreated"]

  storage_queue_endpoint {
    storage_account_id = azurerm_storage_account.sa.id
    queue_name         = azurerm_storage_queue.direct.name
  }
}

resource "azurerm_eventgrid_system_topic" "st" {
  name                   = "tf-systopic"
  resource_group_name    = azurerm_resource_group.rg.name
  location               = "global"
  source_arm_resource_id = azurerm_storage_account.sa.id
  topic_type             = "Microsoft.Storage.StorageAccounts"
}

resource "azurerm_eventgrid_system_topic_event_subscription" "viasystem" {
  name                = "to-viasystem"
  system_topic        = azurerm_eventgrid_system_topic.st.name
  resource_group_name = azurerm_resource_group.rg.name

  included_event_types = ["Microsoft.Storage.BlobCreated"]

  subject_filter {
    subject_begins_with = "/blobServices/default/containers/inbox/"
  }

  storage_queue_endpoint {
    storage_account_id = azurerm_storage_account.sa.id
    queue_name         = azurerm_storage_queue.viasystem.name
  }
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

output "blob_endpoint" {
  value = azurerm_storage_account.sa.primary_blob_endpoint
}

output "queue_endpoint" {
  value = azurerm_storage_account.sa.primary_queue_endpoint
}
