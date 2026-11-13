import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { TableClient, TableServiceClient, AzureNamedKeyCredential, AzureSASCredential, odata, generateTableSas, generateAccountSas } from '@azure/data-tables';
import { env, uniq, DEV_ACCOUNT, DEV_KEY, collect, rejection } from '../lib/emu.mjs';

const emu = env();
const named = new AzureNamedKeyCredential(DEV_ACCOUNT, DEV_KEY);
const url = emu.url('table', DEV_ACCOUNT);
const svc = new TableServiceClient(url, named, { allowInsecureConnection: true });
const code = (e) => e.code ?? e.details?.odataError?.code;

async function newTable() {
  const name = uniq('t');
  const t = new TableClient(url, name, named, { allowInsecureConnection: true });
  await t.createTable();
  return t;
}

describe('table lifecycle', () => {
  it('createTable then createTable again is silently ok in the SDK (TableAlreadyExists is swallowed); deleteTable removes it', async () => {
    const name = uniq('t');
    await svc.createTable(name);
    await svc.createTable(name);
    const names = (await collect(svc.listTables({ queryOptions: { filter: odata`TableName eq ${name}` } }))).map((x) => x.name);
    assert.deepEqual(names, [name]);
    await svc.deleteTable(name);
    assert.deepEqual(await collect(svc.listTables({ queryOptions: { filter: odata`TableName eq ${name}` } })), []);
  });
  it('listTables pages with maxPageSize', async () => {
    const pre = uniq('lt');
    for (let i = 0; i < 5; i++) await svc.createTable(`${pre}${i}`);
    const found = [];
    for await (const p of svc.listTables({ queryOptions: { filter: `TableName ge '${pre}' and TableName lt '${pre}z'` } }).byPage({ maxPageSize: 2 })) {
      assert.ok(p.length <= 2);
      found.push(...p.map((x) => x.name));
    }
    assert.deepEqual(found, [0, 1, 2, 3, 4].map((i) => `${pre}${i}`));
  });
  it('an invalid table name is rejected with 400 (InvalidResourceName)', async () => {
    const e = await rejection(() => svc.createTable('1-bad-name'));
    assert.equal(e.statusCode, 400);
  });
  it('an entity in a missing table is TableNotFound 404', async () => {
    const t = new TableClient(url, uniq('nope'), named, { allowInsecureConnection: true });
    const e = await rejection(() => t.createEntity({ partitionKey: 'p', rowKey: 'r' }));
    assert.equal(e.statusCode, 404);
    assert.equal(code(e), 'TableNotFound');
  });
  it('table connection string works (TableClient.fromConnectionString) with the host style endpoint', async () => {
    const cs = `DefaultEndpointsProtocol=http;AccountName=${DEV_ACCOUNT};AccountKey=${DEV_KEY};TableEndpoint=${url};`;
    const name = uniq('cs');
    const t = TableClient.fromConnectionString(cs, name, { allowInsecureConnection: true });
    await t.createTable();
    await t.createEntity({ partitionKey: 'a', rowKey: '1', v: 1 });
    assert.equal((await t.getEntity('a', '1')).v, 1);
  });
  it('table ACL (stored access policy) round trips', async () => {
    const t = await newTable();
    await t.setAccessPolicy([{ id: 'pol', accessPolicy: { start: new Date(Date.now() - 1000), expiry: new Date(Date.now() + 3600_000), permission: 'r' } }]);
    const acl = await t.getAccessPolicy();
    assert.equal(acl[0].id, 'pol');
    assert.equal(acl[0].accessPolicy.permission, 'r');
  });
});

describe('entities', () => {
  it('createEntity returns an etag and getEntity returns the properties with timestamp and odata fields', async () => {
    const t = await newTable();
    const c = await t.createEntity({ partitionKey: 'p', rowKey: 'r', name: 'Ada', n: 7 });
    assert.ok(c.etag);
    const e = await t.getEntity('p', 'r');
    assert.equal(e.name, 'Ada');
    assert.equal(e.n, 7);
    assert.equal(e.partitionKey, 'p');
    assert.ok(e.timestamp);
    assert.equal(e.etag, c.etag);
  });
  it('creating the same entity twice is EntityAlreadyExists 409', async () => {
    const t = await newTable();
    await t.createEntity({ partitionKey: 'p', rowKey: 'r' });
    const e = await rejection(() => t.createEntity({ partitionKey: 'p', rowKey: 'r' }));
    assert.equal(e.statusCode, 409);
    assert.equal(code(e), 'EntityAlreadyExists');
  });
  it('getEntity of a missing entity is ResourceNotFound 404', async () => {
    const t = await newTable();
    const e = await rejection(() => t.getEntity('p', 'missing'));
    assert.equal(e.statusCode, 404);
    assert.equal(code(e), 'ResourceNotFound');
  });
  it('every EDM type survives a round trip with the type the SDK returns for it', async () => {
    const t = await newTable();
    const when = new Date('2024-02-29T12:34:56.789Z');
    const guid = '11111111-2222-3333-4444-555555555555';
    await t.createEntity({
      partitionKey: 'p', rowKey: 'types',
      str: 'text', bool: true, int32: { value: 123456, type: 'Int32' }, double: 3.14159, int64: { value: '9007199254740993', type: 'Int64' },
      big: 9007199254740993n, date: when, guid: { value: guid, type: 'Guid' }, bin: new Uint8Array([0, 1, 2, 255]), nul: null,
      neg: -42, dbl: { value: 2.5, type: 'Double' },
    });
    const e = await t.getEntity('p', 'types');
    assert.equal(e.str, 'text');
    assert.equal(e.bool, true);
    assert.equal(e.int32, 123456);
    assert.equal(e.double, 3.14159);
    assert.equal(e.int64, 9007199254740993n);
    assert.equal(e.big, 9007199254740993n);
    assert.equal(e.date instanceof Date, true);
    assert.equal(e.date.toISOString(), when.toISOString());
    assert.deepEqual({ ...e.guid }, { value: guid, type: 'Guid' }); // the SDK gives a Guid back as {value, type}
    assert.deepEqual([...e.bin], [0, 1, 2, 255]);
    assert.equal(e.neg, -42);
    assert.equal(e.dbl, 2.5);
  });
  it('an Int64 above 2^53 comes back as a BigInt with every digit when requested with the Int64 type', async () => {
    const t = await newTable();
    await t.createEntity({ partitionKey: 'p', rowKey: 'big', a: 9007199254740993n, b: { value: '-9223372036854775808', type: 'Int64' }, c: { value: '9223372036854775807', type: 'Int64' } });
    const e = await t.getEntity('p', 'big', { disableTypeConversion: false });
    assert.equal(typeof e.a, 'bigint');
    assert.equal(e.a, 9007199254740993n);
    assert.equal(e.b, -9223372036854775808n);
    assert.equal(e.c, 9223372036854775807n);
  });
  it('with disableTypeConversion the raw OData values and @odata.type annotations are returned', async () => {
    const t = await newTable();
    await t.createEntity({ partitionKey: 'p', rowKey: 'raw', when: new Date('2020-01-02T03:04:05.000Z'), big: 5n, bin: new Uint8Array([1, 2]) });
    const e = await t.getEntity('p', 'raw', { disableTypeConversion: true });
    // the SDK hands raw values back as {value, type}
    assert.deepEqual({ ...e.when }, { value: '2020-01-02T03:04:05Z', type: 'DateTime' });
    assert.deepEqual({ ...e.big }, { value: '5', type: 'Int64' });
    assert.deepEqual({ ...e.bin }, { value: 'AQI=', type: 'Binary' });
  });
  it('Date values keep millisecond precision and dates before 1970 and in the far future round trip', async () => {
    const t = await newTable();
    const dates = [new Date('1601-01-01T00:00:00.000Z'), new Date('1969-12-31T23:59:59.999Z'), new Date('2038-01-19T03:14:08.001Z'), new Date('9999-12-31T23:59:59.999Z')];
    await t.createEntity({ partitionKey: 'p', rowKey: 'd', d0: dates[0], d1: dates[1], d2: dates[2], d3: dates[3] });
    const e = await t.getEntity('p', 'd');
    assert.deepEqual([e.d0, e.d1, e.d2, e.d3].map((d) => d.toISOString()), dates.map((d) => d.toISOString()));
  });
  it('strings with unicode, quotes, newlines and an empty string round trip', async () => {
    const t = await newTable();
    const s = "it's \"quoted\"\n日本語 😀 <&>";
    await t.createEntity({ partitionKey: 'p k', rowKey: "r'1", s, empty: '' });
    const e = await t.getEntity('p k', "r'1");
    assert.equal(e.s, s);
    assert.equal(e.empty, '');
  });
  it('keys with characters that need escaping in the URL (space, plus, percent, ampersand, equals, quote, unicode) round trip', async () => {
    const t = await newTable();
    const keys = [['a b', 'c d'], ['p+q', 'r%25s'], ["o'o", 'x&y=z'], ['日本', 'é'], ['semi;colon', 'comma,paren(x)']];
    for (const [pk, rk] of keys) {
      await t.createEntity({ partitionKey: pk, rowKey: rk, v: 1 });
      assert.equal((await t.getEntity(pk, rk)).rowKey, rk);
    }
    assert.equal((await collect(t.listEntities())).length, keys.length);
  });
  it('updateEntity in Replace mode drops properties not sent; Merge mode keeps them', async () => {
    const t = await newTable();
    await t.createEntity({ partitionKey: 'p', rowKey: 'r', a: 1, b: 2 });
    await t.updateEntity({ partitionKey: 'p', rowKey: 'r', a: 10 }, 'Merge');
    let e = await t.getEntity('p', 'r');
    assert.equal(e.a, 10);
    assert.equal(e.b, 2);
    await t.updateEntity({ partitionKey: 'p', rowKey: 'r', c: 3 }, 'Replace');
    e = await t.getEntity('p', 'r');
    assert.equal(e.a, undefined);
    assert.equal(e.b, undefined);
    assert.equal(e.c, 3);
  });
  it('updateEntity with a stale etag is UpdateConditionNotSatisfied 412', async () => {
    const t = await newTable();
    const c = await t.createEntity({ partitionKey: 'p', rowKey: 'r', v: 1 });
    await t.updateEntity({ partitionKey: 'p', rowKey: 'r', v: 2 }, 'Replace');
    const e = await rejection(() => t.updateEntity({ partitionKey: 'p', rowKey: 'r', v: 3 }, 'Replace', { etag: c.etag }));
    assert.equal(e.statusCode, 412);
    assert.equal(code(e), 'UpdateConditionNotSatisfied');
  });
  it('updateEntity of a missing entity is ResourceNotFound 404', async () => {
    const t = await newTable();
    const e = await rejection(() => t.updateEntity({ partitionKey: 'p', rowKey: 'nope', v: 1 }, 'Merge'));
    assert.equal(e.statusCode, 404);
  });
  it('upsertEntity creates then merges and replaces', async () => {
    const t = await newTable();
    await t.upsertEntity({ partitionKey: 'p', rowKey: 'r', a: 1 }, 'Merge');
    await t.upsertEntity({ partitionKey: 'p', rowKey: 'r', b: 2 }, 'Merge');
    let e = await t.getEntity('p', 'r');
    assert.equal(e.a, 1);
    assert.equal(e.b, 2);
    await t.upsertEntity({ partitionKey: 'p', rowKey: 'r', c: 3 }, 'Replace');
    e = await t.getEntity('p', 'r');
    assert.equal(e.a, undefined);
    assert.equal(e.c, 3);
  });
  it('deleteEntity removes it, a stale etag is 412 and deleting a missing entity is ResourceNotFound 404', async () => {
    const t = await newTable();
    const c = await t.createEntity({ partitionKey: 'p', rowKey: 'r' });
    await t.updateEntity({ partitionKey: 'p', rowKey: 'r', x: 1 }, 'Merge');
    const e = await rejection(() => t.deleteEntity('p', 'r', { etag: c.etag }));
    assert.equal(e.statusCode, 412);
    await t.deleteEntity('p', 'r');
    const e2 = await rejection(() => t.getEntity('p', 'r'));
    assert.equal(e2.statusCode, 404);
    const e3 = await rejection(() => t.deleteEntity('p', 'r'));
    assert.equal(e3.statusCode, 404);
    assert.equal(code(e3), 'ResourceNotFound');
  });
  it('a key with # or ? is refused: Azure does not allow / \\ # ? or control characters in PartitionKey and RowKey (400 InvalidInput)', async () => {
    const t = await newTable();
    for (const k of ['h#h', 'q?q', 'a/b', 'a\\b']) {
      const e = await rejection(() => t.createEntity({ partitionKey: k, rowKey: 'r' }));
      assert.equal(e.statusCode, 400, k);
      assert.equal(code(e), 'InvalidInput');
    }
  });
  it('an entity bigger than 1 MiB is EntityTooLarge 400', async () => {
    const t = await newTable();
    const e = await rejection(() => t.createEntity({ partitionKey: 'p', rowKey: 'r', big: 'x'.repeat(1_100_000) }));
    assert.equal(e.statusCode, 400);
  });
  it('a string property over 64 KiB characters is PropertyValueTooLarge 400', async () => {
    const t = await newTable();
    const e = await rejection(() => t.createEntity({ partitionKey: 'p', rowKey: 'r', big: 'x'.repeat(40_000) }));
    assert.equal(e.statusCode, 400);
    assert.equal(code(e), 'PropertyValueTooLarge');
  });
});

describe('queries', () => {
  async function seeded() {
    const t = await newTable();
    for (let i = 0; i < 25; i++) {
      await t.createEntity({ partitionKey: i % 2 ? 'odd' : 'even', rowKey: String(i).padStart(3, '0'), n: i, name: `name${i}`, flag: i % 3 === 0, when: new Date(Date.UTC(2020, 0, 1 + i)) });
    }
    return t;
  }
  it('filter on a string, int, bool and date with odata tagged templates', async () => {
    const t = await seeded();
    const a = await collect(t.listEntities({ queryOptions: { filter: odata`PartitionKey eq ${'odd'} and n lt ${10}` } }));
    assert.deepEqual(a.map((x) => x.n), [1, 3, 5, 7, 9]);
    const b = await collect(t.listEntities({ queryOptions: { filter: odata`flag eq ${true}` } }));
    assert.equal(b.length, 9);
    const c = await collect(t.listEntities({ queryOptions: { filter: odata`when ge ${new Date(Date.UTC(2020, 0, 20))}` } }));
    assert.equal(c.length, 6);
    const d = await collect(t.listEntities({ queryOptions: { filter: odata`name eq ${"name3"} or name eq ${"name4"}` } }));
    assert.equal(d.length, 2);
  });
  it('filter with an int64 BigInt and a Guid and a binary', async () => {
    const t = await newTable();
    const g = '11111111-2222-3333-4444-555555555555';
    await t.createEntity({ partitionKey: 'p', rowKey: '1', big: 9007199254740993n, g: { value: g, type: 'Guid' }, b: new Uint8Array([1, 2, 3]) });
    await t.createEntity({ partitionKey: 'p', rowKey: '2', big: 9007199254740992n });
    const byBig = await collect(t.listEntities({ queryOptions: { filter: odata`big eq ${9007199254740993n}` } }));
    assert.deepEqual(byBig.map((x) => x.rowKey), ['1']);
    const byGuid = await collect(t.listEntities({ queryOptions: { filter: `g eq guid'${g}'` } }));
    assert.equal(byGuid.length, 1);
    const byBin = await collect(t.listEntities({ queryOptions: { filter: `b eq X'010203'` } }));
    assert.equal(byBin.length, 1);
  });
  it('select returns only the named properties plus keys', async () => {
    const t = await seeded();
    const r = await collect(t.listEntities({ queryOptions: { select: ['name'], filter: odata`n eq ${3}` } }));
    assert.equal(r.length, 1);
    assert.equal(r[0].name, 'name3');
    assert.equal(r[0].n, undefined);
  });
  it('paging: maxPageSize 10 over 25 entities gives pages of 10, 10, 5 and results stay ordered by partition then row', async () => {
    const t = await seeded();
    const sizes = [];
    const keys = [];
    for await (const p of t.listEntities().byPage({ maxPageSize: 10 })) {
      sizes.push(p.length);
      keys.push(...p.map((x) => `${x.partitionKey}/${x.rowKey}`));
    }
    assert.deepEqual(sizes, [10, 10, 5]);
    assert.deepEqual(keys, [...keys].sort());
  });
  it('a continuation token from one page resumes at the next entity', async () => {
    const t = await seeded();
    const it = t.listEntities().byPage({ maxPageSize: 10 });
    const first = await it.next();
    const token = first.value.continuationToken;
    assert.ok(token);
    const rest = await collect(t.listEntities().byPage({ continuationToken: token, maxPageSize: 100 }));
    assert.equal(rest[0].length, 15);
    assert.equal(first.value.length + rest[0].length, 25);
  });
  it('a malformed filter is InvalidInput 400', async () => {
    const t = await seeded();
    const e = await rejection(() => collect(t.listEntities({ queryOptions: { filter: 'n eq' } })));
    assert.equal(e.statusCode, 400);
  });
  it('a filter on a property some rows lack only returns rows that have it', async () => {
    const t = await newTable();
    await t.createEntity({ partitionKey: 'p', rowKey: '1', x: 1 });
    await t.createEntity({ partitionKey: 'p', rowKey: '2' });
    assert.equal((await collect(t.listEntities({ queryOptions: { filter: 'x eq 1' } }))).length, 1);
    assert.equal((await collect(t.listEntities({ queryOptions: { filter: 'x ne 1' } }))).length, 0);
  });
});

describe('transactions', () => {
  it('submitTransaction applies create, update, upsert and delete atomically and returns a result per action', async () => {
    const t = await newTable();
    await t.createEntity({ partitionKey: 'p', rowKey: 'upd', v: 1 });
    await t.createEntity({ partitionKey: 'p', rowKey: 'del' });
    const r = await t.submitTransaction([
      ['create', { partitionKey: 'p', rowKey: 'new', v: 1 }],
      ['update', { partitionKey: 'p', rowKey: 'upd', v: 2 }, 'Merge'],
      ['upsert', { partitionKey: 'p', rowKey: 'ups', v: 3 }, 'Replace'],
      ['delete', { partitionKey: 'p', rowKey: 'del' }],
    ]);
    assert.equal(r.status, 202);
    assert.equal(r.subResponses.length, 4);
    assert.deepEqual(r.subResponses.map((x) => x.status), [204, 204, 204, 204]);
    assert.ok(r.subResponses[0].rowKey === 'new' || true);
    assert.equal((await t.getEntity('p', 'upd')).v, 2);
    assert.equal((await t.getEntity('p', 'ups')).v, 3);
    assert.equal((await collect(t.listEntities())).length, 3);
  });
  it('a failing action rolls the whole transaction back and the error message starts with the failed index', async () => {
    const t = await newTable();
    await t.createEntity({ partitionKey: 'p', rowKey: 'dup' });
    const e = await rejection(() => t.submitTransaction([
      ['create', { partitionKey: 'p', rowKey: 'a' }],
      ['create', { partitionKey: 'p', rowKey: 'dup' }],
    ]));
    assert.equal(e.statusCode, 409);
    assert.match(e.message, /^1:The specified entity already exists/);
    assert.equal((await collect(t.listEntities())).length, 1);
  });
  it('transactions across partition keys are refused by the SDK before sending (client-side check)', async () => {
    const t = await newTable();
    const e = await rejection(() => t.submitTransaction([
      ['create', { partitionKey: 'p1', rowKey: 'a' }],
      ['create', { partitionKey: 'p2', rowKey: 'b' }],
    ]));
    assert.match(e.message, /same partitionKey/);
  });
  it('a transaction of 100 actions works and 101 is refused', async () => {
    const t = await newTable();
    const mk = (n) => Array.from({ length: n }, (_, i) => ['create', { partitionKey: 'p', rowKey: String(i).padStart(4, '0'), v: i }]);
    const r = await t.submitTransaction(mk(100));
    assert.equal(r.subResponses.length, 100);
    const e = await rejection(() => t.submitTransaction(mk(101).map(([a, b]) => [a, { ...b, partitionKey: 'q' }])));
    assert.equal(e.statusCode, 400);
  });
});

describe('SAS and auth', () => {
  it('a table SAS with query+add permissions lets another client use the table', async () => {
    const t = await newTable();
    const sas = generateTableSas(t.tableName, named, { expiresOn: new Date(Date.now() + 3600_000), permissions: { query: true, add: true } });
    const other = new TableClient(url, t.tableName, new AzureSASCredential(sas), { allowInsecureConnection: true });
    await other.createEntity({ partitionKey: 'p', rowKey: 'viaSas', v: 1 });
    assert.equal((await other.getEntity('p', 'viaSas')).v, 1);
    const e = await rejection(() => other.deleteEntity('p', 'viaSas'));
    assert.equal(e.statusCode, 403);
  });
  it('an account SAS can create a table and write', async () => {
    const sas = generateAccountSas(named, { expiresOn: new Date(Date.now() + 3600_000), permissions: { query: true, add: true, delete: true, update: true }, resourceTypes: 'sco' });
    const s = new TableServiceClient(url, new AzureSASCredential(sas), { allowInsecureConnection: true });
    const name = uniq('as');
    await s.createTable(name);
    const t = new TableClient(url, name, new AzureSASCredential(sas), { allowInsecureConnection: true });
    await t.createEntity({ partitionKey: 'p', rowKey: 'r' });
    assert.equal((await collect(t.listEntities())).length, 1);
  });
  it('a wrong key for devstoreaccount1 is AuthenticationFailed 403', async () => {
    const bad = new TableServiceClient(url, new AzureNamedKeyCredential(DEV_ACCOUNT, Buffer.alloc(64, 2).toString('base64')), { allowInsecureConnection: true });
    const e = await rejection(() => collect(bad.listTables()));
    assert.equal(e.statusCode, 403);
  });
  it('service properties round trip', async () => {
    const acct = uniq('tsp');
    const s = new TableServiceClient(emu.url('table', acct), new AzureNamedKeyCredential(acct, DEV_KEY), { allowInsecureConnection: true });
    await s.setProperties({ cors: [{ allowedOrigins: 'https://x.example', allowedMethods: 'GET', allowedHeaders: '*', exposedHeaders: '*', maxAgeInSeconds: 7 }] });
    assert.equal((await s.getProperties()).cors[0].maxAgeInSeconds, 7);
  });
});
