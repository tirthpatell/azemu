func queueName(prefix string, i int) string => toLower('${prefix}-q${padLeft(string(i), 3, '0')}')

param prefix string = 'Orders'
param namespaceName string = 'sbfn${uniqueString(resourceGroup().id)}'

resource ns 'Microsoft.ServiceBus/namespaces@2022-10-01-preview' = {
  name: namespaceName
  location: resourceGroup().location
  sku: { name: 'Standard', tier: 'Standard' }
}

resource q 'Microsoft.ServiceBus/namespaces/queues@2022-10-01-preview' = [for i in range(1, 2): {
  parent: ns
  name: queueName(prefix, i)
}]

output names array = [for i in range(1, 2): queueName(prefix, i)]
