type queueSetting = {
  name: string
  @minValue(1)
  maxDelivery: int?
}

@allowed(['Standard', 'Premium'])
param skuName string = 'Standard'
param settings queueSetting[] = [
  { name: 'a', maxDelivery: 3 }
  { name: 'b' }
]
param ns string = 'probe${uniqueString(resourceGroup().id)}'

resource sa 'Microsoft.Storage/storageAccounts@2023-05-01' = {
  name: 'st${uniqueString(resourceGroup().id)}'
  location: resourceGroup().location
  sku: { name: 'Standard_LRS' }
  kind: 'StorageV2'
}

resource bus 'Microsoft.ServiceBus/namespaces@2022-10-01-preview' = {
  name: ns
  location: resourceGroup().location
  sku: { name: skuName, tier: skuName }
}

resource ex 'Microsoft.ServiceBus/namespaces@2022-10-01-preview' existing = {
  name: ns
  dependsOn: [bus]
}

resource q 'Microsoft.ServiceBus/namespaces/queues@2022-10-01-preview' = [for s in settings: {
  parent: ex
  name: s.name
  properties: { maxDeliveryCount: s.?maxDelivery ?? 10 }
}]

output saId string = sa.id
output saKeys string = sa.listKeys().keys[0].keyName
output endpoint string = bus.properties.serviceBusEndpoint
output qids array = [for i in range(0, length(settings)): q[i].id]
output exname string = ex.name
output st queueSetting[] = settings
