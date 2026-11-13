# Shared by every case: the azurerm provider pointed at the emulator. `run.py` copies this file next to the
# case's main.tf and passes the port with -var.
terraform {
  required_providers {
    azurerm = {
      source  = "hashicorp/azurerm"
      version = "~> 4.0"
    }
  }
}

variable "port" {
  type = number
}

provider "azurerm" {
  features {}

  # The provider reads https://management.localhost:PORT/metadata/endpoints, which points it at the emulator's
  # identity service (tokens) and ARM (everything else).
  metadata_host                   = "management.localhost:${var.port}"
  subscription_id                 = "11111111-2222-3333-4444-555555555555"
  tenant_id                       = "22222222-2222-2222-2222-222222222222"
  client_id                       = "33333333-3333-3333-3333-333333333333"
  client_secret                   = "anything"
  resource_provider_registrations = "none"
  storage_use_azuread             = false
}
