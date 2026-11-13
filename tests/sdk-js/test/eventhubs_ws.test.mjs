// Event Hubs over AMQP in WebSocket (wss through the `ws` package) with a custom endpoint.
import { describe, it, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import WebSocket from 'ws';
import { EventHubProducerClient, EventHubConsumerClient, earliestEventPosition } from '@azure/event-hubs';
import { env, uniq, sleep, track, closeTracked } from '../lib/emu.mjs';

const emu = env();
after(() => closeTracked());
const ns = uniq('ehws');
const opts = { webSocketOptions: { webSocket: WebSocket }, customEndpointAddress: `https://${ns}.eventhub.localhost:${emu.port}` };
const cs = (hub) => `Endpoint=sb://${ns}.eventhub.localhost/;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2VjcmV0;EntityPath=${hub}`;
/** Create an event hub through the admin API before using it (the emulator does not create hubs by being named). */
async function newHub(prefix = 'h') {
  const name = uniq(prefix);
  const r = await fetch(`${emu.gateway}/_azemu/eventhubs/${ns}/hubs/${name}`, { method: 'PUT', headers: { 'content-type': 'application/json' }, body: '{}' });
  assert.equal(r.status, 201, await r.text());
  return name;
}
const upgrades = () => (readFileSync(emu.logPath, 'utf8').match(/websocket upgrade[^\n]*status=101/g) ?? []).length;

describe('Event Hubs over WebSocket', () => {
  it('producer and consumer work over wss and the emulator logs the upgrade', async () => {
    const hub = await newHub();
    const before = upgrades();
    const p = track(new EventHubProducerClient(cs(hub), opts));
    await p.sendBatch([{ body: 'a' }, { body: { b: 2 }, properties: { k: 'v' } }], { partitionId: '1' });
    assert.ok(upgrades() > before);
    assert.equal((await p.getPartitionProperties('1')).lastEnqueuedSequenceNumber, 1);
    const c = track(new EventHubConsumerClient('$Default', cs(hub), opts));
    const got = [];
    const sub = c.subscribe('1', { processEvents: async (evs) => { got.push(...evs); }, processError: async () => {} }, { startPosition: earliestEventPosition });
    for (let i = 0; i < 80 && got.length < 2; i++) await sleep(100);
    await sub.close();
    assert.deepEqual(got.map((e) => e.body), ['a', { b: 2 }]);
    assert.equal(got[1].properties.k, 'v');
  });
  it('an 800 KB event crosses many WebSocket frames intact', async () => {
    const hub = await newHub();
    const p = track(new EventHubProducerClient(cs(hub), opts));
    const body = 'xyz'.repeat(260_000);
    await p.sendBatch([{ body }], { partitionId: '0' });
    const c = track(new EventHubConsumerClient('$Default', cs(hub), opts));
    const got = [];
    const sub = c.subscribe('0', { processEvents: async (evs) => { got.push(...evs); }, processError: async () => {} }, { startPosition: earliestEventPosition });
    for (let i = 0; i < 100 && got.length < 1; i++) await sleep(100);
    await sub.close();
    assert.equal(got[0].body, body);
  });
  it('a WebSocket consumer sees events sent by a plain TCP producer of the same hub', async () => {
    const hub = await newHub();
    const tcp = track(new EventHubProducerClient(`Endpoint=sb://${ns}.eventhub.localhost:${emu.port};SharedAccessKeyName=k;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true;EntityPath=${hub}`));
    await tcp.sendBatch([{ body: 'tcp' }], { partitionId: '2' });
    const c = track(new EventHubConsumerClient('$Default', cs(hub), opts));
    const got = [];
    const sub = c.subscribe('2', { processEvents: async (evs) => { got.push(...evs); }, processError: async () => {} }, { startPosition: earliestEventPosition });
    for (let i = 0; i < 80 && got.length < 1; i++) await sleep(100);
    await sub.close();
    assert.equal(got[0].body, 'tcp');
  });
  it('getEventHubProperties over WebSocket (management link)', async () => {
    const hub = await newHub();
    const p = track(new EventHubProducerClient(cs(hub), opts));
    assert.deepEqual((await p.getEventHubProperties()).partitionIds, ['0', '1', '2', '3']);
  });
});
