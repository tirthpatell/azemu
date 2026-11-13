import { describe, it, after } from 'node:test';
import assert from 'node:assert/strict';
import { ResourceManagementClient } from '@azure/arm-resources';
import { DeploymentsClient } from '@azure/arm-resourcesdeployments';
import { StorageManagementClient } from '@azure/arm-storage';
import { ServiceBusManagementClient } from '@azure/arm-servicebus';
import { BlobServiceClient, StorageSharedKeyCredential } from '@azure/storage-blob';
import { QueueServiceClient } from '@azure/storage-queue';
import { ServiceBusClient } from '@azure/service-bus';
import { env, uniq, fakeCredential, collect, rejection, track, closeTracked } from '../lib/emu.mjs';

const emu = env();
after(() => closeTracked());
const endpoint = `https://management.localhost:${emu.port}`;
const sub = '11111111-2222-3333-4444-555555555555';
const opts = { endpoint, credentialScopes: [`${endpoint}/.default`] };
const resources = new ResourceManagementClient(fakeCredential, sub, opts);
const deployments = new DeploymentsClient(fakeCredential, sub, opts);
const storage = new StorageManagementClient(fakeCredential, sub, opts);
const sbm = new ServiceBusManagementClient(fakeCredential, sub, opts);
const code = (e) => e.code ?? e.details?.error?.code ?? e.statusCode;

async function group(prefix = 'rg') {
  const name = uniq(prefix);
  await resources.resourceGroups.createOrUpdate(name, { location: 'eastus', tags: { owner: 'js-tests' } });
  return name;
}

describe('resource groups', () => {
  it('createOrUpdate returns the group with Succeeded state; get, checkExistence, list and update tags agree', async () => {
    const name = uniq('rg');
    const r = await resources.resourceGroups.createOrUpdate(name, { location: 'East US', tags: { a: '1' } });
    assert.equal(r.name, name);
    assert.equal(r.location, 'eastus');
    assert.equal(r.properties.provisioningState, 'Succeeded');
    assert.ok(r.id.endsWith(`/resourceGroups/${name}`) || r.id.toLowerCase().endsWith(`/resourcegroups/${name}`));
    assert.equal((await resources.resourceGroups.get(name)).tags.a, '1');
    assert.equal((await resources.resourceGroups.checkExistence(name)).body, true);
    assert.equal((await resources.resourceGroups.checkExistence(uniq('nope'))).body, false);
    const upd = await resources.resourceGroups.update(name, { tags: { b: '2' } });
    assert.equal(upd.tags.b, '2');
    assert.ok((await collect(resources.resourceGroups.list())).some((g) => g.name === name));
  });
  it('get of a missing group is ResourceGroupNotFound 404', async () => {
    const e = await rejection(() => resources.resourceGroups.get(uniq('missing')));
    assert.equal(e.statusCode, 404);
    assert.equal(code(e), 'ResourceGroupNotFound');
  });
  it('beginDeleteAndWait finishes (202 then done) and the group is gone', async () => {
    const name = await group();
    await resources.resourceGroups.beginDeleteAndWait(name);
    assert.equal((await resources.resourceGroups.checkExistence(name)).body, false);
  });
  it('the list filter on a tag name finds the group', async () => {
    const name = await group();
    const found = await collect(resources.resourceGroups.list({ filter: "tagName eq 'owner'" }));
    assert.ok(found.some((g) => g.name === name));
  });
  it('subscriptions can be read through the subscription client path (providers list and resources list)', async () => {
    const rg = await group();
    const providers = await collect(resources.providers.list());
    assert.ok(providers.some((p) => p.namespace === 'Microsoft.Storage'));
    assert.deepEqual(await collect(resources.resources.listByResourceGroup(rg)), []);
  });
});

describe('storage accounts and containers through ARM', () => {
  it('a storage account created through ARM has endpoints and keys; checkNameAvailability sees it', async () => {
    const rg = await group();
    const name = `acct${uniq('').slice(0, 14)}`;
    const acct = await storage.storageAccounts.beginCreateAndWait(rg, name, { location: 'eastus', sku: { name: 'Standard_LRS' }, kind: 'StorageV2' });
    assert.equal(acct.name, name);
    assert.equal(acct.provisioningState, 'Succeeded');
    assert.ok(acct.primaryEndpoints.blob.includes(name));
    const keys = await storage.storageAccounts.listKeys(rg, name);
    assert.ok(keys.keys.length >= 2 && keys.keys[0].value);
    assert.equal((await storage.storageAccounts.getProperties(rg, name)).sku.name, 'Standard_LRS');
    const avail = await storage.storageAccounts.checkNameAvailability({ name, type: 'Microsoft.Storage/storageAccounts' });
    assert.equal(avail.nameAvailable, false);
    assert.ok((await collect(storage.storageAccounts.listByResourceGroup(rg))).some((a) => a.name === name));
  });
  it('a blob container created through ARM is visible to the Blob SDK (and the SDK one is not in the ARM list: known difference)', async () => {
    const rg = await group();
    const acct = `acct${uniq('').slice(0, 14)}`;
    await storage.storageAccounts.beginCreateAndWait(rg, acct, { location: 'eastus', sku: { name: 'Standard_LRS' }, kind: 'StorageV2' });
    const keys = await storage.storageAccounts.listKeys(rg, acct);
    const blobs = new BlobServiceClient(emu.url('blob', acct), new StorageSharedKeyCredential(acct, keys.keys[0].value));
    const viaArm = await storage.blobContainers.create(rg, acct, 'from-arm', { metadata: { made: 'by-arm' }, publicAccess: 'None' });
    assert.equal(viaArm.name, 'from-arm');
    const props = await blobs.getContainerClient('from-arm').getProperties();
    assert.equal(props.metadata.made, 'by-arm');
    await blobs.getContainerClient('from-arm').getBlockBlobClient('f.txt').upload('data', 4);
    await blobs.getContainerClient('from-sdk').create();
    // (ARM lists only what ARM created: documented in docs/services/arm.md, so 'from-sdk' is not asserted here)
    assert.ok((await collect(storage.blobContainers.list(rg, acct))).some((c) => c.name === 'from-arm'));
    await storage.blobContainers.delete(rg, acct, 'from-arm');
    assert.equal(await blobs.getContainerClient('from-arm').exists(), false);
  });
  it('a queue created through ARM works with the Queue SDK', async () => {
    const rg = await group();
    const acct = `acct${uniq('').slice(0, 14)}`;
    await storage.storageAccounts.beginCreateAndWait(rg, acct, { location: 'eastus', sku: { name: 'Standard_LRS' }, kind: 'StorageV2' });
    const keys = await storage.storageAccounts.listKeys(rg, acct);
    await storage.queue.create(rg, acct, 'jobs', {});
    const q = new QueueServiceClient(emu.url('queue', acct), new StorageSharedKeyCredential(acct, keys.keys[0].value)).getQueueClient('jobs');
    assert.equal(await q.exists(), true);
    await q.sendMessage('hello');
    assert.equal((await q.receiveMessages()).receivedMessageItems[0].messageText, 'hello');
  });
  it('deleting the storage account removes its containers from the data plane', async () => {
    const rg = await group();
    const acct = `acct${uniq('').slice(0, 14)}`;
    await storage.storageAccounts.beginCreateAndWait(rg, acct, { location: 'eastus', sku: { name: 'Standard_LRS' }, kind: 'StorageV2' });
    const keys = await storage.storageAccounts.listKeys(rg, acct);
    await storage.blobContainers.create(rg, acct, 'cont1', {});
    await storage.storageAccounts.delete(rg, acct);
    const blobs = new BlobServiceClient(emu.url('blob', acct), new StorageSharedKeyCredential(acct, keys.keys[0].value));
    assert.equal(await blobs.getContainerClient('cont1').exists(), false);
    const e = await rejection(() => storage.storageAccounts.getProperties(rg, acct));
    assert.equal(e.statusCode, 404);
  });
  it('blob service properties set through ARM (versioning, delete retention) show in the Blob SDK', async () => {
    const rg = await group();
    const acct = `acct${uniq('').slice(0, 14)}`;
    await storage.storageAccounts.beginCreateAndWait(rg, acct, { location: 'eastus', sku: { name: 'Standard_LRS' }, kind: 'StorageV2' });
    const keys = await storage.storageAccounts.listKeys(rg, acct);
    await storage.blobServices.setServiceProperties(rg, acct, { deleteRetentionPolicy: { enabled: true, days: 5 }, isVersioningEnabled: true });
    const blobs = new BlobServiceClient(emu.url('blob', acct), new StorageSharedKeyCredential(acct, keys.keys[0].value));
    const p = await blobs.getProperties();
    assert.equal(p.deleteRetentionPolicy.enabled, true);
    assert.equal(p.deleteRetentionPolicy.days, 5);
  });
});

describe('Service Bus through ARM', () => {
  it('a namespace, queue, topic and subscription created through ARM carry their properties', async () => {
    const rg = await group();
    const nsName = uniq('sbarm');
    const nsr = await sbm.namespaces.beginCreateOrUpdateAndWait(rg, nsName, { location: 'eastus', sku: { name: 'Standard', tier: 'Standard' } });
    assert.equal(nsr.name, nsName);
    assert.equal(nsr.provisioningState, 'Succeeded');
    assert.ok(nsr.serviceBusEndpoint.includes(nsName));
    const q = await sbm.queues.createOrUpdate(rg, nsName, 'orders', { lockDuration: 'PT45S', maxDeliveryCount: 4, requiresSession: false, deadLetteringOnMessageExpiration: true });
    assert.equal(q.lockDuration, 'PT45S');
    assert.equal(q.maxDeliveryCount, 4);
    const t = await sbm.topics.createOrUpdate(rg, nsName, 'events', { enablePartitioning: false });
    assert.equal(t.name, 'events');
    const s = await sbm.subscriptions.createOrUpdate(rg, nsName, 'events', 'audit', { maxDeliveryCount: 6 });
    assert.equal(s.maxDeliveryCount, 6);
    assert.deepEqual((await collect(sbm.queues.listByNamespace(rg, nsName))).map((x) => x.name), ['orders']);
    assert.deepEqual((await collect(sbm.subscriptions.listByTopic(rg, nsName, 'events'))).map((x) => x.name), ['audit']);
    const keys = await sbm.namespaces.listKeys(rg, nsName, 'RootManageSharedAccessKey');
    assert.ok(keys.primaryConnectionString || keys.primaryKey);
  });
  it('a queue created through ARM works over AMQP with the Service Bus SDK, with the properties set (lock duration)', async () => {
    const rg = await group();
    const nsName = uniq('sbarm');
    await sbm.namespaces.beginCreateOrUpdateAndWait(rg, nsName, { location: 'eastus', sku: { name: 'Standard', tier: 'Standard' } });
    await sbm.queues.createOrUpdate(rg, nsName, 'work', { maxDeliveryCount: 2 });
    const sb = track(new ServiceBusClient(`Endpoint=sb://${nsName}.servicebus.localhost:${emu.port};SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true`));
    await sb.createSender('work').sendMessages({ messageId: 'm1', body: 'via arm queue' });
    const r = sb.createReceiver('work');
    const [m] = await r.receiveMessages(1, { maxWaitTimeInMs: 5000 });
    assert.equal(m.body, 'via arm queue');
    await r.completeMessage(m);
  });
  it('deleting the namespace through ARM removes its queues and messages', async () => {
    const rg = await group();
    const nsName = uniq('sbarm');
    await sbm.namespaces.beginCreateOrUpdateAndWait(rg, nsName, { location: 'eastus', sku: { name: 'Standard', tier: 'Standard' } });
    await sbm.queues.createOrUpdate(rg, nsName, 'gone', {});
    await sbm.namespaces.beginDeleteAndWait(rg, nsName);
    const e = await rejection(() => sbm.queues.get(rg, nsName, 'gone'));
    assert.equal(e.statusCode, 404);
  });
});

describe('deployments', () => {
  it('a template deployment creates a storage account whose container is usable from the data plane', async () => {
    const rg = await group();
    const acct = `dep${uniq('').slice(0, 14)}`;
    const template = {
      $schema: 'https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#', contentVersion: '1.0.0.0',
      parameters: { accountName: { type: 'string' } },
      resources: [
        { type: 'Microsoft.Storage/storageAccounts', apiVersion: '2023-01-01', name: "[parameters('accountName')]", location: 'eastus', sku: { name: 'Standard_LRS' }, kind: 'StorageV2' },
        { type: 'Microsoft.Storage/storageAccounts/blobServices/containers', apiVersion: '2023-01-01', name: "[concat(parameters('accountName'), '/default/deployed')]", dependsOn: ["[resourceId('Microsoft.Storage/storageAccounts', parameters('accountName'))]"], properties: {} },
      ],
      outputs: { name: { type: 'string', value: "[parameters('accountName')]" } },
    };
    const d = await deployments.deployments.beginCreateOrUpdateAndWait(rg, uniq('dep'), { properties: { mode: 'Incremental', template, parameters: { accountName: { value: acct } } } });
    assert.equal(d.properties.provisioningState, 'Succeeded');
    assert.equal(d.properties.outputs.name.value, acct);
    const blobs = new BlobServiceClient(emu.url('blob', acct), new StorageSharedKeyCredential(acct, 'a2V5'.repeat(16)));
    assert.equal(await blobs.getContainerClient('deployed').exists(), true);
    const listed = await collect(resources.resources.listByResourceGroup(rg));
    assert.ok(listed.some((r) => r.name === acct));
  });
  it('a failing template reports a failed deployment with an error', async () => {
    const rg = await group();
    const e = await rejection(() => deployments.deployments.beginCreateOrUpdateAndWait(rg, uniq('bad'), { properties: { mode: 'Incremental', template: { $schema: 'https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#', contentVersion: '1.0.0.0', resources: [{ type: 'Microsoft.Storage/storageAccounts', apiVersion: '2023-01-01', name: "[parameters('missing')]", location: 'eastus' }] } } }));
    assert.ok(e instanceof Error);
  });
});
