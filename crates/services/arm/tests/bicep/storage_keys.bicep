param name string = 'stg${uniqueString(resourceGroup().id)}'
param location string = resourceGroup().location

resource sa 'Microsoft.Storage/storageAccounts@2023-05-01' = {
  name: name
  location: location
  sku: { name: 'Standard_LRS' }
  kind: 'StorageV2'
}

var conn = 'DefaultEndpointsProtocol=https;AccountName=${sa.name};AccountKey=${sa.listKeys().keys[0].value};EndpointSuffix=${environment().suffixes.storage}'

output key1Length int = length(sa.listKeys().keys[0].value)
output connectionString string = conn
output blobEndpoint string = sa.properties.primaryEndpoints.blob
