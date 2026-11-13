import { describe, it, before } from 'node:test';
import assert from 'node:assert/strict';
import { Readable } from 'node:stream';
import { writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { randomBytes, createHash } from 'node:crypto';
import {
  BlobServiceClient, StorageSharedKeyCredential, BlobSASPermissions, ContainerSASPermissions, AccountSASPermissions,
  AccountSASServices, AccountSASResourceTypes, SASProtocol, generateBlobSASQueryParameters, generateAccountSASQueryParameters,
  BlobBatchClient, ContainerClient, BlobClient, BlockBlobClient,
} from '@azure/storage-blob';
import { env, uniq, DEV_ACCOUNT, DEV_KEY, collect, streamToBuffer, rejection, sleep, fakeCredential, accountWithVersioning } from '../lib/emu.mjs';

const emu = env();
const cred = new StorageSharedKeyCredential(DEV_ACCOUNT, DEV_KEY);
const svc = new BlobServiceClient(emu.url('blob', DEV_ACCOUNT), cred);

async function newContainer(prefix = 'c') {
  const c = svc.getContainerClient(uniq(prefix));
  await c.create();
  return c;
}
async function text(resp) {
  return (await streamToBuffer(resp.readableStreamBody)).toString();
}
const code = (e) => e.details?.errorCode ?? e.code;

describe('containers', () => {
  it('create returns 201 and a second create is ContainerAlreadyExists 409', async () => {
    const c = svc.getContainerClient(uniq('c'));
    const r = await c.create();
    assert.equal(r._response.status, 201);
    assert.ok(r.etag);
    const e = await rejection(() => c.create());
    assert.equal(e.statusCode, 409);
    assert.equal(code(e), 'ContainerAlreadyExists');
  });
  it('createIfNotExists reports succeeded only the first time', async () => {
    const c = svc.getContainerClient(uniq('c'));
    assert.equal((await c.createIfNotExists()).succeeded, true);
    assert.equal((await c.createIfNotExists()).succeeded, false);
  });
  it('exists is false for a missing container and true after create', async () => {
    const c = svc.getContainerClient(uniq('c'));
    assert.equal(await c.exists(), false);
    await c.create();
    assert.equal(await c.exists(), true);
  });
  it('metadata set at creation comes back from getProperties', async () => {
    const c = svc.getContainerClient(uniq('c'));
    await c.create({ metadata: { Foo: 'bar', env: 'test' }, access: 'blob' });
    const p = await c.getProperties();
    assert.equal(p.metadata.foo, 'bar');
    assert.equal(p.metadata.env, 'test');
    assert.equal(p.blobPublicAccess, 'blob');
  });
  it('setMetadata replaces all metadata', async () => {
    const c = await newContainer();
    await c.setMetadata({ a: '1' });
    await c.setMetadata({ b: '2' });
    const p = await c.getProperties();
    assert.deepEqual({ ...p.metadata }, { b: '2' });
  });
  it('list containers filters by prefix and pages with continuation', async () => {
    const pre = uniq('lst');
    for (let i = 0; i < 5; i++) await svc.getContainerClient(`${pre}${i}`).create();
    const names = [];
    for await (const page of svc.listContainers({ prefix: pre }).byPage({ maxPageSize: 2 })) {
      assert.ok(page.containerItems.length <= 2);
      names.push(...page.containerItems.map((x) => x.name));
    }
    assert.deepEqual(names, [0, 1, 2, 3, 4].map((i) => `${pre}${i}`));
  });
  it('list containers includes metadata when asked', async () => {
    const pre = uniq('lm');
    await svc.getContainerClient(pre).create({ metadata: { k: 'v' } });
    const items = await collect(svc.listContainers({ prefix: pre, includeMetadata: true }));
    assert.equal(items.length, 1);
    assert.equal(items[0].properties.etag !== undefined, true);
    assert.equal(items[0].metadata.k, 'v');
  });
  it('delete removes the container and a second delete is ContainerNotFound 404', async () => {
    const c = await newContainer();
    await c.delete();
    const e = await rejection(() => c.delete());
    assert.equal(e.statusCode, 404);
    assert.equal(code(e), 'ContainerNotFound');
    assert.equal((await c.deleteIfExists()).succeeded, false);
  });
  it('a blob call on a missing container is ContainerNotFound', async () => {
    const c = svc.getContainerClient(uniq('nope'));
    const e = await rejection(() => c.getBlockBlobClient('x').upload('a', 1));
    assert.equal(e.statusCode, 404);
    assert.equal(code(e), 'ContainerNotFound');
  });
  it('container lease can be acquired, renewed, changed and released', async () => {
    const c = await newContainer();
    const l = c.getBlobLeaseClient();
    const a = await l.acquireLease(30);
    assert.ok(a.leaseId);
    await l.renewLease();
    const newId = '11111111-1111-1111-1111-111111111111';
    const ch = await l.changeLease(newId);
    assert.equal(ch.leaseId, newId);
    const e = await rejection(() => c.delete());
    assert.equal(e.statusCode, 412);
    assert.equal(code(e), 'LeaseIdMissing');
    await l.releaseLease();
    await c.delete();
  });
  it('stored access policy round trips through getAccessPolicy', async () => {
    const c = await newContainer();
    const start = new Date(Date.now() - 60_000);
    const expiry = new Date(Date.now() + 3600_000);
    await c.setAccessPolicy(undefined, [{ id: 'p1', accessPolicy: { startsOn: start, expiresOn: expiry, permissions: 'rl' } }]);
    const r = await c.getAccessPolicy();
    assert.equal(r.signedIdentifiers.length, 1);
    assert.equal(r.signedIdentifiers[0].id, 'p1');
    assert.equal(r.signedIdentifiers[0].accessPolicy.permissions, 'rl');
  });
});

describe('block blobs', () => {
  it('upload then download returns the same bytes and properties', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('hello.txt');
    const u = await b.upload('hello world', 11, { blobHTTPHeaders: { blobContentType: 'text/plain', blobCacheControl: 'no-cache' } });
    assert.equal(u._response.status, 201);
    assert.ok(u.etag && u.lastModified && u.requestId);
    const d = await b.download();
    assert.equal(await text(d), 'hello world');
    assert.equal(d.contentType, 'text/plain');
    assert.equal(d.cacheControl, 'no-cache');
    assert.equal(d.contentLength, 11);
    assert.equal(d.blobType, 'BlockBlob');
  });
  it('download with a range returns 206 and the slice', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('r');
    await b.upload('0123456789', 10);
    const d = await b.download(2, 4);
    assert.equal(d._response.status, 206);
    assert.equal(await text(d), '2345');
    assert.equal(d.contentRange, 'bytes 2-5/10');
    const tail = await b.download(7);
    assert.equal(await text(tail), '789');
  });
  it('range past the end is InvalidRange 416', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('r');
    await b.upload('abc', 3);
    const e = await rejection(() => b.download(10, 5));
    assert.equal(e.statusCode, 416);
    assert.equal(code(e), 'InvalidRange');
  });
  it('uploadData with a buffer and downloadToBuffer round trip', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('data.bin');
    const data = randomBytes(300_000);
    await b.uploadData(data);
    const back = await b.downloadToBuffer();
    assert.ok(back.equals(data));
  });
  it('uploadData splits a large buffer into parallel blocks that commit to the right bytes', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('big.bin');
    const data = randomBytes(9 * 1024 * 1024 + 123);
    await b.uploadData(data, { maxSingleShotSize: 1024 * 1024, blockSize: 1024 * 1024, concurrency: 4 });
    const bl = await b.getBlockList('committed');
    assert.equal(bl.committedBlocks.length, 10);
    const back = await b.downloadToBuffer();
    assert.equal(createHash('md5').update(back).digest('hex'), createHash('md5').update(data).digest('hex'));
  });
  it('uploadStream with 4 buffers in flight stores every byte in order', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('stream.bin');
    const data = randomBytes(5 * 1024 * 1024 + 17);
    const r = await b.uploadStream(Readable.from([data.subarray(0, 2_000_000), data.subarray(2_000_000)]), 1024 * 1024, 4);
    assert.ok(r.etag);
    assert.ok((await b.downloadToBuffer()).equals(data));
  });
  it('uploadFile uploads a file in parallel blocks', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'azjs-'));
    try {
      const f = join(dir, 'f.bin');
      const data = randomBytes(6 * 1024 * 1024 + 1);
      writeFileSync(f, data);
      const c = await newContainer();
      const b = c.getBlockBlobClient('file.bin');
      await b.uploadFile(f, { maxSingleShotSize: 1024 * 1024, blockSize: 1024 * 1024, concurrency: 5 });
      const p = await b.getProperties();
      assert.equal(p.contentLength, data.length);
      assert.ok((await b.downloadToBuffer()).equals(data));
      const out = join(dir, 'out.bin');
      await b.downloadToFile(out);
      const { readFileSync } = await import('node:fs');
      assert.ok(readFileSync(out).equals(data));
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
  it('stageBlock and commitBlockList build a blob and getBlockList shows uncommitted then committed', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('blocks');
    const id = (n) => Buffer.from(String(n).padStart(6, '0')).toString('base64');
    await b.stageBlock(id(1), 'AAA', 3);
    await b.stageBlock(id(2), 'BBB', 3);
    const u = await b.getBlockList('uncommitted');
    assert.deepEqual(u.uncommittedBlocks.map((x) => x.name), [id(1), id(2)]);
    await b.commitBlockList([id(2), id(1)]);
    assert.equal(await text(await b.download()), 'BBBAAA');
    const cm = await b.getBlockList('committed');
    assert.deepEqual(cm.committedBlocks.map((x) => x.size), [3, 3]);
  });
  it('properties report size, type, etag and the metadata set on upload', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('p');
    await b.upload('abcd', 4, { metadata: { Owner: 'me', two: '2' } });
    const p = await b.getProperties();
    assert.equal(p.contentLength, 4);
    assert.equal(p.blobType, 'BlockBlob');
    assert.equal(p.metadata.owner, 'me');
    assert.equal(p.metadata.two, '2');
    assert.ok(p.etag.startsWith('"') || p.etag.startsWith('0x'));
    assert.equal(p.accessTier, 'Hot');
    assert.equal(p.leaseStatus, 'unlocked');
  });
  it('setMetadata, setHTTPHeaders change properties and the etag', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('m');
    await b.upload('x', 1);
    const e0 = (await b.getProperties()).etag;
    await b.setMetadata({ a: 'b' });
    await b.setHTTPHeaders({ blobContentType: 'application/json', blobContentLanguage: 'en' });
    const p = await b.getProperties();
    assert.equal(p.metadata.a, 'b');
    assert.equal(p.contentType, 'application/json');
    assert.equal(p.contentLanguage, 'en');
    assert.notEqual(p.etag, e0);
  });
  it('tags can be set, read and used to find blobs', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('t');
    const marker = uniq('v');
    await b.upload('x', 1, { tags: { project: marker, tier: 'gold' } });
    const t = await b.getTags();
    assert.deepEqual({ ...t.tags }, { project: marker, tier: 'gold' });
    await b.setTags({ project: marker, tier: 'silver' });
    assert.equal((await b.getTags()).tags.tier, 'silver');
    const found = await collect(svc.findBlobsByTags(`project = '${marker}'`));
    assert.equal(found.length, 1);
    assert.equal(found[0].name, 't');
    assert.equal(found[0].containerName, c.containerName);
  });
  it('delete removes a blob; getProperties then is BlobNotFound 404 and exists is false', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('d');
    await b.upload('x', 1);
    assert.equal((await b.delete())._response.status, 202);
    const e = await rejection(() => b.getProperties());
    assert.equal(e.statusCode, 404);
    assert.equal(await b.exists(), false);
    assert.equal((await b.deleteIfExists()).succeeded, false);
  });
  it('soft delete keeps a deleted blob and undelete brings it back', async () => {
    const acct = uniq('sd');
    const s = new BlobServiceClient(emu.url('blob', acct), new StorageSharedKeyCredential(acct, DEV_KEY));
    await s.setProperties({ deleteRetentionPolicy: { enabled: true, days: 3 } });
    const pr = await s.getProperties();
    assert.equal(pr.deleteRetentionPolicy.enabled, true);
    assert.equal(pr.deleteRetentionPolicy.days, 3);
    const c = s.getContainerClient(uniq('c'));
    await c.create();
    const b = c.getBlockBlobClient('gone');
    await b.upload('data', 4);
    await b.delete();
    assert.equal((await collect(c.listBlobsFlat())).length, 0);
    const del = await collect(c.listBlobsFlat({ includeDeleted: true }));
    assert.equal(del.length, 1);
    assert.equal(del[0].deleted, true);
    await b.undelete();
    assert.equal(await text(await b.download()), 'data');
  });
  it('overwriting a blob replaces its content', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('o');
    await b.upload('one', 3);
    await b.upload('three', 5);
    assert.equal(await text(await b.download()), 'three');
  });
  it('uploads an empty blob', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('empty');
    await b.upload('', 0);
    const p = await b.getProperties();
    assert.equal(p.contentLength, 0);
  });
  it('content-md5 sent by the client is checked: a wrong MD5 is Md5Mismatch 400', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('md5');
    const e = await rejection(() => b.upload('abc', 3, { transactionalContentMD5: createHash('md5').update('zzz').digest() }));
    assert.equal(e.statusCode, 400);
    assert.ok(['Md5Mismatch', 'MD5Mismatch'].includes(code(e)));
  });
  it('tier can be set to Cool and read back', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('tier');
    await b.upload('x', 1);
    await b.setAccessTier('Cool');
    const p = await b.getProperties();
    assert.equal(p.accessTier, 'Cool');
  });
});

describe('listing', () => {
  it('flat listing with prefix returns names in order and pages with maxPageSize', async () => {
    const c = await newContainer();
    for (const n of ['a/1', 'a/2', 'a/3', 'b/1', 'c']) await c.getBlockBlobClient(n).upload('x', 1);
    const all = (await collect(c.listBlobsFlat())).map((x) => x.name);
    assert.deepEqual(all, ['a/1', 'a/2', 'a/3', 'b/1', 'c']);
    const pre = (await collect(c.listBlobsFlat({ prefix: 'a/' }))).map((x) => x.name);
    assert.deepEqual(pre, ['a/1', 'a/2', 'a/3']);
    const pages = [];
    let marker;
    for await (const p of c.listBlobsFlat().byPage({ maxPageSize: 2 })) {
      pages.push(p.segment.blobItems.length);
      marker = p.continuationToken;
    }
    assert.deepEqual(pages, [2, 2, 1]);
    assert.ok(marker === undefined || marker === '');
  });
  it('a continuation token resumes where the page stopped', async () => {
    const c = await newContainer();
    for (let i = 0; i < 5; i++) await c.getBlockBlobClient(`n${i}`).upload('x', 1);
    const it = c.listBlobsFlat().byPage({ maxPageSize: 2 });
    const first = await it.next();
    const token = first.value.continuationToken;
    assert.ok(token);
    const rest = await collect(c.listBlobsFlat().byPage({ continuationToken: token, maxPageSize: 10 }));
    assert.deepEqual(rest[0].segment.blobItems.map((x) => x.name), ['n2', 'n3', 'n4']);
  });
  it('hierarchy listing with a delimiter returns prefixes and blobs', async () => {
    const c = await newContainer();
    for (const n of ['root.txt', 'dir1/a', 'dir1/b', 'dir2/sub/c', 'dir2/d']) await c.getBlockBlobClient(n).upload('x', 1);
    const items = await collect(c.listBlobsByHierarchy('/'));
    const got = items.map((x) => `${x.kind}:${x.name}`);
    assert.deepEqual(got, ['prefix:dir1/', 'prefix:dir2/', 'blob:root.txt']);
    const sub = await collect(c.listBlobsByHierarchy('/', { prefix: 'dir2/' }));
    // the SDK yields the prefixes of a page before its blobs
    assert.deepEqual(sub.map((x) => `${x.kind}:${x.name}`), ['prefix:dir2/sub/', 'blob:dir2/d']);
  });
  it('listing can include metadata, tags and snapshots', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('x');
    await b.upload('v1', 2, { metadata: { m: '1' }, tags: { t: 'a' } });
    await b.createSnapshot();
    const items = await collect(c.listBlobsFlat({ includeMetadata: true, includeTags: true, includeSnapshots: true }));
    assert.equal(items.length, 2);
    const base = items.find((x) => !x.snapshot);
    assert.equal(base.metadata.m, '1');
    assert.equal(base.tags.t, 'a');
    assert.ok(items.find((x) => x.snapshot));
  });
  it('empty container lists nothing', async () => {
    const c = await newContainer();
    assert.deepEqual(await collect(c.listBlobsFlat()), []);
  });
});

describe('leases', () => {
  it('writing a leased blob without the lease id is LeaseIdMissing 412, with it succeeds', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('l');
    await b.upload('v1', 2);
    const l = b.getBlobLeaseClient();
    const a = await l.acquireLease(60);
    const e = await rejection(() => b.upload('v2', 2));
    assert.equal(e.statusCode, 412);
    assert.equal(code(e), 'LeaseIdMissing');
    await b.upload('v2', 2, { conditions: { leaseId: a.leaseId } });
    const p = await b.getProperties();
    assert.equal(p.leaseState, 'leased');
    assert.equal(p.leaseStatus, 'locked');
    assert.equal(p.leaseDuration, 'fixed');
  });
  it('infinite lease (-1) and release', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('l');
    await b.upload('v1', 2);
    const l = b.getBlobLeaseClient();
    await l.acquireLease(-1);
    assert.equal((await b.getProperties()).leaseDuration, 'infinite');
    await l.releaseLease();
    assert.equal((await b.getProperties()).leaseState, 'available');
  });
  it('break lease with a period leaves the blob in the breaking state', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('l');
    await b.upload('v1', 2);
    const l = b.getBlobLeaseClient();
    await l.acquireLease(60);
    const br = await l.breakLease(30);
    assert.ok(br.leaseTime >= 0 && br.leaseTime <= 30);
    assert.equal((await b.getProperties()).leaseState, 'breaking');
  });
  it('acquiring a lease on a blob that is already leased is LeaseAlreadyPresent 409', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('l');
    await b.upload('v1', 2);
    await b.getBlobLeaseClient().acquireLease(60);
    const e = await rejection(() => b.getBlobLeaseClient().acquireLease(60));
    assert.equal(e.statusCode, 409);
    assert.equal(code(e), 'LeaseAlreadyPresent');
  });
});

describe('copy, snapshots, versions', () => {
  it('beginCopyFromURL within the account copies content and metadata and completes', async () => {
    const c = await newContainer();
    const src = c.getBlockBlobClient('src');
    await src.upload('copy me', 7, { metadata: { a: '1' } });
    const dst = c.getBlockBlobClient('dst');
    const poller = await dst.beginCopyFromURL(src.url);
    const r = await poller.pollUntilDone();
    assert.equal(r.copyStatus, 'success');
    assert.equal(await text(await dst.download()), 'copy me');
    assert.equal((await dst.getProperties()).metadata.a, '1');
  });
  it('syncCopyFromURL with a SAS on the source copies the content', async () => {
    const c = await newContainer();
    const src = c.getBlockBlobClient('src');
    await src.upload('sync copy', 9);
    const sas = generateBlobSASQueryParameters({ containerName: c.containerName, blobName: 'src', permissions: BlobSASPermissions.parse('r'), expiresOn: new Date(Date.now() + 3600_000) }, cred).toString();
    const dst = c.getBlockBlobClient('dst');
    const r = await dst.syncCopyFromURL(`${src.url}?${sas}`);
    assert.equal(r.copyStatus, 'success');
    assert.equal(await text(await dst.download()), 'sync copy');
  });
  it('copy from a source that does not exist is 404', async () => {
    const c = await newContainer();
    const e = await rejection(async () => {
      const p = await c.getBlockBlobClient('dst').beginCopyFromURL(c.getBlockBlobClient('missing').url);
      await p.pollUntilDone();
    });
    assert.equal(e.statusCode, 404);
  });
  it('createSnapshot keeps the old content readable through withSnapshot', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('s');
    await b.upload('old', 3);
    const snap = await b.createSnapshot({ metadata: { s: '1' } });
    assert.ok(snap.snapshot);
    await b.upload('newer', 5);
    assert.equal(await text(await b.withSnapshot(snap.snapshot).download()), 'old');
    assert.equal(await text(await b.download()), 'newer');
    await b.withSnapshot(snap.snapshot).delete();
    const e = await rejection(() => b.withSnapshot(snap.snapshot).getProperties());
    assert.equal(e.statusCode, 404);
  });
  it('deleting a blob with snapshots needs deleteSnapshots (SnapshotsPresent 409)', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('s');
    await b.upload('x', 1);
    await b.createSnapshot();
    const e = await rejection(() => b.delete());
    assert.equal(e.statusCode, 409);
    assert.equal(code(e), 'SnapshotsPresent');
    await b.delete({ deleteSnapshots: 'include' });
    assert.equal((await collect(c.listBlobsFlat({ includeSnapshots: true }))).length, 0);
  });
  it('an account without versioning (the default, as on Azure) keeps no versions', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('plain');
    const u1 = await b.upload('one', 3);
    assert.equal(u1.versionId, undefined);
    await b.upload('two', 3);
    assert.equal((await collect(c.listBlobsFlat({ includeVersions: true }))).length, 1);
    await b.delete();
    assert.equal((await collect(c.listBlobsFlat({ includeVersions: true }))).length, 0);
  });
  it('every overwrite makes a version that can be read by versionId (versioning switched on through ARM)', async () => {
    const account = await accountWithVersioning(emu);
    const c = new BlobServiceClient(emu.url('blob', account), new StorageSharedKeyCredential(account, DEV_KEY)).getContainerClient(uniq('c'));
    await c.create();
    const b = c.getBlockBlobClient('v');
    const u1 = await b.upload('one', 3);
    assert.ok(u1.versionId);
    const u2 = await b.upload('two', 3);
    assert.notEqual(u1.versionId, u2.versionId);
    assert.equal(await text(await b.withVersion(u1.versionId).download()), 'one');
    const vs = await collect(c.listBlobsFlat({ includeVersions: true }));
    assert.equal(vs.length, 2);
    assert.equal(vs.filter((x) => x.isCurrentVersion).length, 1);
    await b.withVersion(u1.versionId).delete();
    assert.equal((await collect(c.listBlobsFlat({ includeVersions: true }))).length, 1);
  });
});

describe('append and page blobs', () => {
  it('append blob appendBlock returns offsets and the content is the concatenation', async () => {
    const c = await newContainer();
    const b = c.getAppendBlobClient('a');
    await b.create();
    const r1 = await b.appendBlock('abc', 3);
    const r2 = await b.appendBlock('def', 3);
    assert.equal(r1.blobAppendOffset, '0');
    assert.equal(r2.blobAppendOffset, '3');
    assert.equal(r2.blobCommittedBlockCount, 2);
    assert.equal(await text(await b.download()), 'abcdef');
    assert.equal((await b.getProperties()).blobType, 'AppendBlob');
  });
  it('append with a wrong appendPosition is AppendPositionConditionNotMet 412', async () => {
    const c = await newContainer();
    const b = c.getAppendBlobClient('a');
    await b.create();
    await b.appendBlock('abc', 3);
    const e = await rejection(() => b.appendBlock('x', 1, { conditions: { appendPosition: 0 } }));
    assert.equal(e.statusCode, 412);
    assert.equal(code(e), 'AppendPositionConditionNotMet');
  });
  it('sealing an append blob refuses further appends', async () => {
    const c = await newContainer();
    const b = c.getAppendBlobClient('a');
    await b.create();
    await b.appendBlock('abc', 3);
    await b.seal();
    assert.equal((await b.getProperties()).isSealed, true);
    const e = await rejection(() => b.appendBlock('x', 1));
    assert.equal(e.statusCode, 409);
    assert.equal(code(e), 'BlobIsSealed');
  });
  it('page blob uploadPages, getPageRanges, clearPages and resize', async () => {
    const c = await newContainer();
    const b = c.getPageBlobClient('p');
    await b.create(4096);
    const page = Buffer.alloc(512, 7);
    await b.uploadPages(page, 512, 512);
    const ranges = await b.getPageRanges(0, 4096);
    // the SDK's own offset/count view computes count as end - start, so check the raw start and end in the XML
    assert.match(ranges._response.bodyAsText, /<Start>512<\/Start><End>1023<\/End>/);
    assert.equal(ranges.pageRange[0].offset, 512);
    const d = await b.download(512, 512);
    assert.ok((await streamToBuffer(d.readableStreamBody)).equals(page));
    const zero = await b.download(0, 512);
    assert.ok((await streamToBuffer(zero.readableStreamBody)).equals(Buffer.alloc(512)));
    await b.clearPages(512, 512);
    assert.equal((await b.getPageRanges(0, 4096)).pageRange.length, 0);
    await b.resize(8192);
    assert.equal((await b.getProperties()).contentLength, 8192);
    assert.equal((await b.getProperties()).blobType, 'PageBlob');
  });
});

describe('conditional requests', () => {
  it('ifMatch with a stale etag is ConditionNotMet 412', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('c');
    const u = await b.upload('a', 1);
    await b.upload('b', 1);
    const e = await rejection(() => b.upload('c', 1, { conditions: { ifMatch: u.etag } }));
    assert.equal(e.statusCode, 412);
    assert.equal(code(e), 'ConditionNotMet');
  });
  it('ifNoneMatch "*" on upload fails with BlobAlreadyExists 409 when the blob exists and works when it does not', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('c');
    await b.upload('a', 1, { conditions: { ifNoneMatch: '*' } });
    const e = await rejection(() => b.upload('b', 1, { conditions: { ifNoneMatch: '*' } }));
    assert.equal(e.statusCode, 409);
    assert.equal(code(e), 'BlobAlreadyExists');
  });
  it('download with ifNoneMatch of the current etag is 304 Not Modified', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('c');
    const u = await b.upload('a', 1);
    const e = await rejection(() => b.download(0, undefined, { conditions: { ifNoneMatch: u.etag } }));
    assert.equal(e.statusCode, 304);
  });
  it('ifUnmodifiedSince in the past on delete is 412', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('c');
    await b.upload('a', 1);
    const e = await rejection(() => b.delete({ conditions: { ifUnmodifiedSince: new Date(Date.now() - 86400_000) } }));
    assert.equal(e.statusCode, 412);
  });
  it('tagConditions: delete only when the tag matches', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('c');
    await b.upload('a', 1, { tags: { state: 'new' } });
    const e = await rejection(() => b.delete({ conditions: { tagConditions: "\"state\" = 'old'" } }));
    assert.equal(e.statusCode, 412);
    await b.delete({ conditions: { tagConditions: "\"state\" = 'new'" } });
  });
});

describe('names that need encoding', () => {
  const names = [
    'with space.txt', 'plus+sign', 'percent%25literal', 'percent%', 'hash#tag', 'question?mark', 'unicode-日本語-é.txt',
    'emoji-😀.txt', 'a/b c/d+e.txt', 'ampersand&equals=semi;colon', 'trailing.', 'tilde~and\'quote', '[brackets]{braces}',
  ];
  for (const name of names) {
    it(`round trips the blob name ${JSON.stringify(name)} through upload, download, list and delete`, async () => {
      const c = await newContainer();
      const b = c.getBlockBlobClient(name);
      await b.upload('n', 1);
      assert.equal(await text(await b.download()), 'n');
      const listed = (await collect(c.listBlobsFlat())).map((x) => x.name);
      assert.deepEqual(listed, [name]);
      assert.equal(b.name, name);
      await b.delete();
      assert.equal(await b.exists(), false);
    });
  }
  it('a blob name with a space can be a copy source and carry a SAS', async () => {
    const c = await newContainer();
    const name = 'dir one/file #1+2.txt';
    const b = c.getBlockBlobClient(name);
    await b.upload('sasdata', 7);
    const sas = generateBlobSASQueryParameters({ containerName: c.containerName, blobName: name, permissions: BlobSASPermissions.parse('r'), expiresOn: new Date(Date.now() + 600_000) }, cred).toString();
    const anon = new BlockBlobClient(`${b.url}?${sas}`);
    assert.equal(await text(await anon.download()), 'sasdata');
  });
});

describe('SAS generated by the SDK', () => {
  it('a blob SAS with read permission lets a second anonymous client read but not write', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('s');
    await b.upload('secret', 6);
    const sas = generateBlobSASQueryParameters({ containerName: c.containerName, blobName: 's', permissions: BlobSASPermissions.parse('r'), expiresOn: new Date(Date.now() + 3600_000), protocol: SASProtocol.HttpsAndHttp }, cred).toString();
    const other = new BlobClient(`${b.url}?${sas}`);
    assert.equal(await text(await other.download()), 'secret');
    const e = await rejection(() => new BlockBlobClient(`${b.url}?${sas}`).upload('x', 1));
    assert.equal(e.statusCode, 403);
    assert.equal(code(e), 'AuthorizationPermissionMismatch');
  });
  it('a SAS signed with the wrong key is AuthenticationFailed 403 (account devstoreaccount1)', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('s');
    await b.upload('secret', 6);
    const bad = new StorageSharedKeyCredential(DEV_ACCOUNT, Buffer.alloc(64, 1).toString('base64'));
    const sas = generateBlobSASQueryParameters({ containerName: c.containerName, blobName: 's', permissions: BlobSASPermissions.parse('r'), expiresOn: new Date(Date.now() + 3600_000) }, bad).toString();
    const e = await rejection(() => new BlobClient(`${b.url}?${sas}`).download());
    assert.equal(e.statusCode, 403);
    assert.equal(code(e), 'AuthenticationFailed');
  });
  it('an expired SAS is AuthenticationFailed 403', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('s');
    await b.upload('secret', 6);
    const sas = generateBlobSASQueryParameters({ containerName: c.containerName, blobName: 's', permissions: BlobSASPermissions.parse('r'), startsOn: new Date(Date.now() - 7200_000), expiresOn: new Date(Date.now() - 3600_000) }, cred).toString();
    const e = await rejection(() => new BlobClient(`${b.url}?${sas}`).download());
    assert.equal(e.statusCode, 403);
  });
  it('a container SAS with list permission lists blobs through a second client', async () => {
    const c = await newContainer();
    await c.getBlockBlobClient('one').upload('1', 1);
    const sas = generateBlobSASQueryParameters({ containerName: c.containerName, permissions: ContainerSASPermissions.parse('rl'), expiresOn: new Date(Date.now() + 3600_000) }, cred).toString();
    const viaSas = new ContainerClient(`${c.url}?${sas}`);
    const names = (await collect(viaSas.listBlobsFlat())).map((x) => x.name);
    assert.deepEqual(names, ['one']);
  });
  it('a SAS with response header overrides (rscd, rsct) changes the download headers', async () => {
    const c = await newContainer();
    const b = c.getBlockBlobClient('s');
    await b.upload('x', 1);
    const sas = generateBlobSASQueryParameters({ containerName: c.containerName, blobName: 's', permissions: BlobSASPermissions.parse('r'), expiresOn: new Date(Date.now() + 3600_000), contentDisposition: 'attachment; filename=foo.txt', contentType: 'text/special' }, cred).toString();
    const d = await new BlobClient(`${b.url}?${sas}`).download();
    assert.equal(d.contentType, 'text/special');
    assert.equal(d.contentDisposition, 'attachment; filename=foo.txt');
  });
  it('a stored access policy SAS honours the policy permissions', async () => {
    const c = await newContainer();
    await c.getBlockBlobClient('s').upload('x', 1);
    await c.setAccessPolicy(undefined, [{ id: 'readonly', accessPolicy: { startsOn: new Date(Date.now() - 60_000), expiresOn: new Date(Date.now() + 3600_000), permissions: 'r' } }]);
    const sas = generateBlobSASQueryParameters({ containerName: c.containerName, blobName: 's', identifier: 'readonly' }, cred).toString();
    assert.equal(await text(await new BlobClient(`${c.getBlobClient('s').url}?${sas}`).download()), 'x');
  });
  it('an account SAS (services b, resource types sco) can create a container and upload', async () => {
    const sas = generateAccountSASQueryParameters({
      services: AccountSASServices.parse('b').toString(), resourceTypes: AccountSASResourceTypes.parse('sco').toString(),
      permissions: AccountSASPermissions.parse('rwdlac'), expiresOn: new Date(Date.now() + 3600_000), protocol: SASProtocol.HttpsAndHttp,
    }, cred).toString();
    const s = new BlobServiceClient(`${emu.url('blob', DEV_ACCOUNT)}?${sas}`);
    const c = s.getContainerClient(uniq('acc'));
    await c.create();
    await c.getBlockBlobClient('f').upload('acc', 3);
    assert.equal(await text(await c.getBlockBlobClient('f').download()), 'acc');
  });
  it('an account SAS for the queue service only is refused on blob (AuthorizationServiceMismatch)', async () => {
    const sas = generateAccountSASQueryParameters({
      services: 'q', resourceTypes: 'sco', permissions: AccountSASPermissions.parse('rwdlac'), expiresOn: new Date(Date.now() + 3600_000),
    }, cred).toString();
    const s = new BlobServiceClient(`${emu.url('blob', DEV_ACCOUNT)}?${sas}`);
    const e = await rejection(() => collect(s.listContainers()));
    assert.equal(e.statusCode, 403);
  });
  it('a user delegation SAS made from getUserDelegationKey works for a second client', async () => {
    const https = new BlobServiceClient(emu.url('blob', DEV_ACCOUNT, 'https'), fakeCredential);
    const start = new Date(Date.now() - 60_000);
    const expiry = new Date(Date.now() + 3600_000);
    const key = await https.getUserDelegationKey(start, expiry);
    assert.ok(key.value);
    assert.ok(key.signedObjectId && key.signedTenantId);
    assert.equal(key.signedService, 'b');
    const c = svc.getContainerClient(uniq('ud'));
    await c.create();
    await c.getBlockBlobClient('u').upload('delegated', 9);
    const sas = generateBlobSASQueryParameters({ containerName: c.containerName, blobName: 'u', permissions: BlobSASPermissions.parse('r'), startsOn: start, expiresOn: expiry }, key, DEV_ACCOUNT).toString();
    assert.ok(sas.includes('skoid='));
    assert.equal(await text(await new BlobClient(`${c.getBlobClient('u').url}?${sas}`).download()), 'delegated');
  });
});

describe('batch', () => {
  it('BlobBatchClient.deleteBlobs deletes several blobs in one request', async () => {
    const c = await newContainer();
    const names = ['b1', 'b2', 'b3'];
    for (const n of names) await c.getBlockBlobClient(n).upload('x', 1);
    const batch = new BlobBatchClient(emu.url('blob', DEV_ACCOUNT), cred);
    const r = await batch.deleteBlobs(names.map((n) => c.getBlobClient(n).url), cred);
    assert.equal(r.subResponsesSucceededCount, 3);
    assert.equal(r.subResponsesFailedCount, 0);
    assert.equal((await collect(c.listBlobsFlat())).length, 0);
  });
  it('a batch with a missing blob reports the failed sub response with 404', async () => {
    const c = await newContainer();
    await c.getBlockBlobClient('here').upload('x', 1);
    const batch = new BlobBatchClient(emu.url('blob', DEV_ACCOUNT), cred);
    const r = await batch.deleteBlobs([c.getBlobClient('here').url, c.getBlobClient('absent').url], cred);
    assert.equal(r.subResponsesSucceededCount, 1);
    assert.equal(r.subResponsesFailedCount, 1);
    assert.equal(r.subResponses[1].status, 404);
    assert.equal(r.subResponses[1].errorCode, 'BlobNotFound');
  });
  it('BlobBatchClient.setBlobsAccessTier moves blobs to Cool', async () => {
    const c = await newContainer();
    for (const n of ['t1', 't2']) await c.getBlockBlobClient(n).upload('x', 1);
    const batch = new BlobBatchClient(emu.url('blob', DEV_ACCOUNT), cred);
    const r = await batch.setBlobsAccessTier([c.getBlobClient('t1').url, c.getBlobClient('t2').url], cred, 'Cool');
    assert.equal(r.subResponsesSucceededCount, 2);
    assert.equal((await c.getBlobClient('t1').getProperties()).accessTier, 'Cool');
  });
});

describe('service level', () => {
  it('getProperties on the service returns the default service properties and setProperties round trips CORS', async () => {
    const acct = uniq('sp');
    const s = new BlobServiceClient(emu.url('blob', acct), new StorageSharedKeyCredential(acct, DEV_KEY));
    await s.setProperties({ cors: [{ allowedOrigins: 'https://a.example', allowedMethods: 'GET,PUT', allowedHeaders: 'x-ms-*', exposedHeaders: 'x-ms-meta-*', maxAgeInSeconds: 60 }] });
    const p = await s.getProperties();
    assert.equal(p.cors.length, 1);
    assert.equal(p.cors[0].allowedOrigins, 'https://a.example');
    assert.equal(p.cors[0].maxAgeInSeconds, 60);
  });
  it('getAccountInfo reports skuName and accountKind', async () => {
    const r = await svc.getAccountInfo();
    assert.ok(r.skuName);
    assert.ok(r.accountKind);
  });
  it('getStatistics is live', async () => {
    const r = await svc.getStatistics();
    assert.equal(r.geoReplication.status, 'live');
  });
});
