# An ARM template deployed through azurerm_resource_group_template_deployment: a storage account and a container.

resource "azurerm_resource_group" "rg" {
  name     = "tf-template"
  location = "eastus"
}

resource "azurerm_resource_group_template_deployment" "t" {
  name                = "storage-deployment"
  resource_group_name = azurerm_resource_group.rg.name
  deployment_mode     = "Incremental"

  parameters_content = jsonencode({
    accountName = { value = "tftemplate01" }
  })

  template_content = <<TEMPLATE
{
  "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
  "contentVersion": "1.0.0.0",
  "parameters": {
    "accountName": { "type": "string" }
  },
  "resources": [
    {
      "type": "Microsoft.Storage/storageAccounts",
      "apiVersion": "2023-05-01",
      "name": "[parameters('accountName')]",
      "location": "[resourceGroup().location]",
      "sku": { "name": "Standard_LRS" },
      "kind": "StorageV2",
      "properties": {}
    },
    {
      "type": "Microsoft.Storage/storageAccounts/blobServices/containers",
      "apiVersion": "2023-05-01",
      "name": "[format('{0}/default/templated', parameters('accountName'))]",
      "dependsOn": [ "[resourceId('Microsoft.Storage/storageAccounts', parameters('accountName'))]" ],
      "properties": { "publicAccess": "None" }
    }
  ],
  "outputs": {
    "blobEndpoint": {
      "type": "string",
      "value": "[reference(resourceId('Microsoft.Storage/storageAccounts', parameters('accountName'))).primaryEndpoints.blob]"
    }
  }
}
TEMPLATE
}

output "blob_endpoint" {
  value = jsondecode(azurerm_resource_group_template_deployment.t.output_content).blobEndpoint.value
}
