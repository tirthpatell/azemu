import { describe, it, after } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { EventHubProducerClient, EventHubConsumerClient, earliestEventPosition, latestEventPosition } from '@azure/event-hubs';
import { ContainerClient, StorageSharedKeyCredential } from '@azure/storage-blob';
import { BlobCheckpointStore } from '@azure/eventhubs-checkpointstore-blob';
import { env, uniq, sleep, rejection, track, closeTracked, DEV_ACCOUNT, DEV_KEY } from '../lib/emu.mjs';

const emu = env();
after(() => closeTracked());
const nsName = uniq('ehns');
const conn = (hub, ns = nsName) => `Endpoint=sb://${ns}.eventhub.localhost:${emu.port};SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true;EntityPath=${hub}`;
const producer = (hub) => track(new EventHubProducerClient(conn(hub)));
const consumer = (hub, group = '$Default', opts) => track(new EventHubConsumerClient(group, conn(hub), opts));
const admin = (path, init) => fetch(`${emu.gateway}/_azemu/eventhubs/${nsName}${path}`, init);
/** Create an event hub (4 partitions) through the admin API before using it, as a team does against Azure; returns its name.
 *  The emulator does not create hubs by being named (AZEMU_EVENTHUBS_AUTOCREATE is off by default). */
async function newHub(prefix = 'h', ns = nsName, body = {}) {
  const name = uniq(prefix);
  const r = await fetch(`${emu.gateway}/_azemu/eventhubs/${ns}/hubs/${name}`, { method: 'PUT', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
  assert.equal(r.status, 201, await r.text());
  return name;
}

/** Subscribe to one partition (or all), wait until `count` events arrived or `ms` passed, then stop. */
async function read(client, partition, count, options = {}, ms = 8000) {
  const events = [];
  const errors = [];
  const handlers = {
    processEvents: async (evs, ctx) => { for (const e of evs) events.push({ e, partitionId: ctx.partitionId }); },
    processError: async (err, ctx) => { errors.push(err); },
  };
  const sub = partition === undefined ? client.subscribe(handlers, options) : client.subscribe(partition, handlers, options);
  const end = Date.now() + ms;
  while (events.length < count && Date.now() < end) await sleep(50);
  if (events.length >= count) await sleep(200);
  await sub.close();
  return { events, errors };
}

describe('producer', () => {
  it('getPartitionIds and getEventHubProperties describe a hub with 4 partitions', async () => {
    const hub = await newHub();
    const p = producer(hub);
    assert.deepEqual(await p.getPartitionIds(), ['0', '1', '2', '3']);
    const props = await p.getEventHubProperties();
    assert.equal(props.name, hub);
    assert.deepEqual(props.partitionIds, ['0', '1', '2', '3']);
    assert.ok(props.createdOn instanceof Date);
  });
  it('getPartitionProperties of an empty partition is isEmpty with -1 sequence numbers and follows the last event sent', async () => {
    const hub = await newHub();
    const p = producer(hub);
    const empty = await p.getPartitionProperties('1');
    assert.equal(empty.isEmpty, true);
    assert.equal(empty.lastEnqueuedSequenceNumber, -1);
    assert.equal(empty.partitionId, '1');
    assert.equal(empty.eventHubName, hub);
    await p.sendBatch([{ body: 1 }, { body: 2 }, { body: 3 }], { partitionId: '1' });
    const after = await p.getPartitionProperties('1');
    assert.equal(after.isEmpty, false);
    assert.equal(after.lastEnqueuedSequenceNumber, 2);
    assert.equal(after.beginningSequenceNumber, 0);
    assert.ok(after.lastEnqueuedOnUtc instanceof Date);
    assert.ok(after.lastEnqueuedOffset !== undefined);
  });
  it('sendBatch to a partition id puts every event in that partition only', async () => {
    const hub = await newHub();
    const p = producer(hub);
    await p.sendBatch([{ body: 'a' }, { body: 'b' }], { partitionId: '2' });
    assert.equal((await p.getPartitionProperties('2')).lastEnqueuedSequenceNumber, 1);
    for (const other of ['0', '1', '3']) assert.equal((await p.getPartitionProperties(other)).isEmpty, true);
  });
  it('createBatch with a partitionKey keeps the whole batch in one partition and the same key lands in the same partition again', async () => {
    const hub = await newHub();
    const p = producer(hub);
    for (let round = 0; round < 2; round++) {
      const b = await p.createBatch({ partitionKey: 'customer-42' });
      for (let i = 0; i < 5; i++) assert.ok(b.tryAdd({ body: `e${round}-${i}` }));
      await p.sendBatch(b);
    }
    const counts = [];
    for (const id of ['0', '1', '2', '3']) {
      const pp = await p.getPartitionProperties(id);
      counts.push(pp.isEmpty ? 0 : pp.lastEnqueuedSequenceNumber + 1);
    }
    assert.deepEqual(counts.sort((a, b) => b - a), [10, 0, 0, 0]);
  });
  it('different partition keys spread over the partitions and every event carries its partitionKey', async () => {
    const hub = await newHub();
    const p = producer(hub);
    for (let i = 0; i < 40; i++) await p.sendBatch([{ body: `k${i}` }], { partitionKey: `key-${i}` });
    const c = consumer(hub);
    const { events } = await read(c, undefined, 40, { startPosition: earliestEventPosition }, 10000);
    assert.equal(events.length, 40);
    assert.ok(events.every(({ e }) => e.partitionKey?.startsWith('key-')));
    assert.ok(new Set(events.map((x) => x.partitionId)).size >= 2);
    const byKey = new Map();
    for (const { e, partitionId } of events) byKey.set(e.partitionKey, partitionId);
    assert.equal(byKey.size, 40);
  });
  it('sendBatch without a partition spreads events round robin so every partition gets some', async () => {
    const hub = await newHub();
    const p = producer(hub);
    for (let i = 0; i < 8; i++) await p.sendBatch([{ body: i }]);
    let total = 0;
    for (const id of ['0', '1', '2', '3']) {
      const pp = await p.getPartitionProperties(id);
      assert.equal(pp.isEmpty, false, `partition ${id}`);
      total += pp.lastEnqueuedSequenceNumber + 1;
    }
    assert.equal(total, 8);
  });
  it('tryAdd returns false once the batch is full (maxSizeInBytes) and the full batch is accepted by sendBatch', async () => {
    const hub = await newHub();
    const p = producer(hub);
    const b = await p.createBatch({ partitionId: '0', maxSizeInBytes: 5000 });
    let added = 0;
    while (b.tryAdd({ body: 'x'.repeat(400) })) added++;
    assert.ok(added >= 5 && added <= 13, `added ${added}`);
    assert.equal(b.count, added);
    assert.ok(b.sizeInBytes <= 5000);
    await p.sendBatch(b);
    assert.equal((await p.getPartitionProperties('0')).lastEnqueuedSequenceNumber, added - 1);
  });
  it('a single event over the 1 MiB limit makes createBatch.tryAdd false and sendBatch of an array fails', async () => {
    const hub = await newHub();
    const p = producer(hub);
    const b = await p.createBatch({ partitionId: '0' });
    assert.equal(b.tryAdd({ body: 'x'.repeat(1_200_000) }), false);
    const e = await rejection(() => p.sendBatch([{ body: 'x'.repeat(1_200_000) }], { partitionId: '0' }));
    assert.ok(e.code === 'MessageTooLargeError' || /size|large/i.test(e.message), `${e.code} ${e.message}`);
  });
  it('sending to a partition id that does not exist fails instead of waiting', async () => {
    const p = track(new EventHubProducerClient(conn(await newHub()), { retryOptions: { maxRetries: 0 } }));
    const e = await rejection(() => p.sendBatch([{ body: 'x' }], { partitionId: '99', abortSignal: AbortSignal.timeout(5000) }));
    assert.notEqual(e.name, 'AbortError');
  });
});

describe('event content', () => {
  it('body types (string, object, array, Buffer, number) and all properties round trip with the system properties set', async () => {
    const hub = await newHub();
    const p = producer(hub);
    const when = new Date('2024-05-06T07:08:09.000Z');
    const bodies = ['text', { a: 1, b: [1, 2] }, [1, 'x'], Buffer.from([0, 255, 7]), 42];
    await p.sendBatch(bodies.map((body, i) => ({ body, properties: { idx: i, s: 'str', f: 1.5, b: true, d: when }, contentType: 'application/x-test', correlationId: `corr-${i}`, messageId: `mid-${i}` })), { partitionId: '0' });
    const { events } = await read(consumer(hub), '0', 6, { startPosition: earliestEventPosition });
    assert.equal(events.length, 5);
    events.forEach(({ e }, i) => {
      if (Buffer.isBuffer(bodies[i])) assert.ok(Buffer.from(e.body).equals(bodies[i]));
      else assert.deepEqual(e.body, bodies[i]);
      assert.equal(e.properties.idx, i);
      assert.equal(e.properties.s, 'str');
      assert.equal(e.properties.f, 1.5);
      assert.equal(e.properties.b, true);
      assert.equal(new Date(e.properties.d).toISOString(), when.toISOString());
      assert.equal(e.contentType, 'application/x-test');
      assert.equal(e.correlationId, `corr-${i}`);
      assert.equal(e.messageId, `mid-${i}`);
      assert.equal(e.sequenceNumber, i);
      assert.ok(e.offset !== undefined);
      assert.ok(e.enqueuedTimeUtc instanceof Date);
    });
  });
  it('an event with a null body is accepted (the JS SDK sends it as a data section holding AMQP null)', async () => {
    const hub = await newHub();
    const p = producer(hub);
    await p.sendBatch([{ body: null }], { partitionId: '0' });
    const { events } = await read(consumer(hub), '0', 1, { startPosition: earliestEventPosition });
    assert.equal(events[0].e.body, null);
  });
  it('sequence numbers are consecutive and offsets increase inside a partition', async () => {
    const hub = await newHub();
    const p = producer(hub);
    for (let i = 0; i < 10; i++) await p.sendBatch([{ body: i }], { partitionId: '3' });
    const { events } = await read(consumer(hub), '3', 10, { startPosition: earliestEventPosition });
    assert.deepEqual(events.map((x) => x.e.sequenceNumber), [...Array(10).keys()]);
    assert.deepEqual(events.map((x) => x.e.body), [...Array(10).keys()]);
    const offsets = events.map((x) => Number(x.e.offset));
    assert.deepEqual([...offsets].sort((a, b) => a - b), offsets);
  });
  it('a 500 KB event is delivered intact', async () => {
    const hub = await newHub();
    const p = producer(hub);
    const body = createHash('sha256').update('seed').digest('hex').repeat(8000);
    await p.sendBatch([{ body }], { partitionId: '0' });
    const { events } = await read(consumer(hub), '0', 1, { startPosition: earliestEventPosition });
    assert.equal(events[0].e.body, body);
  });
});

describe('consumer start positions', () => {
  it('earliestEventPosition reads everything from the start of the partition', async () => {
    const hub = await newHub();
    const p = producer(hub);
    await p.sendBatch([{ body: 1 }, { body: 2 }, { body: 3 }], { partitionId: '0' });
    const { events } = await read(consumer(hub), '0', 3, { startPosition: earliestEventPosition });
    assert.deepEqual(events.map((x) => x.e.body), [1, 2, 3]);
  });
  it('latestEventPosition skips what is there and delivers events sent after the subscription', async () => {
    const hub = await newHub();
    const p = producer(hub);
    await p.sendBatch([{ body: 'old' }], { partitionId: '0' });
    const c = consumer(hub);
    const events = [];
    const sub = c.subscribe('0', { processEvents: async (evs) => { events.push(...evs.map((e) => e.body)); }, processError: async () => {} }, { startPosition: latestEventPosition });
    await sleep(800);
    await p.sendBatch([{ body: 'new1' }, { body: 'new2' }], { partitionId: '0' });
    for (let i = 0; i < 50 && events.length < 2; i++) await sleep(100);
    await sub.close();
    assert.deepEqual(events, ['new1', 'new2']);
  });
  it('a sequenceNumber position (exclusive by default) starts after that event; isInclusive starts at it', async () => {
    const hub = await newHub();
    const p = producer(hub);
    await p.sendBatch([0, 1, 2, 3, 4].map((body) => ({ body })), { partitionId: '0' });
    const a = await read(consumer(hub), '0', 2, { startPosition: { sequenceNumber: 2 } });
    assert.deepEqual(a.events.map((x) => x.e.body), [3, 4]);
    const b = await read(consumer(hub), '0', 3, { startPosition: { sequenceNumber: 2, isInclusive: true } });
    assert.deepEqual(b.events.map((x) => x.e.body), [2, 3, 4]);
  });
  it('an offset position works like the sequence number one', async () => {
    const hub = await newHub();
    const p = producer(hub);
    await p.sendBatch([0, 1, 2].map((body) => ({ body })), { partitionId: '0' });
    const first = await read(consumer(hub), '0', 3, { startPosition: earliestEventPosition });
    const off = first.events[0].e.offset;
    const r = await read(consumer(hub), '0', 2, { startPosition: { offset: off } });
    assert.deepEqual(r.events.map((x) => x.e.body), [1, 2]);
  });
  it('an enqueuedOn position returns the events enqueued at or after that time', async () => {
    const hub = await newHub();
    const p = producer(hub);
    await p.sendBatch([{ body: 'before' }], { partitionId: '0' });
    await sleep(1500);
    const mark = new Date();
    await sleep(300);
    await p.sendBatch([{ body: 'after1' }, { body: 'after2' }], { partitionId: '0' });
    const r = await read(consumer(hub), '0', 2, { startPosition: { enqueuedOn: mark } });
    assert.deepEqual(r.events.map((x) => x.e.body), ['after1', 'after2']);
  });
  it('subscribing to all partitions (no partition id) delivers from each of them with the partition id in the context', async () => {
    const hub = await newHub();
    const p = producer(hub);
    for (const id of ['0', '1', '2', '3']) await p.sendBatch([{ body: `p${id}` }], { partitionId: id });
    const { events } = await read(consumer(hub), undefined, 4, { startPosition: earliestEventPosition });
    assert.deepEqual(events.map((x) => `${x.partitionId}:${x.e.body}`).sort(), ['0:p0', '1:p1', '2:p2', '3:p3']);
  });
  it('maxBatchSize and maxWaitTimeInSeconds control how many events a processEvents call gets, and an empty call comes when nothing arrives', async () => {
    const hub = await newHub();
    const p = producer(hub);
    await p.sendBatch([...Array(25).keys()].map((body) => ({ body })), { partitionId: '0' });
    const c = consumer(hub);
    const sizes = [];
    const sub = c.subscribe('0', { processEvents: async (evs) => { sizes.push(evs.length); }, processError: async () => {} }, { startPosition: earliestEventPosition, maxBatchSize: 10, maxWaitTimeInSeconds: 1 });
    for (let i = 0; i < 80 && !sizes.includes(0); i++) await sleep(100);
    await sub.close();
    assert.ok(sizes.every((n) => n <= 10));
    assert.equal(sizes.reduce((a, b) => a + b, 0), 25);
    assert.ok(sizes.includes(0), 'an empty batch after maxWaitTimeInSeconds');
  });
  it('trackLastEnqueuedEventProperties gives the partition tail in lastEnqueuedEventProperties', async () => {
    const hub = await newHub();
    const p = producer(hub);
    await p.sendBatch([{ body: 1 }, { body: 2 }], { partitionId: '0' });
    const c = consumer(hub);
    let props;
    const sub = c.subscribe('0', { processEvents: async (evs, ctx) => { if (evs.length) props = ctx.lastEnqueuedEventProperties; }, processError: async () => {} }, { startPosition: earliestEventPosition, trackLastEnqueuedEventProperties: true });
    for (let i = 0; i < 50 && !props; i++) await sleep(100);
    await sub.close();
    assert.equal(props.sequenceNumber, 1);
    assert.ok(props.enqueuedOn instanceof Date);
    assert.ok(props.retrievedOn instanceof Date);
  });
  it('the consumer client management calls getPartitionProperties and getEventHubProperties work', async () => {
    const hub = await newHub();
    const p = producer(hub);
    await p.sendBatch([{ body: 1 }], { partitionId: '0' });
    const c = consumer(hub);
    assert.equal((await c.getPartitionProperties('0')).lastEnqueuedSequenceNumber, 0);
    assert.equal((await c.getEventHubProperties()).partitionIds.length, 4);
  });
});

describe('consumer groups and owner level', () => {
  it('consumer groups are independent: each group reads the full stream from the start', async () => {
    const hub = await newHub('h', nsName, { consumerGroups: ['group-one', 'group-two'] });
    const p = producer(hub);
    await p.sendBatch([{ body: 'a' }, { body: 'b' }], { partitionId: '0' });
    const g1 = await read(consumer(hub, 'group-one'), '0', 2, { startPosition: earliestEventPosition });
    const g2 = await read(consumer(hub, 'group-two'), '0', 2, { startPosition: earliestEventPosition });
    assert.deepEqual(g1.events.map((x) => x.e.body), ['a', 'b']);
    assert.deepEqual(g2.events.map((x) => x.e.body), ['a', 'b']);
  });
  it('a receiver with a higher ownerLevel takes over the partition and the older one gets an error', async () => {
    const hub = await newHub();
    const p = producer(hub);
    await p.sendBatch([{ body: 'x' }], { partitionId: '0' });
    const lowErrors = [];
    const low = consumer(hub);
    const subLow = low.subscribe('0', { processEvents: async () => {}, processError: async (e) => { lowErrors.push(e); } }, { startPosition: earliestEventPosition, ownerLevel: 1 });
    await sleep(1000);
    const high = consumer(hub);
    const { events } = await read(high, '0', 1, { startPosition: earliestEventPosition, ownerLevel: 5 });
    assert.equal(events.length, 1);
    for (let i = 0; i < 30 && lowErrors.length === 0; i++) await sleep(100);
    await subLow.close();
    assert.ok(lowErrors.length >= 1, 'the receiver with the lower owner level was disconnected');
    assert.ok(lowErrors.some((e) => e.code === 'ReceiverDisconnectedError' || /epoch|disconnect|higher/i.test(e.message)), lowErrors.map((e) => `${e.code}:${e.message}`).join(' | '));
  });
  it('a lower ownerLevel receiver cannot start while a higher one holds the partition', async () => {
    const hub = await newHub();
    const p = producer(hub);
    await p.sendBatch([{ body: 'x' }], { partitionId: '0' });
    const high = consumer(hub);
    const hs = high.subscribe('0', { processEvents: async () => {}, processError: async () => {} }, { startPosition: earliestEventPosition, ownerLevel: 9 });
    await sleep(800);
    const { errors } = await read(consumer(hub), '0', 1, { startPosition: earliestEventPosition, ownerLevel: 2 }, 3000);
    await hs.close();
    assert.ok(errors.length >= 1);
  });
});

describe('blob checkpoint store', () => {
  const containerClient = () => track({ c: null }) && new ContainerClient(`${emu.url('blob', DEV_ACCOUNT)}/${uniq('cp')}`, new StorageSharedKeyCredential(DEV_ACCOUNT, DEV_KEY));

  it('updateCheckpoint stores the position and a new subscriber resumes after it', async () => {
    const hub = await newHub();
    const cc = containerClient();
    await cc.create();
    const store = new BlobCheckpointStore(cc);
    const p = producer(hub);
    await p.sendBatch([0, 1, 2, 3, 4, 5].map((body) => ({ body })), { partitionId: '0' });
    const c1 = consumer(hub, '$Default', undefined);
    const seen1 = [];
    const subs = new EventHubConsumerClient('$Default', conn(hub), store, { loadBalancingOptions: { updateIntervalInMs: 500, partitionOwnershipExpirationIntervalInMs: 3000 } });
    track(subs);
    const sub = subs.subscribe({
      processEvents: async (evs, ctx) => {
        if (ctx.partitionId !== '0') return;
        for (const e of evs) { seen1.push(e.body); if (e.body === 2) await ctx.updateCheckpoint(e); }
      },
      processError: async () => {},
    }, { startPosition: earliestEventPosition });
    for (let i = 0; i < 80 && !seen1.includes(5); i++) await sleep(100);
    await sub.close();
    await subs.close();
    void c1;
    assert.deepEqual(seen1, [0, 1, 2, 3, 4, 5]);
    const cps = await store.listCheckpoints(`${nsName}.eventhub.localhost`, hub, '$Default');
    const cp0 = cps.find((c) => c.partitionId === '0');
    assert.equal(cp0.sequenceNumber, 2);
    // a new client with the same store starts after the checkpoint
    await sleep(3200);
    const subs2 = track(new EventHubConsumerClient('$Default', conn(hub), store, { loadBalancingOptions: { updateIntervalInMs: 500, partitionOwnershipExpirationIntervalInMs: 3000 } }));
    const seen2 = [];
    const sub2 = subs2.subscribe({ processEvents: async (evs, ctx) => { if (ctx.partitionId === '0') seen2.push(...evs.map((e) => e.body)); }, processError: async () => {} }, { startPosition: latestEventPosition });
    for (let i = 0; i < 80 && seen2.length < 3; i++) await sleep(100);
    await sub2.close();
    assert.deepEqual(seen2, [3, 4, 5]);
  });

  it('two consumers with one checkpoint store share the 4 partitions (balanced load balancing)', { timeout: 90000 }, async () => {
    const hub = await newHub();
    const cc = containerClient();
    await cc.create();
    const store = new BlobCheckpointStore(cc);
    const p = producer(hub);
    const lb = { updateIntervalInMs: 700, partitionOwnershipExpirationIntervalInMs: 4000, strategy: 'balanced' };
    const mk = () => {
      const client = track(new EventHubConsumerClient('$Default', conn(hub), store, { loadBalancingOptions: lb }));
      const owned = new Set();
      const closed = [];
      const sub = client.subscribe({
        processInitialize: async (ctx) => { owned.add(ctx.partitionId); },
        processClose: async (reason, ctx) => { owned.delete(ctx.partitionId); closed.push(`${ctx.partitionId}:${reason}`); },
        processEvents: async () => {},
        processError: async () => {},
      }, { startPosition: earliestEventPosition });
      return { client, owned, closed, sub };
    };
    const a = mk();
    for (let i = 0; i < 100 && a.owned.size < 4; i++) await sleep(100);
    assert.equal(a.owned.size, 4, 'the first consumer takes all partitions while it is alone');
    const b = mk();
    let balanced = false;
    for (let i = 0; i < 300 && !balanced; i++) {
      await sleep(100);
      balanced = a.owned.size === 2 && b.owned.size === 2;
    }
    assert.ok(balanced, `a owns ${[...a.owned]} b owns ${[...b.owned]}`);
    assert.equal(new Set([...a.owned, ...b.owned]).size, 4);
    await p.sendBatch([{ body: 'still delivered' }], { partitionId: '0' });
    await a.sub.close();
    // b takes over a's partitions after the ownership expires
    let all = false;
    for (let i = 0; i < 300 && !all; i++) {
      await sleep(100);
      all = b.owned.size === 4;
    }
    await b.sub.close();
    assert.ok(all, `b owns ${[...b.owned]} after a stopped`);
  });

  it('the checkpoint store lists the ownership blobs with partition id, owner id and an etag', async () => {
    const hub = await newHub();
    const cc = containerClient();
    await cc.create();
    const store = new BlobCheckpointStore(cc);
    const client = track(new EventHubConsumerClient('$Default', conn(hub), store, { loadBalancingOptions: { updateIntervalInMs: 500, strategy: 'greedy' } }));
    const sub = client.subscribe({ processEvents: async () => {}, processError: async () => {} });
    let own = [];
    for (let i = 0; i < 80 && own.length < 4; i++) {
      await sleep(100);
      own = await store.listOwnership(`${nsName}.eventhub.localhost`, hub, '$Default');
    }
    await sub.close();
    assert.equal(own.length, 4);
    assert.deepEqual(own.map((o) => o.partitionId).sort(), ['0', '1', '2', '3']);
    assert.ok(own.every((o) => o.ownerId && o.etag && o.lastModifiedTimeInMs));
  });
});

describe('Event Hubs auth variants', () => {
  it('a producer built from the namespace host plus hub name (credential object) works through a custom endpoint', async () => {
    const hub = await newHub();
    const p = track(new EventHubProducerClient(conn(hub)));
    await p.sendBatch([{ body: 'cs' }], { partitionId: '0' });
    assert.equal((await p.getPartitionProperties('0')).lastEnqueuedSequenceNumber, 0);
  });
  it('the default namespace works through sb://localhost:port', async () => {
    const hub = await newHub('h', 'default');
    const p = track(new EventHubProducerClient(`Endpoint=sb://localhost:${emu.port};SharedAccessKeyName=k;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true;EntityPath=${hub}`));
    await p.sendBatch([{ body: 'default ns' }], { partitionId: '0' });
    assert.equal((await p.getPartitionProperties('0')).isEmpty, false);
  });
});
