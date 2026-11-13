// How the JavaScript stack uses HTTP: keep-alive connection pools, many parallel requests, big bodies, aborted transfers.
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { randomBytes, createHash } from 'node:crypto';
import { Readable } from 'node:stream';
import { BlobServiceClient, StorageSharedKeyCredential } from '@azure/storage-blob';
import { TableClient, AzureNamedKeyCredential } from '@azure/data-tables';
import { QueueServiceClient } from '@azure/storage-queue';
import { env, uniq, DEV_ACCOUNT, DEV_KEY, streamToBuffer, rejection, collect } from '../lib/emu.mjs';

const emu = env();
const cred = new StorageSharedKeyCredential(DEV_ACCOUNT, DEV_KEY);
const svc = new BlobServiceClient(emu.url('blob', DEV_ACCOUNT), cred);
const sha = (b) => createHash('sha256').update(b).digest('hex');

describe('parallel requests and big bodies', () => {
  it('200 blob uploads in flight at once all succeed and read back their own content', async () => {
    const c = svc.getContainerClient(uniq('par'));
    await c.create();
    await Promise.all(Array.from({ length: 200 }, (_, i) => c.getBlockBlobClient(`b${i}`).upload(`content-${i}`, `content-${i}`.length)));
    const back = await Promise.all(Array.from({ length: 200 }, async (_, i) => (await streamToBuffer((await c.getBlockBlobClient(`b${i}`).download()).readableStreamBody)).toString()));
    assert.deepEqual(back, Array.from({ length: 200 }, (_, i) => `content-${i}`));
    assert.equal((await collect(c.listBlobsFlat())).length, 200);
  });
  it('a 48 MiB blob through uploadStream (8 MiB blocks, 6 in flight) and downloadToBuffer (parallel ranges) keeps the SHA-256', async () => {
    const c = svc.getContainerClient(uniq('big'));
    await c.create();
    const data = randomBytes(48 * 1024 * 1024);
    const b = c.getBlockBlobClient('big.bin');
    await b.uploadStream(Readable.from((function* () { for (let o = 0; o < data.length; o += 3_000_000) yield data.subarray(o, o + 3_000_000); })()), 8 * 1024 * 1024, 6);
    assert.equal((await b.getProperties()).contentLength, data.length);
    const back = await b.downloadToBuffer(0, undefined, { blockSize: 4 * 1024 * 1024, concurrency: 6 });
    assert.equal(sha(back), sha(data));
  });
  it('many table inserts and queries run over one keep-alive pool', async () => {
    const t = new TableClient(emu.url('table', DEV_ACCOUNT), uniq('tp'), new AzureNamedKeyCredential(DEV_ACCOUNT, DEV_KEY), { allowInsecureConnection: true });
    await t.createTable();
    await Promise.all(Array.from({ length: 100 }, (_, i) => t.createEntity({ partitionKey: `p${i % 5}`, rowKey: String(i), n: i })));
    const all = await collect(t.listEntities());
    assert.equal(all.length, 100);
    const sums = await Promise.all([0, 1, 2, 3, 4].map(async (p) => (await collect(t.listEntities({ queryOptions: { filter: `PartitionKey eq 'p${p}'` } }))).length));
    assert.deepEqual(sums, [20, 20, 20, 20, 20]);
  });
  it('50 queue senders and 50 receivers in parallel deliver every message exactly once', async () => {
    const q = new QueueServiceClient(emu.url('queue', DEV_ACCOUNT), cred).getQueueClient(uniq('qp'));
    await q.create();
    await Promise.all(Array.from({ length: 50 }, (_, i) => q.sendMessage(`m${i}`)));
    const got = (await Promise.all(Array.from({ length: 50 }, () => q.receiveMessages({ numberOfMessages: 1, visibilityTimeout: 60 })))).flatMap((r) => r.receivedMessageItems.map((m) => m.messageText));
    assert.equal(got.length, 50);
    assert.equal(new Set(got).size, 50);
  });
});

describe('aborted and failing transfers', () => {
  it('aborting a download in the middle leaves the emulator usable for the next request', async () => {
    const c = svc.getContainerClient(uniq('abort'));
    await c.create();
    const b = c.getBlockBlobClient('x');
    await b.uploadData(randomBytes(20 * 1024 * 1024));
    const ac = new AbortController();
    const d = await b.download(0, undefined, { abortSignal: ac.signal });
    let n = 0;
    const err = await rejection(async () => {
      for await (const chunk of d.readableStreamBody) { n += chunk.length; if (n > 1_000_000) ac.abort(); }
    });
    assert.ok(err);
    assert.equal((await b.getProperties()).contentLength, 20 * 1024 * 1024);
  });
  it('a request body shorter than its Content-Length does not wedge the server (raw socket)', async () => {
    const c = svc.getContainerClient(uniq('raw'));
    await c.create();
    const net = await import('node:net');
    await new Promise((resolve) => {
      const s = net.connect(emu.port, '127.0.0.1', () => {
        s.write(`PUT /${c.containerName}/short HTTP/1.1\r\nHost: ${DEV_ACCOUNT}.blob.core.localhost:${emu.port}\r\nx-ms-blob-type: BlockBlob\r\nContent-Length: 1000\r\n\r\nonly 10 b`);
        setTimeout(() => { s.destroy(); resolve(); }, 300);
      });
    });
    assert.equal(await c.getBlockBlobClient('short').exists(), false);
    await c.getBlockBlobClient('ok').upload('fine', 4);
  });
  it('upload with Transfer-Encoding: chunked (no Content-Length) gets a clear answer, not a hang (raw)', async () => {
    const c = svc.getContainerClient(uniq('chunk'));
    await c.create();
    const net = await import('node:net');
    const reply = await new Promise((resolve) => {
      const chunks = [];
      const s = net.connect(emu.port, '127.0.0.1', () => {
        s.write(`PUT /${c.containerName}/chunked HTTP/1.1\r\nHost: ${DEV_ACCOUNT}.blob.core.localhost:${emu.port}\r\nx-ms-blob-type: BlockBlob\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n`);
      });
      s.on('data', (d) => chunks.push(d));
      s.on('close', () => resolve(Buffer.concat(chunks).toString().split('\r\n')[0]));
      setTimeout(() => { s.destroy(); }, 3000);
    });
    // Without a signature the answer is 401/403; what matters is that a status line comes back
    assert.match(reply, /^HTTP\/1\.1 \d{3}/);
  });
});
