# Event Hubs: namespace, hub, consumer group.

resource "azurerm_resource_group" "rg" {
  name     = "tf-eh"
  location = "eastus"
}

resource "azurerm_eventhub_namespace" "ns" {
  name                = "tf-ehns-01"
  resource_group_name = azurerm_resource_group.rg.name
  location            = azurerm_resource_group.rg.location
  sku                 = "Standard"
}

resource "azurerm_eventhub" "hub" {
  name              = "hub1"
  namespace_id      = azurerm_eventhub_namespace.ns.id
  partition_count   = 2
  message_retention = 2
}

resource "azurerm_eventhub_consumer_group" "workers" {
  name                = "workers"
  namespace_name      = azurerm_eventhub_namespace.ns.name
  eventhub_name       = azurerm_eventhub.hub.name
  resource_group_name = azurerm_resource_group.rg.name
}

output "connection_string" {
  value     = azurerm_eventhub_namespace.ns.default_primary_connection_string
  sensitive = true
}
