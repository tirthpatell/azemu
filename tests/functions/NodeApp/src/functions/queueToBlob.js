const { app, output } = require('@azure/functions');

// Queue trigger with a Blob output binding: the same kind of function as queue_to_blob in the .NET app.
const marker = output.storageBlob({ path: 'markers/node-queue-{queueTrigger}.txt', connection: 'AzureWebJobsStorage' });

app.storageQueue('nodeQueueToBlob', {
  queueName: 'nq-in',
  connection: 'AzureWebJobsStorage',
  extraOutputs: [marker],
  handler: (message, context) => {
    context.extraOutputs.set(marker, `node worker got '${message}' on ${process.version}`);
  },
});

app.http('nodePing', {
  methods: ['GET'],
  authLevel: 'anonymous',
  route: 'node-ping',
  handler: async () => ({ body: 'pong' }),
});

// Service Bus queue trigger (peek-lock, auto-complete) with a Blob output, through the extension bundle's Service Bus extension.
const sbMarker = output.storageBlob({ path: 'markers/node-sb.txt', connection: 'AzureWebJobsStorage' });
app.serviceBusQueue('nodeSbQueue', {
  queueName: 'nsbq',
  connection: 'ServiceBusConnection',
  extraOutputs: [sbMarker],
  handler: (message, context) => {
    context.extraOutputs.set(sbMarker, `node worker got service bus message '${message}'`);
  },
});
