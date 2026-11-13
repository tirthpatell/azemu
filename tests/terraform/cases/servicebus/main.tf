# Service Bus: namespace, queue, topic, subscription with a rule.

resource "azurerm_resource_group" "rg" {
  name     = "tf-sb"
  location = "eastus"
}

resource "azurerm_servicebus_namespace" "ns" {
  name                = "tf-sbns-01"
  resource_group_name = azurerm_resource_group.rg.name
  location            = azurerm_resource_group.rg.location
  sku                 = "Standard"
}

resource "azurerm_servicebus_queue" "orders" {
  name         = "orders"
  namespace_id = azurerm_servicebus_namespace.ns.id

  max_delivery_count = 5
  lock_duration      = "PT45S"
}

resource "azurerm_servicebus_topic" "events" {
  name         = "events"
  namespace_id = azurerm_servicebus_namespace.ns.id
}

resource "azurerm_servicebus_subscription" "reds" {
  name               = "reds"
  topic_id           = azurerm_servicebus_topic.events.id
  max_delivery_count = 10
}

resource "azurerm_servicebus_subscription_rule" "only_red" {
  name            = "only-red"
  subscription_id = azurerm_servicebus_subscription.reds.id
  filter_type     = "SqlFilter"
  sql_filter      = "color = 'red'"
}

output "connection_string" {
  value     = azurerm_servicebus_namespace.ns.default_primary_connection_string
  sensitive = true
}
