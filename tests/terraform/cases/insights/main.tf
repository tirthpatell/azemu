# Application Insights and Log Analytics: a workspace and a component that sends to it. The connection string of the component leads to the
# emulator's monitor service; the check sends one event with it and reads it back.

resource "azurerm_resource_group" "rg" {
  name     = "tf-insights"
  location = "eastus2"
}

resource "azurerm_log_analytics_workspace" "ws" {
  name                = "tf-insights-ws"
  location            = azurerm_resource_group.rg.location
  resource_group_name = azurerm_resource_group.rg.name
  sku                 = "PerGB2018"
  retention_in_days   = 30
}

resource "azurerm_application_insights" "ai" {
  name                = "tf-insights-ai"
  location            = azurerm_resource_group.rg.location
  resource_group_name = azurerm_resource_group.rg.name
  workspace_id        = azurerm_log_analytics_workspace.ws.id
  application_type    = "web"
}

output "connection_string" {
  value     = azurerm_application_insights.ai.connection_string
  sensitive = true
}

output "instrumentation_key" {
  value     = azurerm_application_insights.ai.instrumentation_key
  sensitive = true
}

output "app_id" {
  value = azurerm_application_insights.ai.app_id
}

output "workspace_key" {
  value     = azurerm_log_analytics_workspace.ws.primary_shared_key
  sensitive = true
}
