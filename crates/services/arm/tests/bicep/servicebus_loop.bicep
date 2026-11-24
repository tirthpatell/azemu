param namespaceName string = 'sb${uniqueString(resourceGroup().id)}'
param queueNames array = [ 'orders', 'billing', 'audit' ]
param location string = resourceGroup().location

var queueList = map(filter(queueNames, n => !startsWith(n, 'audit')), n => toLower(n))

resource ns 'Microsoft.ServiceBus/namespaces@2022-10-01-preview' = {
  name: namespaceName
  location: location
  sku: { name: 'Standard', tier: 'Standard' }
}

resource queues 'Microsoft.ServiceBus/namespaces/queues@2022-10-01-preview' = [for q in queueList: {
  parent: ns
  name: q
  properties: { maxDeliveryCount: 5 }
}]

output created array = map(queueNames, n => '${namespaceName}/${n}')
output sorted array = sort(queueNames, (a, b) => a < b)
output endpoint string = ns.properties.serviceBusEndpoint
