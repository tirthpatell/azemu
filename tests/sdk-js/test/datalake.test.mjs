import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { Readable } from 'node:stream';
import {
  DataLakeServiceClient, StorageSharedKeyCredential, DataLakeFileClient, DataLakeFileSystemClient,
  generateDataLakeSASQueryParameters, DataLakeSASPermissions, FileSystemSASPermissions, DirectorySASPermissions,
} from '@azure/storage-file-datalake';
import { env, uniq, DEV_ACCOUNT, DEV_KEY, collect, streamToBuffer, rejection, fakeCredential } from '../lib/emu.mjs';

const emu = env();
const cred = new StorageSharedKeyCredential(DEV_ACCOUNT, DEV_KEY);
// The JavaScript Data Lake SDK turns a dfs URL into the blob URL it needs for some calls (file system create/delete/list, leases, metadata,
// user delegation keys) only for the known Azure suffixes (`dfs.core.windows.net`, `dfs.core.chinacloudapi.cn`, ...), so with `acct.dfs.core.localhost`
// those calls would go to the dfs host. A host that still contains `dfs.core.windows.net` and ends in `.localhost` is mapped to `acct.blob.core.windows.net.localhost`
// and both land on the emulator (see docs/sdk-notes.md, JavaScript, Data Lake).
const dfsUrl = (acct, scheme = 'http') => `${scheme}://${acct}.dfs.core.windows.net.localhost:${emu.port}`;
const svc = new DataLakeServiceClient(dfsUrl(DEV_ACCOUNT), cred);
const code = (e) => e.details?.errorCode ?? e.code;
async function newFs() {
  const fs = svc.getFileSystemClient(uniq('fs'));
  await fs.create();
  return fs;
}
const read = async (file, ...a) => (await streamToBuffer((await file.read(...a)).readableStreamBody)).toString();

describe('file systems', () => {
  it('create, exists, getProperties with metadata, delete', async () => {
    const fs = svc.getFileSystemClient(uniq('fs'));
    assert.equal(await fs.exists(), false);
    await fs.create({ metadata: { a: 'b' } });
    assert.equal(await fs.exists(), true);
    const p = await fs.getProperties();
    assert.equal(p.metadata.a, 'b');
    await fs.delete();
    assert.equal(await fs.exists(), false);
  });
  it('creating twice is ContainerAlreadyExists 409', async () => {
    const fs = await newFs();
    const e = await rejection(() => fs.create());
    assert.equal(e.statusCode, 409);
    assert.equal(code(e), 'ContainerAlreadyExists');
  });
  it('listFileSystems finds the new one by prefix and returns metadata when asked', async () => {
    const name = uniq('lfs');
    await svc.getFileSystemClient(name).create({ metadata: { k: 'v' } });
    const r = await collect(svc.listFileSystems({ prefix: name, includeMetadata: true }));
    assert.equal(r.length, 1);
    assert.equal(r[0].name, name);
    assert.equal(r[0].metadata.k, 'v');
  });
  it('a file system made through the Data Lake endpoint is visible to the Blob endpoint as a container', async () => {
    const { BlobServiceClient } = await import('@azure/storage-blob');
    const fs = await newFs();
    await fs.getFileClient('x.txt').create();
    const b = new BlobServiceClient(`http://${DEV_ACCOUNT}.blob.core.windows.net.localhost:${emu.port}`, cred);
    const blobs = await collect(b.getContainerClient(fs.name).listBlobsFlat());
    assert.deepEqual(blobs.map((x) => x.name), ['x.txt']);
  });
});

describe('directories and files', () => {
  it('createDirectory makes nested directories and listPaths shows them with isDirectory', async () => {
    const fs = await newFs();
    await fs.getDirectoryClient('a/b/c').create();
    const all = await collect(fs.listPaths({ recursive: true }));
    assert.deepEqual(all.map((p) => [p.name, p.isDirectory]), [['a', true], ['a/b', true], ['a/b/c', true]]);
    const top = await collect(fs.listPaths());
    assert.deepEqual(top.map((p) => p.name), ['a']);
  });
  it('a file created under a directory shows up with contentLength 0, and upload fills it', async () => {
    const fs = await newFs();
    const dir = fs.getDirectoryClient('data');
    await dir.create();
    const f = dir.getFileClient('f.txt');
    await f.create();
    assert.equal((await f.getProperties()).contentLength, 0);
    await f.upload(Buffer.from('uploaded'));
    assert.equal(await read(f), 'uploaded');
    const paths = await collect(fs.listPaths({ path: 'data' }));
    assert.deepEqual(paths.map((p) => [p.name, p.isDirectory, p.contentLength]), [['data/f.txt', false, 8]]);
  });
  it('append and flush: two appends at offsets 0 and 5 and one flush at 10 give the concatenation', async () => {
    const fs = await newFs();
    const f = fs.getFileClient('log.txt');
    await f.create();
    await f.append('hello', 0, 5);
    await f.append('world', 5, 5);
    assert.equal((await f.getProperties()).contentLength, 0, 'nothing is committed before the flush');
    await f.flush(10);
    assert.equal(await read(f), 'helloworld');
    assert.equal((await f.getProperties()).contentLength, 10);
  });
  it('appends sent out of order in parallel still flush into the right bytes (uploadFile style)', async () => {
    const fs = await newFs();
    const f = fs.getFileClient('par.bin');
    await f.create();
    const chunks = [randomBytes(1000), randomBytes(1000), randomBytes(1000), randomBytes(500)];
    await Promise.all([3, 1, 0, 2].map((i) => f.append(chunks[i], i * 1000, chunks[i].length)));
    await f.flush(3500);
    assert.ok((await streamToBuffer((await f.read()).readableStreamBody)).equals(Buffer.concat(chunks)));
  });
  it('flush at a position past the appended data is InvalidFlushPosition 400', async () => {
    const fs = await newFs();
    const f = fs.getFileClient('f');
    await f.create();
    await f.append('abc', 0, 3);
    const e = await rejection(() => f.flush(10));
    assert.equal(e.statusCode, 400);
    assert.equal(code(e), 'InvalidFlushPosition');
  });
  it('upload of a stream with a large body (6 MiB) with parallel chunks keeps every byte', async () => {
    const fs = await newFs();
    const f = fs.getFileClient('big.bin');
    const data = randomBytes(6 * 1024 * 1024 + 11);
    await f.upload(data, { singleUploadThreshold: 1024 * 1024, chunkSize: 1024 * 1024, concurrency: 4 });
    assert.ok((await f.readToBuffer()).equals(data));
  });
  it('read with an offset and count returns that slice', async () => {
    const fs = await newFs();
    const f = fs.getFileClient('r');
    await f.create();
    await f.append('0123456789', 0, 10);
    await f.flush(10);
    assert.equal(await read(f, 3, 4), '3456');
  });
  it('setMetadata, setHttpHeaders and getProperties on a file', async () => {
    const fs = await newFs();
    const f = fs.getFileClient('m');
    await f.create();
    await f.setMetadata({ owner: 'me' });
    await f.setHttpHeaders({ contentType: 'text/special' });
    const p = await f.getProperties();
    assert.equal(p.metadata.owner, 'me');
    assert.equal(p.contentType, 'text/special');
  });
  it('create with ifNoneMatch "*" on an existing file is PathAlreadyExists 409', async () => {
    const fs = await newFs();
    const f = fs.getFileClient('x');
    await f.create();
    const e = await rejection(() => f.create({ conditions: { ifNoneMatch: '*' } }));
    assert.equal(e.statusCode, 409);
    assert.equal(code(e), 'PathAlreadyExists');
  });
  it('a missing file is PathNotFound 404 and exists() is false', async () => {
    const fs = await newFs();
    const f = fs.getFileClient('nope');
    assert.equal(await f.exists(), false);
    const e = await rejection(() => f.getProperties());
    assert.equal(e.statusCode, 404);
  });
  it('delete of a non-empty directory needs recursive (DirectoryNotEmpty 409)', async () => {
    const fs = await newFs();
    const d = fs.getDirectoryClient('d');
    await d.create();
    await d.getFileClient('f').create();
    const e = await rejection(() => d.delete());
    assert.equal(e.statusCode, 409);
    assert.equal(code(e), 'DirectoryNotEmpty');
    await d.delete(true);
    assert.equal(await d.exists(), false);
  });
  it('listPaths pages with maxResults and returns every path once', async () => {
    const fs = await newFs();
    for (let i = 0; i < 7; i++) await fs.getFileClient(`f${i}`).create();
    const sizes = [];
    const names = [];
    for await (const p of fs.listPaths().byPage({ maxPageSize: 3 })) {
      sizes.push(p.pathItems.length);
      names.push(...p.pathItems.map((x) => x.name));
    }
    assert.deepEqual(sizes, [3, 3, 1]);
    assert.deepEqual(names, [0, 1, 2, 3, 4, 5, 6].map((i) => `f${i}`));
  });
  it('names with spaces, unicode and plus round trip', async () => {
    const fs = await newFs();
    for (const name of ['a b/c d.txt', 'dir/日本語.txt', 'p+q/x%y.txt']) {
      const f = fs.getFileClient(name);
      await f.create();
      await f.append('z', 0, 1);
      await f.flush(1);
      assert.equal(await read(f), 'z');
    }
    const names = (await collect(fs.listPaths({ recursive: true }))).filter((p) => !p.isDirectory).map((p) => p.name).sort();
    assert.deepEqual(names, ['a b/c d.txt', 'dir/日本語.txt', 'p+q/x%y.txt']);
  });
});

describe('rename', () => {
  it('renaming a file moves its content and the source is gone', async () => {
    const fs = await newFs();
    const f = fs.getFileClient('old.txt');
    await f.upload(Buffer.from('payload'));
    await fs.getDirectoryClient('new').create();
    const moved = await f.move('new/place.txt');
    assert.equal(moved._response.status, 201);
    assert.equal(await f.exists(), false);
    assert.equal(await read(fs.getFileClient('new/place.txt')), 'payload');
  });
  it('renaming a directory moves the whole subtree in one call', async () => {
    const fs = await newFs();
    await fs.getFileClient('src/a/1.txt').upload(Buffer.from('1'));
    await fs.getFileClient('src/b.txt').upload(Buffer.from('2'));
    const d = fs.getDirectoryClient('src');
    await d.move('dst');
    const names = (await collect(fs.listPaths({ recursive: true }))).map((p) => p.name);
    assert.deepEqual(names, ['dst', 'dst/a', 'dst/a/1.txt', 'dst/b.txt']);
  });
  it('renaming a missing source is SourcePathNotFound 404', async () => {
    const fs = await newFs();
    const e = await rejection(() => fs.getFileClient('ghost').move('to'));
    assert.equal(e.statusCode, 404);
    assert.equal(code(e), 'SourcePathNotFound');
  });
  it('rename across file systems', async () => {
    const a = await newFs();
    const b = await newFs();
    await a.getFileClient('f').upload(Buffer.from('xx'));
    await a.getFileClient('f').move(b.name, 'g');
    assert.equal(await read(b.getFileClient('g')), 'xx');
  });
});

describe('access control', () => {
  it('setAccessControl round trips an ACL list and permissions', async () => {
    const fs = await newFs();
    const f = fs.getFileClient('acl.txt');
    await f.create();
    await f.setAccessControl([
      { accessControlType: 'user', entityId: '', defaultScope: false, permissions: { read: true, write: true, execute: false } },
      { accessControlType: 'group', entityId: '', defaultScope: false, permissions: { read: true, write: false, execute: false } },
      { accessControlType: 'other', entityId: '', defaultScope: false, permissions: { read: false, write: false, execute: false } },
    ]);
    const r = await f.getAccessControl();
    assert.equal(r.permissions.owner.read, true);
    assert.equal(r.permissions.owner.write, true);
    assert.equal(r.permissions.other.read, false);
    assert.equal(r.acl.length, 3);
  });
  it('setPermissions with an octal-like object and owner/group change', async () => {
    const fs = await newFs();
    const f = fs.getFileClient('p.txt');
    await f.create();
    await f.setPermissions({ owner: { read: true, write: true, execute: true }, group: { read: true, write: false, execute: true }, other: { read: false, write: false, execute: false }, stickyBit: false, extendedAcls: false });
    const r = await f.getAccessControl();
    assert.deepEqual({ ...r.permissions.owner }, { read: true, write: true, execute: true });
    assert.deepEqual({ ...r.permissions.group }, { read: true, write: false, execute: true });
    await f.setAccessControl([{ accessControlType: 'user', entityId: '', defaultScope: false, permissions: { read: true, write: true, execute: true } }], { owner: 'owner-1', group: 'group-1' });
    const r2 = await f.getAccessControl();
    assert.equal(r2.owner, 'owner-1');
    assert.equal(r2.group, 'group-1');
  });
  it('a named user entry in a directory ACL with a default scope is inherited by new files', async () => {
    const fs = await newFs();
    const d = fs.getDirectoryClient('shared');
    await d.create();
    const uid = '11111111-2222-3333-4444-555555555555';
    const acl = (await d.getAccessControl()).acl;
    await d.setAccessControl([
      ...acl,
      { accessControlType: 'user', entityId: uid, defaultScope: false, permissions: { read: true, write: false, execute: true } },
      { accessControlType: 'user', entityId: uid, defaultScope: true, permissions: { read: true, write: false, execute: true } },
    ]);
    const f = d.getFileClient('child.txt');
    await f.create();
    const child = (await f.getAccessControl()).acl;
    assert.ok(child.some((e) => e.accessControlType === 'user' && e.entityId === uid), JSON.stringify(child));
  });
  it('renaming into a directory that does not exist is RenameDestinationParentPathNotFound 404', async () => {
    const fs = await newFs();
    await fs.getFileClient('a').upload(Buffer.from('x'));
    const e = await rejection(() => fs.getFileClient('a').move('missing-dir/b'));
    assert.equal(e.statusCode, 404);
    assert.equal(code(e), 'RenameDestinationParentPathNotFound');
  });
  it('updateAccessControlRecursive (modify) touches directories and files: counts 2 directories and 2 files', async () => {
    const fs = await newFs();
    await fs.getFileClient('tree/a.txt').create();
    await fs.getFileClient('tree/sub/b.txt').create();
    const d = fs.getDirectoryClient('tree');
    const uid = '99999999-2222-3333-4444-555555555555';
    const r = await d.updateAccessControlRecursive([{ accessControlType: 'user', entityId: uid, defaultScope: false, permissions: { read: true, write: false, execute: false } }]);
    assert.equal(r.counters.failedChangesCount, 0);
    assert.equal(r.counters.changedDirectoriesCount, 2);
    assert.equal(r.counters.changedFilesCount, 2);
    const b = await fs.getFileClient('tree/sub/b.txt').getAccessControl();
    assert.ok(b.acl.some((e) => e.entityId === uid));
  });
});

describe('leases, SAS and auth', () => {
  it('a leased file refuses a write without the lease id (LeaseIdMissing 412)', async () => {
    const fs = await newFs();
    const f = fs.getFileClient('l');
    await f.create();
    const lease = f.getDataLakeLeaseClient();
    await lease.acquireLease(30);
    const e = await rejection(() => f.append('x', 0, 1));
    assert.equal(e.statusCode, 412);
    assert.equal(code(e), 'LeaseIdMissing');
    await lease.releaseLease();
  });
  it('a file SAS with read permission lets an anonymous client read', async () => {
    const fs = await newFs();
    const f = fs.getFileClient('s.txt');
    await f.upload(Buffer.from('sas data'));
    const sas = generateDataLakeSASQueryParameters({ fileSystemName: fs.name, pathName: 's.txt', permissions: DataLakeSASPermissions.parse('r'), expiresOn: new Date(Date.now() + 3600_000) }, cred).toString();
    const anon = new DataLakeFileClient(`${f.url}?${sas}`);
    assert.equal(await read(anon), 'sas data');
  });
  it('a file system SAS with list permission lists paths', async () => {
    const fs = await newFs();
    await fs.getFileClient('one').create();
    const sas = generateDataLakeSASQueryParameters({ fileSystemName: fs.name, permissions: FileSystemSASPermissions.parse('rl'), expiresOn: new Date(Date.now() + 3600_000) }, cred).toString();
    const anon = new DataLakeFileSystemClient(`${fs.url}?${sas}`);
    assert.deepEqual((await collect(anon.listPaths())).map((p) => p.name), ['one']);
  });
  it('a directory SAS (sr=d, sdd=1) opens files below that directory only', async () => {
    const fs = await newFs();
    await fs.getFileClient('dirA/f.txt').upload(Buffer.from('A'));
    await fs.getFileClient('dirB/f.txt').upload(Buffer.from('B'));
    const sas = generateDataLakeSASQueryParameters({ fileSystemName: fs.name, pathName: 'dirA', isDirectory: true, permissions: DirectorySASPermissions.parse('rl'), expiresOn: new Date(Date.now() + 3600_000) }, cred).toString();
    assert.equal(await read(new DataLakeFileClient(`${fs.getFileClient('dirA/f.txt').url}?${sas}`)), 'A');
    const e = await rejection(() => new DataLakeFileClient(`${fs.getFileClient('dirB/f.txt').url}?${sas}`).read());
    assert.equal(e.statusCode, 403);
  });
  it('a wrong shared key for devstoreaccount1 is AuthenticationFailed 403', async () => {
    const bad = new DataLakeServiceClient(dfsUrl(DEV_ACCOUNT), new StorageSharedKeyCredential(DEV_ACCOUNT, Buffer.alloc(64, 4).toString('base64')));
    const e = await rejection(() => collect(bad.listFileSystems()));
    assert.equal(e.statusCode, 403);
  });
  it('a bearer token over https works', async () => {
    const s = new DataLakeServiceClient(dfsUrl(uniq('tok'), 'https'), fakeCredential);
    const fs = s.getFileSystemClient(uniq('fs'));
    await fs.create();
    await fs.getFileClient('t').upload(Buffer.from('tok'));
    assert.equal(await read(fs.getFileClient('t')), 'tok');
  });
  it('getUserDelegationKey through the Data Lake service client (https, bearer)', async () => {
    const s = new DataLakeServiceClient(dfsUrl(DEV_ACCOUNT, 'https'), fakeCredential);
    const k = await s.getUserDelegationKey(new Date(Date.now() - 60_000), new Date(Date.now() + 3600_000));
    assert.ok(k.value);
  });
  it('deleteIfExists on a file system that is gone reports succeeded false', async () => {
    const fs = svc.getFileSystemClient(uniq('gone'));
    assert.equal((await fs.deleteIfExists()).succeeded, false);
  });
  it('uploading from a Node stream with file.upload(Buffer) and readableStream round trip', async () => {
    const fs = await newFs();
    const f = fs.getFileClient('s');
    await f.create();
    const data = randomBytes(100_000);
    await f.append(Readable.from([data]), 0, data.length);
    await f.flush(data.length);
    assert.ok((await f.readToBuffer()).equals(data));
  });
});

describe('the natural emulator URL (acct.dfs.core.localhost)', () => {
  it('createFileSystem through acct.dfs.core.localhost', { skip: 'sdk: the JS SDK only maps Azure host suffixes to the blob endpoint, so Create File System goes to the dfs host and gets 501 "dfs: PUT /fs ?restype=container". Use acct.dfs.core.windows.net.localhost (docs/sdk-notes.md)' }, async () => {
    const s = new DataLakeServiceClient(emu.url('dfs', DEV_ACCOUNT), cred);
    await s.getFileSystemClient(uniq('fs')).create();
  });
});
