param vaultName string = 'kv${uniqueString(resourceGroup().id)}'
param location string = resourceGroup().location
param baseTime string = utcNow('u')

resource kv 'Microsoft.KeyVault/vaults@2023-07-01' = {
  name: vaultName
  location: location
  tags: {
    // the vault is reviewed a year after it was made
    reviewBy: dateTimeAdd(baseTime, 'P1Y', 'yyyy-MM-dd')
  }
  properties: {
    tenantId: subscription().tenantId
    sku: { family: 'A', name: 'standard' }
    accessPolicies: []
  }
}

output reviewBy string = kv.tags.reviewBy
output reviewEpoch int = dateTimeToEpoch(dateTimeAdd(baseTime, 'P1Y'))
output fixedExpiry string = dateTimeAdd('2024-01-15T10:00:00Z', 'P30D')
output vaultUri string = kv.properties.vaultUri
