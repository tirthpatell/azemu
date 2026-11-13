import { describe, it, after } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { EventGridPublisherClient, AzureKeyCredential, AzureSASCredential, generateSharedAccessSignature } from '@azure/eventgrid';
import { env, uniq, sleep, rejection } from '../lib/emu.mjs';

const require = createRequire(import.meta.url);
const ns = require('@azure/eventgrid-namespaces');
const emu = env();
const key = new AzureKeyCredential('any-key');
const topicEndpoint = (topic, scheme = 'https') => `${emu.url('eventgrid', topic, scheme)}/api/events`;
const nsEndpoint = (name, scheme = 'https') => emu.url('eventgrid', name, scheme);
const adm = (path, method = 'GET', body) => fetch(`${emu.gateway}/_azemu/eventgrid${path}`, { method, headers: { 'content-type': 'application/json' }, body: body ? JSON.stringify(body) : undefined });

const servers = [];
/** A webhook that records what it gets; `status` can be a number or a function of the request count. */
async function webhook(status = 200) {
  const got = [];
  const srv = createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const text = Buffer.concat(chunks).toString();
      const entry = { method: req.method, headers: req.headers, text, json: (() => { try { return JSON.parse(text); } catch { return undefined; } })() };
      got.push(entry);
      if (req.headers['aeg-event-type'] === 'SubscriptionValidation') {
        res.setHeader('content-type', 'application/json');
        return res.end(JSON.stringify({ validationResponse: entry.json[0].data.validationCode }));
      }
      if (req.method === 'OPTIONS') { res.setHeader('WebHook-Allowed-Origin', '*'); return res.end(); }
      res.statusCode = typeof status === 'function' ? status(got.filter((g) => g.method === 'POST').length) : status;
      res.end();
    });
  });
  await new Promise((r) => srv.listen(0, '127.0.0.1', r));
  servers.push(srv);
  return { url: `http://localhost:${srv.address().port}/hook`, got, notifications: () => got.filter((g) => g.headers['aeg-event-type'] === 'Notification' || g.method === 'POST' && g.headers['aeg-event-type'] !== 'SubscriptionValidation') };
}
after(() => servers.forEach((s) => s.close()));
async function waitFor(fn, ms = 8000) {
  const end = Date.now() + ms;
  while (Date.now() < end) { const v = fn(); if (v) return v; await sleep(50); }
  return fn();
}
async function topicWithHook(opts = {}, hookStatus = 200) {
  const topic = uniq('t');
  const hook = await webhook(hookStatus);
  assert.equal((await adm(`/topics/${topic}`, 'PUT', { kind: 'custom', inputSchema: opts.inputSchema ?? 'Auto' })).status, 201);
  const r = await adm(`/topics/${topic}/subscriptions/s1`, 'PUT', { webhookUrl: hook.url, skipValidation: true, ...opts.sub });
  assert.equal(r.status, 201, await r.text());
  return { topic, hook };
}

describe('publish to a custom topic (Event Grid schema)', () => {
  it('send() of Event Grid events is delivered to a webhook subscription as a JSON array with the same fields and aeg headers', async () => {
    const { topic, hook } = await topicWithHook();
    const client = new EventGridPublisherClient(topicEndpoint(topic), 'EventGrid', key);
    await client.send([{ eventType: 'Order.Created', subject: '/orders/1', dataVersion: '1.0', data: { id: 1, tags: ['a'] }, id: 'evt-1', eventTime: new Date('2024-01-02T03:04:05.000Z') }]);
    const n = await waitFor(() => hook.notifications()[0]);
    assert.ok(n, 'webhook was not called');
    assert.equal(n.headers['aeg-event-type'], 'Notification');
    assert.match(n.headers['content-type'], /application\/json/);
    assert.equal(n.json.length, 1);
    const e = n.json[0];
    assert.equal(e.id, 'evt-1');
    assert.equal(e.eventType, 'Order.Created');
    assert.equal(e.subject, '/orders/1');
    assert.equal(e.dataVersion, '1.0');
    assert.deepEqual(e.data, { id: 1, tags: ['a'] });
    assert.equal(new Date(e.eventTime).toISOString(), '2024-01-02T03:04:05.000Z');
    assert.equal(e.metadataVersion, '1');
    assert.ok(e.topic);
  });
  it('the SDK fills id and eventTime when they are not given', async () => {
    const { topic, hook } = await topicWithHook();
    await new EventGridPublisherClient(topicEndpoint(topic), 'EventGrid', key).send([{ eventType: 't', subject: 's', dataVersion: '1', data: {} }]);
    const e = (await waitFor(() => hook.notifications()[0])).json[0];
    assert.ok(e.id && !Number.isNaN(Date.parse(e.eventTime)));
  });
  it('a batch of 100 events is delivered as 100 notifications', async () => {
    const { topic, hook } = await topicWithHook();
    const client = new EventGridPublisherClient(topicEndpoint(topic), 'EventGrid', key);
    await client.send(Array.from({ length: 100 }, (_, i) => ({ eventType: 't', subject: `/s/${i}`, dataVersion: '1', data: { i } })));
    await waitFor(() => hook.notifications().length >= 100, 15000);
    const subjects = hook.notifications().flatMap((n) => n.json.map((e) => e.subject)).sort();
    assert.equal(subjects.length, 100);
    assert.equal(new Set(subjects).size, 100);
  });
  it('a SAS credential generated by generateSharedAccessSignature is accepted', async () => {
    const { topic, hook } = await topicWithHook();
    const endpoint = topicEndpoint(topic);
    const sas = generateSharedAccessSignature(endpoint, key, new Date(Date.now() + 3600_000));
    await new EventGridPublisherClient(endpoint, 'EventGrid', new AzureSASCredential(sas)).send([{ eventType: 't', subject: 's', dataVersion: '1', data: {} }]);
    assert.ok(await waitFor(() => hook.notifications()[0]));
  });
  it('plain http works with allowInsecureConnection', async () => {
    const { topic, hook } = await topicWithHook();
    await new EventGridPublisherClient(topicEndpoint(topic, 'http'), 'EventGrid', key, { allowInsecureConnection: true }).send([{ eventType: 't', subject: 's', dataVersion: '1', data: { v: 1 } }]);
    assert.ok(await waitFor(() => hook.notifications()[0]));
  });
  it('a topic whose input schema is CloudEvents refuses Event Grid events with 400', async () => {
    const { topic } = await topicWithHook({ inputSchema: 'CloudEventSchemaV1_0' });
    const e = await rejection(() => new EventGridPublisherClient(topicEndpoint(topic), 'EventGrid', key).send([{ eventType: 't', subject: 's', dataVersion: '1', data: {} }]));
    assert.equal(e.statusCode, 400);
  });
  it('a request without any credential header is 401', async () => {
    const { topic } = await topicWithHook();
    const r = await fetch(topicEndpoint(topic), { method: 'POST', headers: { 'content-type': 'application/json' }, body: '[]' });
    assert.equal(r.status, 401);
  });
});

describe('CloudEvents and custom schema', () => {
  it('send() of CloudEvents (structured, batch) delivers CloudEvents with the same attributes', async () => {
    const { topic, hook } = await topicWithHook({ sub: { deliverySchema: 'CloudEventSchemaV1_0' } });
    const client = new EventGridPublisherClient(topicEndpoint(topic), 'CloudEvent', key);
    await client.send([{ type: 'Order.Created', source: '/orders', id: 'ce-1', data: { n: 1 }, subject: 'orders/1', time: new Date('2024-02-03T04:05:06.000Z'), datacontenttype: 'application/json' }]);
    const n = await waitFor(() => hook.notifications()[0]);
    assert.match(n.headers['content-type'], /application\/cloudevents\+json/);
    const e = n.json;
    assert.equal(e.specversion, '1.0');
    assert.equal(e.type, 'Order.Created');
    assert.equal(e.source, '/orders');
    assert.equal(e.id, 'ce-1');
    assert.deepEqual(e.data, { n: 1 });
    assert.equal(e.subject, 'orders/1');
  });
  it('CloudEvent with binary data (data_base64)', async () => {
    const { topic, hook } = await topicWithHook({ sub: { deliverySchema: 'CloudEventSchemaV1_0' } });
    await new EventGridPublisherClient(topicEndpoint(topic), 'CloudEvent', key).send([{ type: 't', source: '/s', id: 'bin', data: new Uint8Array([1, 2, 3, 250]), datacontenttype: 'application/octet-stream' }]);
    const e = (await waitFor(() => hook.notifications()[0])).json;
    assert.equal(e.data_base64, Buffer.from([1, 2, 3, 250]).toString('base64'));
  });
  it('custom input schema events are delivered as sent', async () => {
    const { topic, hook } = await topicWithHook({ inputSchema: 'CustomEventSchema', sub: { deliverySchema: 'CustomInputSchema' } });
    await new EventGridPublisherClient(topicEndpoint(topic), 'Custom', key).send([{ myId: 7, kind: 'x', nested: { a: [1, 2] } }]);
    const n = await waitFor(() => hook.notifications()[0]);
    assert.deepEqual(n.json, [{ myId: 7, kind: 'x', nested: { a: [1, 2] } }]);
  });
  it('a CloudEvent topic with the Event Grid delivery schema converts the event', async () => {
    const { topic, hook } = await topicWithHook({ inputSchema: 'CloudEventSchemaV1_0', sub: { deliverySchema: 'EventGridSchema' } });
    await new EventGridPublisherClient(topicEndpoint(topic), 'CloudEvent', key).send([{ type: 'T.Type', source: '/src', id: 'cv-1', data: { a: 1 }, subject: 'subj' }]);
    const e = (await waitFor(() => hook.notifications()[0])).json[0];
    assert.equal(e.eventType, 'T.Type');
    assert.equal(e.id, 'cv-1');
    assert.deepEqual(e.data, { a: 1 });
  });
});

describe('push delivery behaviour', () => {
  it('a webhook that answers 500 once is retried and then gets the event again with aeg-delivery-count 1', async () => {
    const { topic, hook } = await topicWithHook({ sub: { retryPolicy: { retryIntervalsMs: [200] } } }, (n) => (n === 1 ? 500 : 200));
    await new EventGridPublisherClient(topicEndpoint(topic), 'EventGrid', key).send([{ eventType: 't', subject: 's', dataVersion: '1', data: {} }]);
    await waitFor(() => hook.notifications().length >= 2);
    const [a, b] = hook.notifications();
    assert.equal(a.headers['aeg-delivery-count'], '0');
    assert.equal(b.headers['aeg-delivery-count'], '1');
    assert.equal(a.json[0].id, b.json[0].id);
  });
  it('subject and event type filters decide what reaches the webhook', async () => {
    const { topic, hook } = await topicWithHook({ sub: { filter: { includedEventTypes: ['Keep'], subjectBeginsWith: '/in/' } } });
    const c = new EventGridPublisherClient(topicEndpoint(topic), 'EventGrid', key);
    await c.send([
      { eventType: 'Keep', subject: '/in/1', dataVersion: '1', data: {}, id: 'k1' },
      { eventType: 'Drop', subject: '/in/2', dataVersion: '1', data: {}, id: 'd1' },
      { eventType: 'Keep', subject: '/out/3', dataVersion: '1', data: {}, id: 'd2' },
    ]);
    await waitFor(() => hook.notifications().length >= 1);
    await sleep(500);
    assert.deepEqual(hook.notifications().flatMap((n) => n.json.map((e) => e.id)), ['k1']);
  });
  it('the validation handshake for a new webhook subscription succeeds when the webhook echoes the code', async () => {
    const topic = uniq('t');
    const hook = await webhook(200);
    await adm(`/topics/${topic}`, 'PUT', { kind: 'custom' });
    const r = await adm(`/topics/${topic}/subscriptions/v`, 'PUT', { webhookUrl: hook.url });
    assert.equal(r.status, 201);
    const sub = await (await adm(`/topics/${topic}/subscriptions/v`)).json();
    assert.equal(sub.validation.status, 'Succeeded');
    assert.equal(hook.got[0].headers['aeg-event-type'], 'SubscriptionValidation');
    assert.equal(hook.got[0].json[0].eventType, 'Microsoft.EventGrid.SubscriptionValidationEvent');
  });
});

describe('namespace topics (pull delivery) with @azure/eventgrid-namespaces', () => {
  async function nsTopic() {
    const name = uniq('egns');
    const topic = uniq('top');
    assert.ok([200, 201].includes((await adm(`/topics/${topic}?namespace=${name}`, 'PUT', { kind: 'namespace' })).status));
    const r = await adm(`/topics/${topic}/subscriptions/sub1?namespace=${name}`, 'PUT', { deliveryMode: 'pull', lockDurationSeconds: 60, maxDeliveryCount: 3 });
    assert.ok([200, 201].includes(r.status), await r.text());
    const endpoint = nsEndpoint(name);
    return {
      name, topic,
      sender: new ns.EventGridSenderClient(endpoint, key, topic),
      receiver: new ns.EventGridReceiverClient(endpoint, key, topic, 'sub1'),
    };
  }
  const ev = (id, data = { n: 1 }) => ({ id, source: '/test', type: 'Test.Type', data, specversion: '1.0', time: new Date() });

  it('sendEvents then receiveEvents returns the event with a lock token and delivery count 1; acknowledge removes it', async () => {
    const { sender, receiver } = await nsTopic();
    await sender.sendEvents([ev('e1', { hello: 'world' })]);
    const r = await receiver.receiveEvents({ maxEvents: 5, maxWaitTime: 5 });
    assert.equal(r.details.length, 1);
    const d = r.details[0];
    assert.equal(d.event.id, 'e1');
    assert.equal(d.event.type, 'Test.Type');
    assert.deepEqual(d.event.data, { hello: 'world' });
    assert.ok(d.brokerProperties.lockToken);
    assert.equal(d.brokerProperties.deliveryCount, 1);
    const ack = await receiver.acknowledgeEvents([d.brokerProperties.lockToken]);
    assert.deepEqual(ack.succeededLockTokens, [d.brokerProperties.lockToken]);
    assert.deepEqual(ack.failedLockTokens, []);
    assert.equal((await receiver.receiveEvents({ maxEvents: 1, maxWaitTime: 1 })).details.length, 0);
  });
  it('release returns the event with deliveryCount 2; reject dead-letters it', async () => {
    const { sender, receiver, topic, name } = await nsTopic();
    await sender.sendEvents(ev('r1'));
    const [d1] = (await receiver.receiveEvents({ maxWaitTime: 5 })).details;
    const rel = await receiver.releaseEvents([d1.brokerProperties.lockToken]);
    assert.equal(rel.succeededLockTokens.length, 1);
    const [d2] = (await receiver.receiveEvents({ maxWaitTime: 5 })).details;
    assert.equal(d2.event.id, 'r1');
    assert.equal(d2.brokerProperties.deliveryCount, 2);
    const rej = await receiver.rejectEvents([d2.brokerProperties.lockToken]);
    assert.equal(rej.succeededLockTokens.length, 1);
    assert.equal((await receiver.receiveEvents({ maxWaitTime: 1 })).details.length, 0);
    const dead = await (await adm(`/topics/${topic}/subscriptions/sub1/deadletters?namespace=${name}`)).json();
    assert.ok(JSON.stringify(dead).includes('r1'));
  });
  it('renewEventLocks answers for a held lock and a made-up token is a failed token (BadToken)', async () => {
    const { sender, receiver } = await nsTopic();
    await sender.sendEvents(ev('l1'));
    const [d] = (await receiver.receiveEvents({ maxWaitTime: 5 })).details;
    const renew = await receiver.renewEventLocks([d.brokerProperties.lockToken]);
    assert.equal(renew.succeededLockTokens.length, 1);
    const bad = await receiver.acknowledgeEvents(['bm90LWEtcmVhbC10b2tlbg==']);
    assert.equal(bad.failedLockTokens.length, 1);
    assert.equal(bad.failedLockTokens[0].error.code, 'BadToken');
  });
  it('maxEvents limits a receive and events come out in FIFO order', async () => {
    const { sender, receiver } = await nsTopic();
    await sender.sendEvents([1, 2, 3, 4, 5].map((i) => ev(`f${i}`)));
    const first = await receiver.receiveEvents({ maxEvents: 3, maxWaitTime: 5 });
    assert.deepEqual(first.details.map((d) => d.event.id), ['f1', 'f2', 'f3']);
    const rest = await receiver.receiveEvents({ maxEvents: 10, maxWaitTime: 5 });
    assert.deepEqual(rest.details.map((d) => d.event.id), ['f4', 'f5']);
  });
  it('after maxDeliveryCount releases the event is dead-lettered and not offered again', async () => {
    const { sender, receiver } = await nsTopic();
    await sender.sendEvents(ev('m1'));
    for (let i = 0; i < 3; i++) {
      const [d] = (await receiver.receiveEvents({ maxWaitTime: 5 })).details;
      assert.equal(d.event.id, 'm1');
      await receiver.releaseEvents([d.brokerProperties.lockToken]);
    }
    assert.equal((await receiver.receiveEvents({ maxWaitTime: 1 })).details.length, 0);
  });
  it('a bearer token credential over https works for sending and receiving', async () => {
    const { topic, name } = await nsTopic();
    const cred = { getToken: async () => ({ token: 'any', expiresOnTimestamp: Date.now() + 3600_000 }) };
    const sender = new ns.EventGridSenderClient(nsEndpoint(name), cred, topic);
    const receiver = new ns.EventGridReceiverClient(nsEndpoint(name), cred, topic, 'sub1');
    await sender.sendEvents(ev('t1'));
    assert.equal((await receiver.receiveEvents({ maxWaitTime: 5 })).details[0].event.id, 't1');
  });
});
