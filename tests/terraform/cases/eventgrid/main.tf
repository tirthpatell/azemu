# Event Grid: a storage account raises events; one subscription on the account itself (the extension form) and one
# on a system topic, both delivering to Storage queues; one to a Service Bus queue with delivery properties (static, secret,
# dynamic, and the MessageId of the message); one to a queue that does not exist with a blob container for its dead letters.

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

resource "azurerm_storage_container" "deadletters" {
  name               = "deadletters"
  storage_account_id = azurerm_storage_account.sa.id
}

resource "azurerm_servicebus_namespace" "ns" {
  name                = "tf-egsb-01"
  resource_group_name = azurerm_resource_group.rg.name
  location            = azurerm_resource_group.rg.location
  sku                 = "Standard"
}

resource "azurerm_servicebus_queue" "props" {
  name         = "props"
  namespace_id = azurerm_servicebus_namespace.ns.id
}

resource "azurerm_eventgrid_event_subscription" "props" {
  name  = "to-props"
  scope = azurerm_storage_account.sa.id

  included_event_types = ["Microsoft.Storage.BlobCreated"]

  subject_filter {
    subject_begins_with = "/blobServices/default/containers/inbox/blobs/props-"
  }

  service_bus_queue_endpoint_id = azurerm_servicebus_queue.props.id

  delivery_property {
    header_name = "x-static"
    type        = "Static"
    value       = "abc"
  }

  delivery_property {
    header_name = "x-secret"
    type        = "Static"
    value       = "hush"
    secret      = true
  }

  delivery_property {
    header_name  = "x-dyn"
    type         = "Dynamic"
    source_field = "data.contentType"
  }

  delivery_property {
    header_name  = "MessageId"
    type         = "Dynamic"
    source_field = "data.eTag"
  }
}

resource "azurerm_eventgrid_event_subscription" "dead" {
  name  = "to-nowhere"
  scope = azurerm_storage_account.sa.id

  included_event_types = ["Microsoft.Storage.BlobCreated"]

  subject_filter {
    subject_begins_with = "/blobServices/default/containers/inbox/blobs/dead-"
  }

  # the queue is not made: every delivery fails, and with one attempt the event goes to the blob container
  storage_queue_endpoint {
    storage_account_id = azurerm_storage_account.sa.id
    queue_name         = "not-made"
  }

  retry_policy {
    max_delivery_attempts = 1
    event_time_to_live    = 1
  }

  storage_blob_dead_letter_destination {
    storage_account_id          = azurerm_storage_account.sa.id
    storage_blob_container_name = azurerm_storage_container.deadletters.name
  }
}

output "sb_connection_string" {
  value     = azurerm_servicebus_namespace.ns.default_primary_connection_string
  sensitive = true
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
