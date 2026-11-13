// Service Bus over AMQP in WebSocket (wss through the `ws` package), pointed at the emulator with customEndpointAddress.
import { describe, it, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import WebSocket from 'ws';
import { ServiceBusClient, ServiceBusAdministrationClient } from '@azure/service-bus';
import { env, uniq, sleep, rejection, track, closeTracked } from '../lib/emu.mjs';

const emu = env();
const ns = uniq('wsns');
const cs = `Endpoint=sb://${ns}.servicebus.localhost/;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2VjcmV0`;
const wsClient = (extra = {}) => track(new ServiceBusClient(cs, { webSocketOptions: { webSocket: WebSocket }, customEndpointAddress: `https://${ns}.servicebus.localhost:${emu.port}`, ...extra }));
const sb = wsClient();
const admin = new ServiceBusAdministrationClient(`Endpoint=sb://${ns}.servicebus.localhost:${emu.port};SharedAccessKeyName=k;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true`);
after(() => closeTracked());
/** Create a queue through the administration client before using it (the emulator does not create queues by being named). */
async function newQueue(props = {}) {
  const q = uniq('q');
  await admin.createQueue(q, props);
  return q;
}
const upgrades = () => (readFileSync(emu.logPath, 'utf8').match(/websocket upgrade[^\n]*\/\$servicebus\/websocket status=101/g) ?? []).length;

describe('Service Bus over WebSocket', () => {
  it('the client really upgrades to a WebSocket on /$servicebus/websocket with the AMQPWSB10 subprotocol (wss, CA trusted through NODE_EXTRA_CA_CERTS)', async () => {
    const before = upgrades();
    const q = await newQueue();
    await sb.createSender(q).sendMessages({ messageId: 'a', body: 'over ws' });
    assert.ok(upgrades() > before);
    assert.match(readFileSync(emu.logPath, 'utf8'), /subprotocol="AMQPWSB10" tls=true/);
  });
  it('send, receive and complete on a queue', async () => {
    const q = await newQueue();
    await sb.createSender(q).sendMessages({ messageId: 'a', body: { n: 1 }, subject: 's', applicationProperties: { k: 'v' } });
    const r = sb.createReceiver(q);
    const [m] = await r.receiveMessages(1, { maxWaitTimeInMs: 5000 });
    assert.deepEqual(m.body, { n: 1 });
    assert.equal(m.subject, 's');
    assert.equal(m.applicationProperties.k, 'v');
    await r.completeMessage(m);
    assert.equal((await admin.getQueueRuntimeProperties(q)).totalMessageCount, 0);
  });
  it('a 200 KB message crosses many WebSocket frames intact', async () => {
    const q = await newQueue();
    const body = 'ab'.repeat(100_000);
    await sb.createSender(q).sendMessages({ messageId: 'big', body });
    const r = sb.createReceiver(q, { receiveMode: 'receiveAndDelete' });
    const [m] = await r.receiveMessages(1, { maxWaitTimeInMs: 5000 });
    assert.equal(m.body, body);
  });
  it('batch send, then receive 30 messages in order', async () => {
    const q = await newQueue();
    const s = sb.createSender(q);
    const batch = await s.createMessageBatch();
    for (let i = 0; i < 30; i++) assert.ok(batch.tryAddMessage({ messageId: `b${i}`, body: i }));
    await s.sendMessages(batch);
    const got = await sb.createReceiver(q, { receiveMode: 'receiveAndDelete' }).receiveMessages(40, { maxWaitTimeInMs: 4000 });
    assert.deepEqual(got.map((m) => m.body), [...Array(30).keys()]);
  });
  it('topic with two subscriptions and a SQL rule', async () => {
    const t = uniq('t');
    await admin.createTopic(t);
    await admin.createSubscription(t, 'all');
    await admin.createSubscription(t, 'only');
    await admin.deleteRule(t, 'only', '$Default');
    await admin.createRule(t, 'only', 'r', { sqlExpression: "kind = 'x'" });
    const s = sb.createSender(t);
    await s.sendMessages({ messageId: '1', body: 'x1', applicationProperties: { kind: 'x' } });
    await s.sendMessages({ messageId: '2', body: 'y1', applicationProperties: { kind: 'y' } });
    const all = await sb.createReceiver(t, 'all', { receiveMode: 'receiveAndDelete' }).receiveMessages(5, { maxWaitTimeInMs: 2000 });
    const only = await sb.createReceiver(t, 'only', { receiveMode: 'receiveAndDelete' }).receiveMessages(5, { maxWaitTimeInMs: 2000 });
    assert.equal(all.length, 2);
    assert.deepEqual(only.map((m) => m.body), ['x1']);
  });
  it('sessions over WebSocket: acceptSession, session state, complete', async () => {
    const q = uniq('q');
    await admin.createQueue(q, { requiresSession: true });
    await sb.createSender(q).sendMessages([{ messageId: '1', body: 'a', sessionId: 'S' }, { messageId: '2', body: 'b', sessionId: 'S' }]);
    const r = await sb.acceptSession(q, 'S');
    const got = await r.receiveMessages(5, { maxWaitTimeInMs: 2000 });
    assert.deepEqual(got.map((m) => m.body), ['a', 'b']);
    for (const m of got) await r.completeMessage(m);
    await r.setSessionState('state-1');
    assert.equal(await r.getSessionState(), 'state-1');
    await r.close();
  });
  it('subscribe() handler over WebSocket, dead-letter and scheduled messages', async () => {
    const q = await newQueue();
    const s = sb.createSender(q);
    await s.scheduleMessages({ messageId: 'sch', body: 'scheduled' }, new Date(Date.now() + 1500));
    await s.sendMessages({ messageId: 'now', body: 'now' });
    const r = sb.createReceiver(q);
    const seen = [];
    const sub = r.subscribe({ processMessage: async (m) => { seen.push(m.body); }, processError: async () => {} });
    for (let i = 0; i < 60 && seen.length < 2; i++) await sleep(100);
    await sub.close();
    assert.deepEqual(seen.sort(), ['now', 'scheduled']);
    const [dm] = (await s.sendMessages({ messageId: 'dl', body: 'dl' }), await r.receiveMessages(1, { maxWaitTimeInMs: 3000 }));
    await r.deadLetterMessage(dm, { deadLetterReason: 'ws-reason' });
    const [d] = await sb.createReceiver(q, { subQueueType: 'deadLetter' }).receiveMessages(1, { maxWaitTimeInMs: 3000 });
    assert.equal(d.deadLetterReason, 'ws-reason');
  });
  it('a WebSocket client and a TCP client share the same queue', async () => {
    const q = await newQueue();
    const tcp = track(new ServiceBusClient(`Endpoint=sb://${ns}.servicebus.localhost:${emu.port};SharedAccessKeyName=k;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true`));
    await tcp.createSender(q).sendMessages({ messageId: 't', body: 'from tcp' });
    const [m] = await sb.createReceiver(q, { receiveMode: 'receiveAndDelete' }).receiveMessages(1, { maxWaitTimeInMs: 4000 });
    assert.equal(m.body, 'from tcp');
  });
  it('a client whose WebSocket cannot reach the port fails with a connection error, not a hang', async () => {
    const dead = new ServiceBusClient(cs, { webSocketOptions: { webSocket: WebSocket }, customEndpointAddress: `https://${ns}.servicebus.localhost:1`, retryOptions: { maxRetries: 0, timeoutInMs: 4000 } });
    track(dead);
    const e = await rejection(() => dead.createSender(uniq('q')).sendMessages({ messageId: 'x', body: 'x' }));
    assert.ok(e instanceof Error); // an AggregateError of ECONNREFUSED from the ws package
  });
});
