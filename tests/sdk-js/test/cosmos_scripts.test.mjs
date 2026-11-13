// Some code samples and query examples in this file are reproduced from Microsoft Learn and from Microsoft's SDKs (MIT licence, Copyright Microsoft); see NOTICE and LICENSES/MIT-Microsoft.txt.
// Server-side scripts through @azure/cosmos: stored procedure execution (sample scripts of
// learn.microsoft.com/azure/cosmos-db/nosql/how-to-write-stored-procedures-triggers-udfs), triggers on item operations, and a UDF in a query.
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { CosmosClient, TriggerType, TriggerOperation } from '@azure/cosmos';
import { env, uniq, rejection } from '../lib/emu.mjs';

const emu = env();
const KEY = 'C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==';
const client = new CosmosClient({ endpoint: `${emu.url('documents', uniq('cosmosjs'), 'https')}/`, key: KEY });

async function newContainer() {
  const { database } = await client.databases.createIfNotExists({ id: uniq('db') });
  const { container } = await database.containers.createIfNotExists({ id: uniq('c'), partitionKey: { paths: ['/pk'] } });
  return container;
}

const ids = async (container, pk) => (await container.items.query('SELECT * FROM c', { partitionKey: pk }).fetchAll()).resources.map((d) => d.id).sort();

const CREATE_ITEMS = `function createToDoItems(items) {
    var collection = getContext().getCollection();
    var collectionLink = collection.getSelfLink();
    var count = 0;
    if (!items) throw new Error("The array is undefined or null.");
    var numItems = items.length;
    if (numItems == 0) { getContext().getResponse().setBody(0); return; }
    tryCreate(items[count], callback);
    function tryCreate(item, callback) {
        var options = { disableAutomaticIdGeneration: false };
        var isAccepted = collection.createDocument(collectionLink, item, options, callback);
        if (!isAccepted) getContext().getResponse().setBody(count);
    }
    function callback(err, item, options) {
        if (err) throw err;
        count++;
        if (count >= numItems) { getContext().getResponse().setBody(count); } else { tryCreate(items[count], callback); }
    }
}`;

describe('stored procedures', () => {
  it('hello world returns the response body', async () => {
    const c = await newContainer();
    await c.scripts.storedProcedures.create({ id: 'hello', body: 'function () { var context = getContext(); var response = context.getResponse(); response.setBody("Hello, World"); }' });
    const { resource, statusCode, requestCharge } = await c.scripts.storedProcedure('hello').execute('p');
    assert.equal(statusCode, 200);
    assert.equal(resource, 'Hello, World');
    assert.ok(requestCharge > 0);
  });

  it('creates documents from the parameters; an exception rolls them all back with a 400', async () => {
    const c = await newContainer();
    await c.scripts.storedProcedures.create({ id: 'create', body: CREATE_ITEMS });
    const sp = c.scripts.storedProcedure('create');
    const ok = await sp.execute('p', [[{ id: 'a', pk: 'p' }, { id: 'b', pk: 'p' }, { pk: 'p', name: 'generated id' }]]);
    assert.equal(ok.resource, 3);
    assert.equal((await ids(c, 'p')).length, 3);
    const e = await rejection(() => sp.execute('q', [[{ id: 'x', pk: 'q' }, { id: 'y', pk: 'q' }, { id: 'x', pk: 'q' }]]));
    assert.equal(e.code, 400);
    assert.match(e.message, /Encountered exception while executing function/);
    assert.deepEqual(await ids(c, 'q'), []);
  });

  it('script logging is returned in x-ms-documentdb-script-log-results when asked for', async () => {
    const c = await newContainer();
    await c.scripts.storedProcedures.create({ id: 'log', body: "function () { console.log('hello'); getContext().getResponse().setBody(1); }" });
    const withLog = await c.scripts.storedProcedure('log').execute('p', [], { enableScriptLogging: true });
    assert.equal(decodeURIComponent(withLog.headers['x-ms-documentdb-script-log-results']), 'hello');
    const without = await c.scripts.storedProcedure('log').execute('p', []);
    assert.equal(without.headers['x-ms-documentdb-script-log-results'], undefined);
  });

  it('bulk import answers with how many it did when a call is not accepted, and the client calls again', async () => {
    const c = await newContainer();
    const body = `function bulkImport(items) {
      var container = getContext().getCollection(); var link = container.getSelfLink(); var count = 0;
      if (items.length == 0) getContext().getResponse().setBody(0);
      tryCreate(items[count], callback);
      function tryCreate(item, callback) { var ok = container.createDocument(link, item, callback); if (!ok) getContext().getResponse().setBody(count); }
      function callback(err) { if (err) throw err; count++; if (count >= items.length) getContext().getResponse().setBody(count); else tryCreate(items[count], callback); }
    }`;
    await c.scripts.storedProcedures.create({ id: 'import', body });
    const items = Array.from({ length: 40 }, (_, i) => ({ id: `i${i}`, pk: 'p', n: i }));
    let done = 0;
    for (let calls = 0; done < items.length && calls < 20; calls++) {
      const { resource } = await c.scripts.storedProcedure('import').execute('p', [items.slice(done)]);
      assert.ok(resource > 0);
      done += resource;
    }
    assert.equal((await ids(c, 'p')).length, 40);
  });

  it('a script that loops forever is stopped with an error, and the container still works', async () => {
    const c = await newContainer();
    await c.scripts.storedProcedures.create({ id: 'spin', body: 'function () { while (true) {} }' });
    const e = await rejection(() => c.scripts.storedProcedure('spin').execute('p'));
    assert.ok(e.code === 400 || e.code === 408, `status ${e.code}`);
    await c.items.create({ id: 'after', pk: 'p' });
    assert.deepEqual(await ids(c, 'p'), ['after']);
  });
});

describe('triggers', () => {
  it('a pre-trigger changes the item that is created, replaced or upserted', async () => {
    const c = await newContainer();
    await c.scripts.triggers.create({
      id: 'stamp', triggerType: TriggerType.Pre, triggerOperation: TriggerOperation.All,
      body: 'function () { var r = getContext().getRequest(); var b = r.getBody(); b.stamped = r.getOperationType(); r.setBody(b); }',
    });
    const created = await c.items.create({ id: 'a', pk: 'p' }, { preTriggerInclude: ['stamp'] });
    assert.equal(created.resource.stamped, 'Create');
    const replaced = await c.item('a', 'p').replace({ id: 'a', pk: 'p', v: 2 }, { preTriggerInclude: ['stamp'] });
    assert.equal(replaced.resource.stamped, 'Replace');
    const upserted = await c.items.upsert({ id: 'a', pk: 'p', v: 3 }, { preTriggerInclude: ['stamp'] });
    assert.equal(upserted.resource.stamped, 'Upsert');
    const plain = await c.items.create({ id: 'b', pk: 'p' });
    assert.equal(plain.resource.stamped, undefined);
  });

  it('the documented post-trigger updates a metadata document in the same transaction; a throwing one undoes the item', async () => {
    const c = await newContainer();
    await c.scripts.triggers.create({
      id: 'meta', triggerType: 'post', triggerOperation: 'create',
      body: `function updateMetadata() {
        var context = getContext(); var container = context.getCollection(); var response = context.getResponse();
        var createdItem = response.getBody();
        var filterQuery = 'SELECT * FROM root r WHERE r.id = "_metadata"';
        var accept = container.queryDocuments(container.getSelfLink(), filterQuery, updateMetadataCallback);
        if (!accept) throw "Unable to update metadata, abort";
        function updateMetadataCallback(err, items, responseOptions) {
          if (err) throw new Error("Error" + err.message);
          if (items.length != 1) throw 'Unable to find metadata document';
          var metadataItem = items[0];
          metadataItem.createdItems += 1;
          metadataItem.createdNames += " " + createdItem.id;
          var accept = container.replaceDocument(metadataItem._self, metadataItem, function (err, itemReplaced) { if (err) throw "Unable to update metadata, abort"; });
          if (!accept) throw "Unable to update metadata, abort";
        }
      }`,
    });
    await c.items.create({ id: '_metadata', pk: 'p', createdItems: 0, createdNames: '' });
    await c.items.create({ id: 'one', pk: 'p' }, { postTriggerInclude: ['meta'] });
    await c.items.create({ id: 'two', pk: 'p' }, { postTriggerInclude: ['meta'] });
    const { resource } = await c.item('_metadata', 'p').read();
    assert.equal(resource.createdItems, 2);
    assert.equal(resource.createdNames, ' one two');
    const e = await rejection(() => c.items.create({ id: 'no-meta', pk: 'other' }, { postTriggerInclude: ['meta'] }));
    assert.equal(e.code, 400);
    assert.deepEqual(await ids(c, 'other'), []);
  });

  it('a trigger of the wrong type or operation, or an unknown one, is a 400', async () => {
    const c = await newContainer();
    await c.scripts.triggers.create({ id: 'createonly', triggerType: 'pre', triggerOperation: 'create', body: 'function () {}' });
    await c.items.create({ id: 'a', pk: 'p' });
    const e1 = await rejection(() => c.item('a', 'p').replace({ id: 'a', pk: 'p' }, { preTriggerInclude: ['createonly'] }));
    assert.equal(e1.code, 400);
    const e2 = await rejection(() => c.items.create({ id: 'b', pk: 'p' }, { postTriggerInclude: ['createonly'] }));
    assert.equal(e2.code, 400);
    const e3 = await rejection(() => c.items.create({ id: 'b', pk: 'p' }, { preTriggerInclude: ['missing'] }));
    assert.equal(e3.code, 400);
  });
});

describe('user-defined functions', () => {
  it('the tax sample runs in SELECT, WHERE and cross-partition queries', async () => {
    const c = await newContainer();
    await c.scripts.userDefinedFunctions.create({
      id: 'Tax',
      body: "function tax(income) { if (income == undefined) throw 'no input'; if (income < 1000) return income * 0.1; else if (income < 10000) return income * 0.2; else return income * 0.4; }",
    });
    for (const [i, income] of [500, 5000, 70000, 100000].entries()) await c.items.create({ id: `i${i}`, pk: i % 2 ? 'a' : 'b', income });
    const where = await c.items.query('SELECT * FROM Incomes t WHERE udf.Tax(t.income) > 20000').fetchAll();
    assert.deepEqual(where.resources.map((d) => d.id).sort(), ['i2', 'i3']);
    const select = await c.items.query('SELECT t.id, udf.Tax(t.income) AS tax FROM t').fetchAll();
    assert.deepEqual(select.resources.map((d) => d.tax).sort((a, b) => a - b), [50, 1000, 28000, 40000]);
    const e = await rejection(() => c.items.query('SELECT VALUE udf.Tax(t.nothing) FROM t', { partitionKey: 'a' }).fetchAll());
    assert.equal(e.code, 400);
    assert.match(e.message, /no input/);
  });
});
