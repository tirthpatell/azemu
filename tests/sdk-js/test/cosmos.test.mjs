import { describe, it, before } from 'node:test';
import assert from 'node:assert/strict';
import { CosmosClient, PartitionKeyKind, ChangeFeedStartFrom, BulkOperationType, PatchOperationType, ConsistencyLevel, PartitionKeyBuilder } from '@azure/cosmos';
import { env, uniq, collect, rejection, sleep } from '../lib/emu.mjs';

const emu = env();
const KEY = 'C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==';
const client = new CosmosClient({ endpoint: `${emu.url('documents', uniq('cosmos'), 'https')}/`, key: KEY });
const code = (e) => e.code;

async function newContainer(opts = {}) {
  const { database } = await client.databases.createIfNotExists({ id: uniq('db') });
  const { container } = await database.containers.createIfNotExists({ id: uniq('c'), partitionKey: { paths: ['/pk'] }, ...opts });
  return { database, container };
}

describe('databases and containers', () => {
  it('create database returns 201, a second create is 409 Conflict, createIfNotExists returns 200', async () => {
    const id = uniq('db');
    const a = await client.databases.create({ id });
    assert.equal(a.statusCode, 201);
    const e = await rejection(() => client.databases.create({ id }));
    assert.equal(e.code, 409);
    const b = await client.databases.createIfNotExists({ id });
    assert.equal(b.statusCode, 200);
  });
  it('read and delete a database; a missing one is 404', async () => {
    const id = uniq('db');
    await client.databases.create({ id });
    assert.equal((await client.database(id).read()).resource.id, id);
    await client.database(id).delete();
    const e = await rejection(() => client.database(id).read());
    assert.equal(e.code, 404);
  });
  it('readAll lists databases and query filters them by id', async () => {
    const id = uniq('dbq');
    await client.databases.create({ id });
    const all = await client.databases.readAll().fetchAll();
    assert.ok(all.resources.some((d) => d.id === id));
    const q = await client.databases.query({ query: 'SELECT * FROM d WHERE d.id = @id', parameters: [{ name: '@id', value: id }] }).fetchAll();
    assert.equal(q.resources.length, 1);
  });
  it('create container with partition key, indexing policy and unique key; read returns them', async () => {
    const { database } = await newContainer();
    const id = uniq('c');
    const { resource } = await database.containers.create({
      id, partitionKey: { paths: ['/tenant'], version: 2 }, uniqueKeyPolicy: { uniqueKeys: [{ paths: ['/email'] }] },
      indexingPolicy: { indexingMode: 'consistent', includedPaths: [{ path: '/*' }], excludedPaths: [{ path: '/"_etag"/?' }] }, defaultTtl: -1,
    });
    assert.deepEqual(resource.partitionKey.paths, ['/tenant']);
    assert.equal(resource.partitionKey.version, 2);
    assert.equal(resource.uniqueKeyPolicy.uniqueKeys[0].paths[0], '/email');
    assert.equal(resource.defaultTtl, -1);
    assert.ok(resource._rid && resource._etag && resource._ts);
  });
  it('hierarchical partition keys (MultiHash with 2 paths) are accepted', async () => {
    const { database } = await newContainer();
    const { resource } = await database.containers.create({ id: uniq('c'), partitionKey: { paths: ['/tenant', '/user'], kind: PartitionKeyKind.MultiHash, version: 2 } });
    assert.equal(resource.partitionKey.kind, 'MultiHash');
    assert.deepEqual(resource.partitionKey.paths, ['/tenant', '/user']);
  });
  it('replace a container changes the default TTL; delete removes it', async () => {
    const { database, container } = await newContainer();
    const { resource } = await container.read();
    await container.replace({ ...resource, defaultTtl: 60 });
    assert.equal((await container.read()).resource.defaultTtl, 60);
    await container.delete();
    const e = await rejection(() => container.read());
    assert.equal(e.code, 404);
    void database;
  });
  it('database.containers.readAll lists the containers', async () => {
    const { database, container } = await newContainer();
    const all = await database.containers.readAll().fetchAll();
    assert.deepEqual(all.resources.map((c) => c.id), [container.id]);
  });
  it('throughput can be read and replaced on a container with provisioned throughput', async () => {
    const { database } = await newContainer();
    const { container } = await database.containers.create({ id: uniq('c'), partitionKey: { paths: ['/pk'] }, throughput: 600 });
    const offer = await container.readOffer();
    assert.equal(offer.resource.content.offerThroughput, 600);
    await container.offer?.read?.();
  });
});

describe('items', () => {
  it('create, read, replace, delete an item with its system properties', async () => {
    const { container } = await newContainer();
    const c = await container.items.create({ id: 'a', pk: 'p1', name: 'Ada' });
    assert.equal(c.statusCode, 201);
    assert.ok(c.etag);
    assert.ok(c.resource._rid && c.resource._ts && c.resource._self);
    const r = await container.item('a', 'p1').read();
    assert.equal(r.resource.name, 'Ada');
    const rep = await container.item('a', 'p1').replace({ id: 'a', pk: 'p1', name: 'Ada L' });
    assert.notEqual(rep.etag, c.etag);
    const d = await container.item('a', 'p1').delete();
    assert.equal(d.statusCode, 204);
    const e = await container.item('a', 'p1').read();
    assert.equal(e.statusCode, 404);
    assert.equal(e.resource, undefined);
  });
  it('create with an existing id in the same partition is 409', async () => {
    const { container } = await newContainer();
    await container.items.create({ id: 'a', pk: 'p' });
    const e = await rejection(() => container.items.create({ id: 'a', pk: 'p' }));
    assert.equal(e.code, 409);
  });
  it('the same id in different partitions is allowed', async () => {
    const { container } = await newContainer();
    await container.items.create({ id: 'a', pk: 'p1', v: 1 });
    await container.items.create({ id: 'a', pk: 'p2', v: 2 });
    assert.equal((await container.item('a', 'p2').read()).resource.v, 2);
  });
  it('upsert creates (201) then replaces (200)', async () => {
    const { container } = await newContainer();
    assert.equal((await container.items.upsert({ id: 'u', pk: 'p', v: 1 })).statusCode, 201);
    const second = await container.items.upsert({ id: 'u', pk: 'p', v: 2 });
    assert.equal(second.statusCode, 200);
    assert.equal(second.resource.v, 2);
  });
  it('replace with a stale ETag (IfMatch) is 412 PreconditionFailed', async () => {
    const { container } = await newContainer();
    const c = await container.items.create({ id: 'a', pk: 'p', v: 1 });
    await container.item('a', 'p').replace({ id: 'a', pk: 'p', v: 2 });
    const e = await rejection(() => container.item('a', 'p').replace({ id: 'a', pk: 'p', v: 3 }, { accessCondition: { type: 'IfMatch', condition: c.etag } }));
    assert.equal(e.code, 412);
  });
  it('read with IfNoneMatch of the current etag returns 304', async () => {
    const { container } = await newContainer();
    const c = await container.items.create({ id: 'a', pk: 'p' });
    const r = await container.item('a', 'p').read({ accessCondition: { type: 'IfNoneMatch', condition: c.etag } });
    assert.equal(r.statusCode, 304);
  });
  it('delete with a stale ETag is 412', async () => {
    const { container } = await newContainer();
    const c = await container.items.create({ id: 'a', pk: 'p' });
    await container.item('a', 'p').replace({ id: 'a', pk: 'p', v: 2 });
    const e = await rejection(() => container.item('a', 'p').delete({ accessCondition: { type: 'IfMatch', condition: c.etag } }));
    assert.equal(e.code, 412);
  });
  it('JSON types round trip: numbers (int and float), booleans, null, nested objects and arrays, unicode', async () => {
    const { container } = await newContainer();
    const doc = { id: 'types', pk: 'p', i: 42, f: 3.14159, big: 9007199254740991, neg: -1, t: true, f2: false, n: null, o: { a: { b: [1, 2, { c: 'd' }] } }, arr: [], s: '日本語 😀 "quoted" \n newline', empty: '' };
    await container.items.create(doc);
    const r = (await container.item('types', 'p').read()).resource;
    for (const k of Object.keys(doc)) assert.deepEqual(r[k], doc[k], k);
  });
  it('ids and partition keys with special characters (space, slash is refused, unicode, percent, plus, quote)', async () => {
    const { container } = await newContainer();
    for (const id of ['with space', 'unicode-é日本', 'pct%25', 'plus+x', "quote'q", 'a,b;c=d']) {
      await container.items.create({ id, pk: id });
      assert.equal((await container.item(id, id).read()).resource.id, id, id);
    }
    // the SDK validates the id itself before sending
    const e = await rejection(() => container.items.create({ id: 'a/b', pk: 'p' }));
    assert.match(e.message, /illegal|invalid/i);
  });
  it('partition key types: number, boolean, null and undefined (missing) read back by the same key', async () => {
    const { container } = await newContainer();
    await container.items.create({ id: 'n', pk: 5 });
    await container.items.create({ id: 'b', pk: true });
    await container.items.create({ id: 'z', pk: null });
    await container.items.create({ id: 'u' });
    assert.equal((await container.item('n', 5).read()).resource.id, 'n');
    assert.equal((await container.item('b', true).read()).resource.id, 'b');
    assert.equal((await container.item('z', null).read()).resource.id, 'z');
    assert.equal((await container.item('u', {}).read()).statusCode, 200);
  });
  it('an item over 2 MB is 413', async () => {
    const { container } = await newContainer();
    const e = await rejection(() => container.items.create({ id: 'big', pk: 'p', blob: 'x'.repeat(2_200_000) }));
    assert.equal(e.code, 413);
  });
  it('unique key violation is 409', async () => {
    const { database } = await newContainer();
    const { container } = await database.containers.create({ id: uniq('c'), partitionKey: { paths: ['/pk'] }, uniqueKeyPolicy: { uniqueKeys: [{ paths: ['/email'] }] } });
    await container.items.create({ id: '1', pk: 'p', email: 'a@x' });
    const e = await rejection(() => container.items.create({ id: '2', pk: 'p', email: 'a@x' }));
    assert.equal(e.code, 409);
    await container.items.create({ id: '3', pk: 'other', email: 'a@x' });
  });
  it('hierarchical partition key items: create, point read with PartitionKeyBuilder and a prefix query', async () => {
    const { database } = await newContainer();
    const { container } = await database.containers.create({ id: uniq('c'), partitionKey: { paths: ['/tenant', '/user'], kind: PartitionKeyKind.MultiHash, version: 2 } });
    await container.items.create({ id: '1', tenant: 't1', user: 'u1', v: 1 });
    await container.items.create({ id: '2', tenant: 't1', user: 'u2', v: 2 });
    await container.items.create({ id: '3', tenant: 't2', user: 'u1', v: 3 });
    const pk = new PartitionKeyBuilder().addValue('t1').addValue('u2').build();
    assert.equal((await container.item('2', pk).read()).resource.v, 2);
    const prefix = new PartitionKeyBuilder().addValue('t1').build();
    const r = await container.items.query('SELECT * FROM c ORDER BY c.v', { partitionKey: prefix }).fetchAll();
    assert.deepEqual(r.resources.map((x) => x.v), [1, 2]);
    const all = await container.items.query('SELECT VALUE COUNT(1) FROM c').fetchAll();
    assert.deepEqual(all.resources, [3]);
  });
  it('deleteAllItemsForPartitionKey removes only that partition', async () => {
    const { container } = await newContainer();
    for (const [id, pk] of [['1', 'a'], ['2', 'a'], ['3', 'b']]) await container.items.create({ id, pk });
    await container.deleteAllItemsForPartitionKey('a');
    const r = await container.items.readAll().fetchAll();
    assert.deepEqual(r.resources.map((x) => x.id), ['3']);
  });
  it('the response carries a request charge, activity id and session token', async () => {
    const { container } = await newContainer();
    const c = await container.items.create({ id: 'a', pk: 'p' });
    assert.ok(c.requestCharge > 0);
    assert.ok(c.activityId);
    assert.ok(c.headers['x-ms-session-token']);
  });
});

describe('patch', () => {
  const ops = [
    { op: PatchOperationType.add, path: '/newField', value: 'n' },
    { op: PatchOperationType.set, path: '/b/c', value: 20 },
    { op: PatchOperationType.replace, path: '/a', value: 10 },
    { op: PatchOperationType.remove, path: '/old' },
    { op: PatchOperationType.incr, path: '/a', value: 5 },
    { op: PatchOperationType.add, path: '/arr/-', value: 4 },
    { op: 'move', from: '/newField', path: '/moved' }, // the SDK's PatchOperationType enum has no move
  ];
  async function patched(body) {
    const { container } = await newContainer();
    await container.items.create({ id: 'p', pk: 'k', a: 1, b: { c: 2 }, arr: [1, 2, 3], old: 'x' });
    const r = await container.item('p', 'k').patch(body);
    assert.equal(r.statusCode, 200);
    const d = r.resource;
    assert.equal(d.a, 15);
    assert.equal(d.b.c, 20);
    assert.equal(d.old, undefined);
    assert.deepEqual(d.arr, [1, 2, 3, 4]);
    assert.equal(d.moved, 'n');
    assert.equal(d.newField, undefined);
  }
  it('add, set, replace, remove, incr and move operations apply in order ({operations} body)', async () => patched({ operations: ops }));
  it('the same operations as a bare array, the form the JS SDK documents first (patch(operations))', async () => patched(ops));
  it('a patch with a failing filter predicate condition is 412', async () => {
    const { container } = await newContainer();
    await container.items.create({ id: 'p', pk: 'k', state: 'new' });
    const e = await rejection(() => container.item('p', 'k').patch({ condition: 'from c where c.state = "old"', operations: [{ op: 'set', path: '/state', value: 'x' }] }));
    assert.equal(e.code, 412);
    const ok = await container.item('p', 'k').patch({ condition: 'from c where c.state = "new"', operations: [{ op: 'set', path: '/state', value: 'done' }] });
    assert.equal(ok.resource.state, 'done');
  });
  it('patching a missing item is 404 and replace on a missing path is 400', async () => {
    const { container } = await newContainer();
    const e = await rejection(() => container.item('nope', 'k').patch({ operations: [{ op: 'set', path: '/a', value: 1 }] }));
    assert.equal(e.code, 404);
    await container.items.create({ id: 'p', pk: 'k' });
    const e2 = await rejection(() => container.item('p', 'k').patch({ operations: [{ op: 'replace', path: '/missing', value: 1 }] }));
    assert.equal(e2.code, 400);
  });
});

describe('bulk and transactional batch', () => {
  it('executeBulkOperations creates, upserts, reads, replaces and deletes with a status per operation', async () => {
    const { container } = await newContainer();
    await container.items.create({ id: 'exists', pk: 'p', v: 0 });
    const ops = [
      { operationType: BulkOperationType.Create, partitionKey: 'p', resourceBody: { id: 'c1', pk: 'p', v: 1 } },
      { operationType: BulkOperationType.Upsert, partitionKey: 'p', resourceBody: { id: 'u1', pk: 'p', v: 2 } },
      { operationType: BulkOperationType.Read, id: 'exists', partitionKey: 'p' },
      { operationType: BulkOperationType.Replace, id: 'exists', partitionKey: 'p', resourceBody: { id: 'exists', pk: 'p', v: 99 } },
      { operationType: BulkOperationType.Delete, id: 'c1', partitionKey: 'p' },
      { operationType: BulkOperationType.Read, id: 'missing', partitionKey: 'p' },
    ];
    const res = await container.items.executeBulkOperations(ops);
    assert.deepEqual(res.map((r) => r.response?.statusCode ?? r.error?.code), [201, 201, 200, 200, 204, 404]);
    assert.equal(res[2].response.resourceBody.v, 0);
    assert.equal((await container.item('exists', 'p').read()).resource.v, 99);
  });
  it('executeBulkOperations with 250 creates over 7 partition keys', async () => {
    const { container } = await newContainer();
    const ops = Array.from({ length: 250 }, (_, i) => ({ operationType: BulkOperationType.Create, partitionKey: `p${i % 7}`, resourceBody: { id: `i${i}`, pk: `p${i % 7}`, n: i } }));
    const res = await container.items.executeBulkOperations(ops);
    assert.equal(res.length, 250);
    assert.ok(res.every((r) => r.response?.statusCode === 201));
    assert.deepEqual((await container.items.query('SELECT VALUE COUNT(1) FROM c').fetchAll()).resources, [250]);
  });
  it('transactional batch applies all operations in one partition', async () => {
    const { container } = await newContainer();
    await container.items.create({ id: 'old', pk: 'p' });
    const r = await container.items.batch([
      { operationType: 'Create', resourceBody: { id: 'b1', pk: 'p', v: 1 } },
      { operationType: 'Upsert', resourceBody: { id: 'b2', pk: 'p', v: 2 } },
      { operationType: 'Delete', id: 'old' },
      { operationType: 'Patch', id: 'b1', resourceBody: { operations: [{ op: 'incr', path: '/v', value: 10 }] } },
    ], 'p');
    assert.equal(r.code, 200);
    assert.equal(r.result.length, 4);
    assert.equal((await container.item('b1', 'p').read()).resource.v, 11);
    assert.equal((await container.item('old', 'p').read()).statusCode, 404);
  });
  it('a failing operation rolls the whole batch back (207: others get 424)', async () => {
    const { container } = await newContainer();
    await container.items.create({ id: 'dup', pk: 'p' });
    const r = await container.items.batch([
      { operationType: 'Create', resourceBody: { id: 'ok', pk: 'p' } },
      { operationType: 'Create', resourceBody: { id: 'dup', pk: 'p' } },
    ], 'p');
    assert.equal(r.code, 207);
    assert.deepEqual(r.result.map((x) => x.statusCode), [424, 409]);
    assert.equal((await container.item('ok', 'p').read()).statusCode, 404);
  });
});

describe('queries', () => {
  async function seeded() {
    const { container } = await newContainer();
    for (let i = 0; i < 60; i++) await container.items.create({ id: `i${String(i).padStart(2, '0')}`, pk: `p${i % 4}`, n: i, name: `name${i}`, tags: [i % 2 ? 'odd' : 'even', 'all'], nested: { score: i * 1.5 } });
    return container;
  }
  it('SELECT * with a parameter filters across partitions', async () => {
    const c = await seeded();
    const r = await c.items.query({ query: 'SELECT * FROM c WHERE c.n >= @min AND c.n < @max', parameters: [{ name: '@min', value: 10 }, { name: '@max', value: 15 }] }).fetchAll();
    assert.deepEqual(r.resources.map((x) => x.n).sort((a, b) => a - b), [10, 11, 12, 13, 14]);
  });
  it('a query scoped to one partition key reads only that partition', async () => {
    const c = await seeded();
    const r = await c.items.query('SELECT VALUE c.n FROM c', { partitionKey: 'p1' }).fetchAll();
    assert.equal(r.resources.length, 15);
    assert.ok(r.resources.every((n) => n % 4 === 1));
  });
  it('cross-partition ORDER BY gives a globally sorted result', async () => {
    const c = await seeded();
    const r = await c.items.query('SELECT c.n FROM c ORDER BY c.n DESC').fetchAll();
    assert.deepEqual(r.resources.map((x) => x.n), [...Array(60).keys()].reverse());
  });
  it('ORDER BY with TOP and OFFSET LIMIT', async () => {
    const c = await seeded();
    const top = await c.items.query('SELECT TOP 3 VALUE c.n FROM c ORDER BY c.n').fetchAll();
    assert.deepEqual(top.resources, [0, 1, 2]);
    const off = await c.items.query('SELECT VALUE c.n FROM c ORDER BY c.n OFFSET 10 LIMIT 5').fetchAll();
    assert.deepEqual(off.resources, [10, 11, 12, 13, 14]);
  });
  it('aggregates across partitions: COUNT, SUM, MIN, MAX, AVG', async () => {
    const c = await seeded();
    const r = await c.items.query('SELECT VALUE COUNT(1) FROM c').fetchAll();
    assert.deepEqual(r.resources, [60]);
    const s = await c.items.query('SELECT VALUE SUM(c.n) FROM c').fetchAll();
    assert.deepEqual(s.resources, [1770]);
    const avg = await c.items.query('SELECT VALUE AVG(c.n) FROM c').fetchAll();
    assert.deepEqual(avg.resources, [29.5]);
    // several aggregates in one projection across partitions is refused by the SDK itself ("must use the VALUE keyword"); inside one partition it is sent to the service
    const one = await c.items.query('SELECT MIN(c.n) AS lo, MAX(c.n) AS hi FROM c', { partitionKey: 'p0' }).fetchAll();
    assert.deepEqual(one.resources, [{ lo: 0, hi: 56 }]);
  });
  it('MIN and MAX across partitions (the SDK merges the per-partition answers itself)', async () => {
    const c = await seeded();
    assert.deepEqual((await c.items.query('SELECT VALUE MIN(c.n) FROM c').fetchAll()).resources, [0]);
    assert.deepEqual((await c.items.query('SELECT VALUE MAX(c.n) FROM c').fetchAll()).resources, [59]);
  });
  it('MIN and MAX inside one partition key (the service answers directly)', async () => {
    const c = await seeded();
    assert.deepEqual((await c.items.query('SELECT VALUE MIN(c.n) FROM c', { partitionKey: 'p0' }).fetchAll()).resources, [0]);
    assert.deepEqual((await c.items.query('SELECT VALUE MAX(c.n) FROM c', { partitionKey: 'p0' }).fetchAll()).resources, [56]);
  });
  it('GROUP BY across partitions', async () => {
    const c = await seeded();
    const r = await c.items.query('SELECT c.pk, COUNT(1) AS cnt FROM c GROUP BY c.pk').fetchAll();
    assert.deepEqual(r.resources.sort((a, b) => a.pk.localeCompare(b.pk)), [{ pk: 'p0', cnt: 15 }, { pk: 'p1', cnt: 15 }, { pk: 'p2', cnt: 15 }, { pk: 'p3', cnt: 15 }]);
  });
  it('DISTINCT and ARRAY_CONTAINS and string functions', async () => {
    const c = await seeded();
    const d = await c.items.query('SELECT DISTINCT VALUE c.pk FROM c').fetchAll();
    assert.deepEqual(d.resources.sort(), ['p0', 'p1', 'p2', 'p3']);
    const a = await c.items.query("SELECT VALUE COUNT(1) FROM c WHERE ARRAY_CONTAINS(c.tags, 'odd')").fetchAll();
    assert.deepEqual(a.resources, [30]);
    const s = await c.items.query("SELECT VALUE c.id FROM c WHERE STARTSWITH(c.name, 'name5') AND CONTAINS(c.name, '9')").fetchAll();
    assert.deepEqual(s.resources, ['i59']);
  });
  it('JOIN over an array property', async () => {
    const c = await seeded();
    const r = await c.items.query("SELECT VALUE COUNT(1) FROM c JOIN t IN c.tags WHERE t = 'all'").fetchAll();
    assert.deepEqual(r.resources, [60]);
  });
  it('paging with maxItemCount: fetchNext returns pages of the given size and a continuation token that resumes', async () => {
    const c = await seeded();
    const it1 = c.items.query('SELECT * FROM c', { maxItemCount: 7, partitionKey: 'p0' });
    const page1 = await it1.fetchNext();
    assert.equal(page1.resources.length, 7);
    assert.ok(page1.hasMoreResults);
    assert.ok(page1.continuationToken);
    const it2 = c.items.query('SELECT * FROM c', { maxItemCount: 100, partitionKey: 'p0', continuationToken: page1.continuationToken });
    const rest = await it2.fetchAll();
    assert.equal(rest.resources.length, 8);
    const ids = new Set([...page1.resources, ...rest.resources].map((x) => x.id));
    assert.equal(ids.size, 15);
  });
  it('a cross-partition query with maxItemCount pages through every document exactly once', async () => {
    const c = await seeded();
    const it1 = c.items.query('SELECT c.id FROM c', { maxItemCount: 8 });
    const ids = [];
    while (it1.hasMoreResults()) {
      const p = await it1.fetchNext();
      assert.ok(p.resources.length <= 8);
      ids.push(...p.resources.map((x) => x.id));
    }
    assert.equal(ids.length, 60);
    assert.equal(new Set(ids).size, 60);
  });
  it('asyncIterator over items.readAll yields all documents', async () => {
    const c = await seeded();
    const ids = [];
    for await (const page of c.items.readAll({ maxItemCount: 20 }).getAsyncIterator()) ids.push(...page.resources.map((x) => x.id));
    assert.equal(ids.length, 60);
  });
  it('a query with a syntax error is 400', async () => {
    const c = await seeded();
    const e = await rejection(() => c.items.query('SELEKT * FROM c').fetchAll());
    assert.equal(e.code, 400);
  });
  it('a query with a parameter of each JSON type matches stored values', async () => {
    const { container } = await newContainer();
    await container.items.create({ id: '1', pk: 'p', s: 'x', n: 1.5, b: true, z: null, a: [1], o: { k: 1 } });
    for (const [field, v] of [['s', 'x'], ['n', 1.5], ['b', true], ['z', null]]) {
      const r = await container.items.query({ query: `SELECT VALUE c.id FROM c WHERE c.${field} = @v`, parameters: [{ name: '@v', value: v }] }).fetchAll();
      assert.deepEqual(r.resources, ['1'], field);
    }
  });
  it('SELECT with TOP over a cross-partition query without ORDER BY returns that many', async () => {
    const c = await seeded();
    const r = await c.items.query('SELECT TOP 5 * FROM c').fetchAll();
    assert.equal(r.resources.length, 5);
  });
});

describe('change feed', () => {
  it('reading from the beginning returns every document and a continuation that returns 304 (no changes) next', async () => {
    const { container } = await newContainer();
    for (let i = 0; i < 5; i++) await container.items.create({ id: `c${i}`, pk: 'p' });
    const it = container.items.getChangeFeedIterator({ changeFeedStartFrom: ChangeFeedStartFrom.Beginning() });
    const seen = [];
    let token;
    while (it.hasMoreResults) {
      const r = await it.readNext();
      if (r.statusCode === 304) { token = r.continuationToken; break; }
      seen.push(...r.result.map((x) => x.id));
      token = r.continuationToken;
    }
    assert.deepEqual(seen.sort(), ['c0', 'c1', 'c2', 'c3', 'c4']);
    await container.items.create({ id: 'later', pk: 'p' });
    const it2 = container.items.getChangeFeedIterator({ changeFeedStartFrom: ChangeFeedStartFrom.Continuation(token) });
    const r2 = await it2.readNext();
    assert.deepEqual(r2.result.map((x) => x.id), ['later']);
  });
  it('a change feed for one partition key returns only that partition and an update appears as a new version', async () => {
    const { container } = await newContainer();
    await container.items.create({ id: 'a', pk: 'p1', v: 1 });
    await container.items.create({ id: 'b', pk: 'p2', v: 1 });
    await container.item('a', 'p1').replace({ id: 'a', pk: 'p1', v: 2 });
    const it = container.items.getChangeFeedIterator({ changeFeedStartFrom: ChangeFeedStartFrom.Beginning('p1') });
    const r = await it.readNext();
    assert.deepEqual(r.result.map((x) => [x.id, x.v]), [['a', 2]]);
  });
  it('Now() starts after the existing items', async () => {
    const { container } = await newContainer();
    await container.items.create({ id: 'before', pk: 'p' });
    const it = container.items.getChangeFeedIterator({ changeFeedStartFrom: ChangeFeedStartFrom.Now() });
    const first = await it.readNext();
    assert.equal(first.statusCode, 304);
    await container.items.create({ id: 'after', pk: 'p' });
    const second = await it.readNext();
    assert.deepEqual(second.result.map((x) => x.id), ['after']);
  });
  it('a feed range iterator returns the documents in that range', async () => {
    const { container } = await newContainer();
    for (let i = 0; i < 6; i++) await container.items.create({ id: `c${i}`, pk: `p${i}` });
    const ranges = await container.getFeedRanges();
    assert.ok(ranges.length >= 1);
    const seen = [];
    for (const range of ranges) {
      const it = container.items.getChangeFeedIterator({ changeFeedStartFrom: ChangeFeedStartFrom.Beginning(range) });
      const r = await it.readNext();
      seen.push(...r.result.map((x) => x.id));
    }
    assert.equal(new Set(seen).size, 6);
  });
});

describe('TTL', () => {
  it('an item with ttl of 2 seconds disappears when the container default TTL is on', async () => {
    const { database } = await newContainer();
    const { container } = await database.containers.create({ id: uniq('c'), partitionKey: { paths: ['/pk'] }, defaultTtl: -1 });
    await container.items.create({ id: 'short', pk: 'p', ttl: 2 });
    await container.items.create({ id: 'long', pk: 'p' });
    assert.equal((await container.item('short', 'p').read()).statusCode, 200);
    await sleep(3500);
    assert.equal((await container.item('short', 'p').read()).statusCode, 404);
    assert.equal((await container.item('long', 'p').read()).statusCode, 200);
  });
  it('the container default TTL expires items that have no ttl of their own', async () => {
    const { database } = await newContainer();
    const { container } = await database.containers.create({ id: uniq('c'), partitionKey: { paths: ['/pk'] }, defaultTtl: 2 });
    await container.items.create({ id: 'a', pk: 'p' });
    await sleep(3500);
    assert.equal((await container.item('a', 'p').read()).statusCode, 404);
  });
});
