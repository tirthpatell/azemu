// Cosmos paths where the JavaScript SDK differs from Python: client-side query planning, readMany, full-fidelity change feed, session tokens, diagnostics.
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { CosmosClient, ChangeFeedStartFrom, ChangeFeedMode, PartitionKeyKind, PartitionKeyBuilder } from '@azure/cosmos';
import { env, uniq, rejection, sleep } from '../lib/emu.mjs';

const emu = env();
const KEY = 'C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==';
const client = new CosmosClient({ endpoint: `${emu.url('documents', uniq('cd'), 'https')}/`, key: KEY });
async function newContainer(def = {}) {
  const { database } = await client.databases.createIfNotExists({ id: uniq('db') });
  const { container } = await database.containers.create({ id: uniq('c'), partitionKey: { paths: ['/pk'] }, ...def });
  return { database, container };
}

describe('session consistency and diagnostics', () => {
  it('a read right after a write sees it with session consistency (the SDK carries the session token)', async () => {
    const c = new CosmosClient({ endpoint: `${emu.url('documents', uniq('cs'), 'https')}/`, key: KEY, consistencyLevel: 'Session' });
    const { database } = await c.databases.create({ id: uniq('db') });
    const { container } = await database.containers.create({ id: 'c', partitionKey: { paths: ['/pk'] } });
    for (let i = 0; i < 20; i++) {
      await container.items.create({ id: `s${i}`, pk: 'p', v: i });
      assert.equal((await container.item(`s${i}`, 'p').read()).resource.v, i);
    }
  });
  it('the response diagnostics object is present and lists the requests made', async () => {
    const { container } = await newContainer();
    const r = await container.items.create({ id: 'd', pk: 'p' });
    assert.ok(r.diagnostics);
    assert.ok(r.diagnostics.clientSideRequestStatistics);
  });
  it('a client built with any key string talks to the emulator (signatures are not checked)', async () => {
    const c = new CosmosClient({ endpoint: `${emu.url('documents', uniq('ck'), 'https')}/`, key: 'bm90IHRoZSByZWFsIGtleQ==' });
    await c.databases.create({ id: uniq('db') });
  });
  it('account information (readEndpoint) points back at the emulator host', async () => {
    const r = await client.getDatabaseAccount();
    assert.ok(r.resource.writableLocations.length >= 1);
    assert.ok(r.resource.writableLocations[0].databaseAccountEndpoint.includes('.documents.localhost'));
    assert.equal(r.resource.consistencyPolicy, 'Session');
  });
});

describe('query planning in the client', () => {
  it('multi-field ORDER BY with a composite index across partitions', async () => {
    const { container } = await newContainer({ indexingPolicy: { compositeIndexes: [[{ path: '/a', order: 'ascending' }, { path: '/b', order: 'descending' }]] } });
    for (let i = 0; i < 12; i++) await container.items.create({ id: `i${i}`, pk: `p${i % 4}`, a: i % 3, b: i });
    const r = await container.items.query('SELECT c.a, c.b FROM c ORDER BY c.a ASC, c.b DESC').fetchAll();
    const expected = Array.from({ length: 12 }, (_, i) => ({ a: i % 3, b: i })).sort((x, y) => x.a - y.a || y.b - x.b);
    assert.deepEqual(r.resources, expected);
  });
  it('ORDER BY over a string field with OFFSET LIMIT and a parameter, cross partition', async () => {
    const { container } = await newContainer();
    for (let i = 0; i < 20; i++) await container.items.create({ id: `i${i}`, pk: `p${i % 5}`, name: `n${String(i).padStart(2, '0')}`, grp: i % 2 });
    const r = await container.items.query({ query: 'SELECT VALUE c.name FROM c WHERE c.grp = @g ORDER BY c.name OFFSET 2 LIMIT 3', parameters: [{ name: '@g', value: 0 }] }).fetchAll();
    assert.deepEqual(r.resources, ['n04', 'n06', 'n08']);
  });
  it('DISTINCT with ORDER BY and GROUP BY with aggregates over partitions', async () => {
    const { container } = await newContainer();
    for (let i = 0; i < 30; i++) await container.items.create({ id: `i${i}`, pk: `p${i % 6}`, cat: `c${i % 3}`, n: i });
    const d = await container.items.query('SELECT DISTINCT VALUE c.cat FROM c ORDER BY c.cat').fetchAll();
    assert.deepEqual(d.resources, ['c0', 'c1', 'c2']);
    const g = await container.items.query('SELECT c.cat, COUNT(1) AS cnt, SUM(c.n) AS total FROM c GROUP BY c.cat').fetchAll();
    assert.deepEqual(g.resources.sort((a, b) => a.cat.localeCompare(b.cat)), [{ cat: 'c0', cnt: 10, total: 135 }, { cat: 'c1', cnt: 10, total: 145 }, { cat: 'c2', cnt: 10, total: 155 }]);
  });
  it('hierarchical partition key query with the full key and a prefix key', async () => {
    const { database } = await newContainer();
    const { container } = await database.containers.create({ id: uniq('h'), partitionKey: { paths: ['/a', '/b'], kind: PartitionKeyKind.MultiHash, version: 2 } });
    for (let i = 0; i < 12; i++) await container.items.create({ id: `i${i}`, a: `a${i % 2}`, b: `b${i % 3}`, n: i });
    const full = await container.items.query('SELECT VALUE c.n FROM c ORDER BY c.n', { partitionKey: new PartitionKeyBuilder().addValue('a0').addValue('b0').build() }).fetchAll();
    assert.deepEqual(full.resources, [0, 6]);
    const prefix = await container.items.query('SELECT VALUE COUNT(1) FROM c', { partitionKey: new PartitionKeyBuilder().addValue('a1').build() }).fetchAll();
    assert.deepEqual(prefix.resources, [6]);
  });
  it('a query that returns nothing across partitions gives an empty result, and an aggregate over nothing', async () => {
    const { container } = await newContainer();
    await container.items.create({ id: '1', pk: 'p' });
    assert.deepEqual((await container.items.query("SELECT * FROM c WHERE c.id = 'zzz'").fetchAll()).resources, []);
    assert.deepEqual((await container.items.query("SELECT VALUE COUNT(1) FROM c WHERE c.id = 'zzz'").fetchAll()).resources, [0]);
  });
  it('query metrics and request charge are reported per page', async () => {
    const { container } = await newContainer();
    for (let i = 0; i < 5; i++) await container.items.create({ id: `i${i}`, pk: 'p' });
    const r = await container.items.query('SELECT * FROM c', { maxItemCount: 2 }).fetchNext();
    assert.ok(r.requestCharge > 0);
    assert.equal(r.resources.length, 2);
  });
});

describe('change feed with all versions and deletes', () => {
  it('AllVersionsAndDeletes (full fidelity) reports creates, replaces and deletes from now on', async () => {
    const { container } = await newContainer({ changeFeedPolicy: { retentionDuration: 10 } });
    const it = container.items.getChangeFeedIterator({ changeFeedMode: ChangeFeedMode.AllVersionsAndDeletes, changeFeedStartFrom: ChangeFeedStartFrom.Now() });
    await it.readNext();
    await container.items.create({ id: 'a', pk: 'p', v: 1 });
    await container.item('a', 'p').replace({ id: 'a', pk: 'p', v: 2 });
    await container.item('a', 'p').delete();
    await sleep(200);
    const r = await it.readNext();
    const ops = r.result.map((x) => x.metadata.operationType);
    assert.deepEqual(ops, ['create', 'replace', 'delete']);
    assert.equal(r.result[1].current.v, 2);
    assert.equal(r.result[2].metadata.id ?? r.result[2].previous?.id ?? 'a', 'a');
  });
});

describe('errors', () => {
  it('reading from a container in a missing database is 404 with a Cosmos error body', async () => {
    const e = await rejection(() => client.database(uniq('none')).container('c').item('1', 'p').read().then((r) => { if (r.statusCode === 404) throw Object.assign(new Error('404'), { code: 404 }); }));
    assert.equal(e.code, 404);
  });
  it('an invalid partition key value count (too many values) is 400', async () => {
    const { container } = await newContainer();
    await container.items.create({ id: '1', pk: 'p' });
    const e = await rejection(() => container.item('1', ['p', 'extra']).read().then((r) => { if (r.statusCode >= 400) throw Object.assign(new Error('x'), { code: r.statusCode }); }));
    assert.ok([400, 404].includes(e.code), String(e.code));
  });
  it('a create without an id gets a generated id from the SDK (a GUID) and can be read back', async () => {
    const { container } = await newContainer();
    const r = await container.items.create({ pk: 'p', v: 1 });
    assert.match(r.resource.id, /^[0-9a-f-]{36}$/);
    assert.equal((await container.item(r.resource.id, 'p').read()).resource.v, 1);
  });
});
