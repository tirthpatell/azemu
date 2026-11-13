// Azurite style: pinned ports, path-style URLs (http://127.0.0.1:{port}/devstoreaccount1) and connection strings. Uses a second emulator
// with --pin, sharing the certificate directory of the first so NODE_EXTRA_CA_CERTS still works (Cosmos on a pinned port is https).
import { describe, it, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { BlobServiceClient } from '@azure/storage-blob';
import { QueueServiceClient } from '@azure/storage-queue';
import { TableClient, TableServiceClient } from '@azure/data-tables';
import { DataLakeServiceClient, StorageSharedKeyCredential } from '@azure/storage-file-datalake';
import { CosmosClient } from '@azure/cosmos';
import { startEmulator, freePort, uniq, DEV_ACCOUNT, DEV_KEY, collect, streamToBuffer } from '../lib/emu.mjs';

let emu;
const pins = {};
before(async () => {
  for (const s of ['blob', 'queue', 'table', 'dfs', 'cosmos']) pins[s] = await freePort();
  emu = await startEmulator({ pins, certDir: process.env.AZEMU_CERT_DIR });
});
after(async () => { await emu?.stop(); });

const cs = () => `DefaultEndpointsProtocol=http;AccountName=${DEV_ACCOUNT};AccountKey=${DEV_KEY};BlobEndpoint=http://127.0.0.1:${pins.blob}/${DEV_ACCOUNT};QueueEndpoint=http://127.0.0.1:${pins.queue}/${DEV_ACCOUNT};TableEndpoint=http://127.0.0.1:${pins.table}/${DEV_ACCOUNT};`;

describe('Azurite style connection strings against pinned ports', () => {
  it('UseDevelopmentStorage=true makes the SDK clients target 127.0.0.1:10000/10001/10002 with the development account (URL only, those ports are fixed)', () => {
    assert.equal(BlobServiceClient.fromConnectionString('UseDevelopmentStorage=true').url, 'http://127.0.0.1:10000/devstoreaccount1');
    assert.equal(QueueServiceClient.fromConnectionString('UseDevelopmentStorage=true').url, 'http://127.0.0.1:10001/devstoreaccount1');
    assert.equal(TableServiceClient.fromConnectionString('UseDevelopmentStorage=true', { allowInsecureConnection: true }).url, 'http://127.0.0.1:10002/devstoreaccount1');
  });
  it('blob: the equivalent explicit connection string on a pinned port works (container, upload, download, list)', async () => {
    const svc = BlobServiceClient.fromConnectionString(cs());
    assert.equal(svc.accountName, DEV_ACCOUNT);
    const c = svc.getContainerClient(uniq('c'));
    await c.create();
    await c.getBlockBlobClient('a b/ü.txt').upload('path style', 10);
    assert.equal((await streamToBuffer((await c.getBlockBlobClient('a b/ü.txt').download()).readableStreamBody)).toString(), 'path style');
    assert.deepEqual((await collect(c.listBlobsFlat())).map((b) => b.name), ['a b/ü.txt']);
  });
  it('blob: a SAS made for the path-style URL works on the pinned port', async () => {
    const { generateBlobSASQueryParameters, BlobSASPermissions, BlobClient, StorageSharedKeyCredential: Key } = await import('@azure/storage-blob');
    const svc = BlobServiceClient.fromConnectionString(cs());
    const c = svc.getContainerClient(uniq('c'));
    await c.create();
    await c.getBlockBlobClient('s').upload('sas', 3);
    const sas = generateBlobSASQueryParameters({ containerName: c.containerName, blobName: 's', permissions: BlobSASPermissions.parse('r'), expiresOn: new Date(Date.now() + 3600_000) }, new Key(DEV_ACCOUNT, DEV_KEY)).toString();
    const r = await new BlobClient(`${c.getBlobClient('s').url}?${sas}`).download();
    assert.equal((await streamToBuffer(r.readableStreamBody)).toString(), 'sas');
  });
  it('queue: the pinned port with a path-style connection string', async () => {
    const q = QueueServiceClient.fromConnectionString(cs()).getQueueClient(uniq('q'));
    await q.create();
    await q.sendMessage('hi');
    assert.equal((await q.receiveMessages()).receivedMessageItems[0].messageText, 'hi');
  });
  it('table: the pinned port with a path-style connection string', async () => {
    const t = TableClient.fromConnectionString(cs(), uniq('t'), { allowInsecureConnection: true });
    await t.createTable();
    await t.createEntity({ partitionKey: 'p', rowKey: 'r', v: 1 });
    assert.equal((await t.getEntity('p', 'r')).v, 1);
    assert.equal((await collect(t.listEntities())).length, 1);
  });
  it('the data lake SDK on a pinned dfs port in path-style form (the SDK keeps the same address for the blob calls)', async () => {
    const s = new DataLakeServiceClient(`http://127.0.0.1:${pins.dfs}/${DEV_ACCOUNT}`, new StorageSharedKeyCredential(DEV_ACCOUNT, DEV_KEY));
    const fs = s.getFileSystemClient(uniq('fs'));
    await fs.create();
    await fs.getFileClient('d/f.txt').upload(Buffer.from('pinned dfs'));
    assert.equal((await streamToBuffer((await fs.getFileClient('d/f.txt').read()).readableStreamBody)).toString(), 'pinned dfs');
  });
  it('cosmos on a pinned port works like the Cosmos DB emulator drop-in (https://localhost:port, well-known key)', async () => {
    const client = new CosmosClient({ endpoint: `https://localhost:${pins.cosmos}`, key: 'C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==' });
    const { database } = await client.databases.create({ id: uniq('db') });
    const { container } = await database.containers.create({ id: 'c', partitionKey: { paths: ['/pk'] } });
    await container.items.create({ id: '1', pk: 'a', v: 1 });
    assert.equal((await container.item('1', 'a').read()).resource.v, 1);
    assert.deepEqual((await container.items.query('SELECT VALUE c.v FROM c').fetchAll()).resources, [1]);
  });
  it('the gateway and a pinned port share state: a container made on the pinned port is visible on the gateway host', async () => {
    const pinned = BlobServiceClient.fromConnectionString(cs());
    const name = uniq('shared');
    await pinned.getContainerClient(name).create();
    const gw = new BlobServiceClient(emu.url('blob', DEV_ACCOUNT), new StorageSharedKeyCredential(DEV_ACCOUNT, DEV_KEY));
    assert.equal(await gw.getContainerClient(name).exists(), true);
  });
});
