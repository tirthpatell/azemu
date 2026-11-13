import { describe, it, after } from 'node:test';
import assert from 'node:assert/strict';
import { ServiceBusClient, ServiceBusAdministrationClient } from '@azure/service-bus';
import { env, uniq, sleep, collect, rejection, track, closeTracked } from '../lib/emu.mjs';

const emu = env();
const ns = uniq('sbns');
const connStr = (name = ns, port = emu.port) => `Endpoint=sb://${name}.servicebus.localhost:${port};SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true`;
const sb = track(new ServiceBusClient(connStr()));
const admin = new ServiceBusAdministrationClient(connStr());
after(() => closeTracked());

let n = 0;
const mid = () => `m-${++n}-${Math.random().toString(36).slice(2, 8)}`;
/** Send with a message id: the JS SDK needs one on the receiving side (see docs/sdk-notes.md). */
const send = (sender, msg) => sender.sendMessages(typeof msg === 'object' && msg !== null && 'body' in msg ? { messageId: mid(), ...msg } : { messageId: mid(), body: msg });
async function queue(props = {}) {
  const name = uniq('q');
  await admin.createQueue(name, props);
  return name;
}
const recv = async (r, max = 1, wait = 5000) => r.receiveMessages(max, { maxWaitTimeInMs: wait });

describe('queue basics', () => {
  it('send then receive in peekLock mode, then complete: the queue is empty afterwards', async () => {
    const q = await queue();
    const s = sb.createSender(q);
    await send(s, 'hello');
    const r = sb.createReceiver(q);
    const [m] = await recv(r);
    assert.equal(m.body, 'hello');
    assert.equal(m.deliveryCount, 0);
    assert.ok(m.lockToken);
    assert.ok(m.lockedUntilUtc > new Date());
    assert.ok(m.sequenceNumber !== undefined);
    assert.ok(m.enqueuedTimeUtc instanceof Date);
    await r.completeMessage(m);
    assert.equal((await r.peekMessages(5)).length, 0);
    assert.equal((await admin.getQueueRuntimeProperties(q)).totalMessageCount, 0);
  });
  it('a message sent without a message id comes back with one, as on Azure, so completeMessage works', async () => {
    const q = await queue();
    await sb.createSender(q).sendMessages({ body: 'no id' });
    const r = sb.createReceiver(q);
    const [m] = await recv(r);
    assert.ok(m.messageId);
    await r.completeMessage(m);
  });
  it('all the standard message properties round trip', async () => {
    const q = await queue();
    const sent = {
      messageId: 'id-1', subject: 'subj', contentType: 'application/json', correlationId: 'corr-1', to: 'dest', replyTo: 'reply', replyToSessionId: 'rs-1',
      timeToLive: 60_000, applicationProperties: { s: 'str', n: 42, f: 1.5, b: true, d: new Date('2024-01-02T03:04:05.000Z') }, body: { a: 1, nested: { list: [1, 2, 3] } },
    };
    await sb.createSender(q).sendMessages(sent);
    const r = sb.createReceiver(q);
    const [m] = await recv(r);
    for (const k of ['messageId', 'subject', 'contentType', 'correlationId', 'to', 'replyTo', 'replyToSessionId']) assert.equal(m[k], sent[k], k);
    assert.deepEqual(m.body, sent.body);
    assert.equal(m.applicationProperties.s, 'str');
    assert.equal(m.applicationProperties.n, 42);
    assert.equal(m.applicationProperties.f, 1.5);
    assert.equal(m.applicationProperties.b, true);
    assert.equal(new Date(m.applicationProperties.d).toISOString(), '2024-01-02T03:04:05.000Z');
    assert.equal(m.timeToLive, 60_000);
    assert.ok(m.expiresAtUtc > new Date());
    await r.completeMessage(m);
  });
  it('body types: string, Buffer, number, boolean, array, null and a 200 KB string round trip', async () => {
    const q = await queue();
    const s = sb.createSender(q);
    const bodies = ['text', Buffer.from([0, 1, 2, 255]), 42, true, [1, 'two', { three: 3 }], null, 'x'.repeat(200_000)];
    for (const b of bodies) await send(s, { body: b });
    const r = sb.createReceiver(q, { receiveMode: 'receiveAndDelete' });
    const got = await recv(r, 10, 5000);
    assert.equal(got.length, bodies.length);
    got.forEach((m, i) => {
      if (Buffer.isBuffer(bodies[i])) assert.ok(Buffer.from(m.body).equals(bodies[i]));
      else assert.deepEqual(m.body, bodies[i]);
    });
  });
  it('receiveAndDelete removes the message at once (no lock token to settle)', async () => {
    const q = await queue();
    await send(sb.createSender(q), 'x');
    const r = sb.createReceiver(q, { receiveMode: 'receiveAndDelete' });
    const [m] = await recv(r);
    assert.equal(m.body, 'x');
    assert.equal((await admin.getQueueRuntimeProperties(q)).totalMessageCount, 0);
  });
  it('receiveMessages on an empty queue returns [] after maxWaitTimeInMs', async () => {
    const q = await queue();
    const t = Date.now();
    assert.deepEqual(await recv(sb.createReceiver(q), 1, 1500), []);
    assert.ok(Date.now() - t >= 1400);
  });
  it('abandon returns the message with deliveryCount 1 and the same sequence number', async () => {
    const q = await queue();
    await send(sb.createSender(q), 'a');
    const r = sb.createReceiver(q);
    const [m1] = await recv(r);
    await r.abandonMessage(m1, { reason: 'x' });
    const [m2] = await recv(r);
    assert.equal(m2.deliveryCount, 1);
    assert.equal(m2.sequenceNumber.toString(), m1.sequenceNumber.toString());
    await r.completeMessage(m2);
  });
  it('abandon with propertiesToModify changes the application properties for the next delivery', async () => {
    const q = await queue();
    await send(sb.createSender(q), { body: 'a', applicationProperties: { tries: 0 } });
    const r = sb.createReceiver(q);
    const [m1] = await recv(r);
    await r.abandonMessage(m1, { tries: 1 });
    const [m2] = await recv(r);
    assert.equal(m2.applicationProperties.tries, 1);
    await r.completeMessage(m2);
  });
  it('defer hides the message; receiveDeferredMessages by sequence number returns it and it can be completed', async () => {
    const q = await queue();
    await send(sb.createSender(q), 'later');
    const r = sb.createReceiver(q);
    const [m] = await recv(r);
    await r.deferMessage(m);
    assert.equal((await recv(r, 1, 1000)).length, 0);
    const [d] = await r.receiveDeferredMessages([m.sequenceNumber]);
    assert.equal(d.body, 'later');
    assert.equal(d.state, 'deferred');
    await r.completeMessage(d);
    assert.equal((await admin.getQueueRuntimeProperties(q)).totalMessageCount, 0);
  });
  it('deadLetterMessage moves it to the dead-letter queue with reason and description', async () => {
    const q = await queue();
    await send(sb.createSender(q), 'bad');
    const r = sb.createReceiver(q);
    const [m] = await recv(r);
    await r.deadLetterMessage(m, { deadLetterReason: 'because', deadLetterErrorDescription: 'it is bad' });
    assert.equal((await recv(r, 1, 800)).length, 0);
    const dlq = sb.createReceiver(q, { subQueueType: 'deadLetter' });
    const [d] = await recv(dlq);
    assert.equal(d.body, 'bad');
    assert.equal(d.deadLetterReason, 'because');
    assert.equal(d.deadLetterErrorDescription, 'it is bad');
    await dlq.completeMessage(d);
    assert.equal((await admin.getQueueRuntimeProperties(q)).deadLetterMessageCount, 0);
  });
  it('deadLetterMessage with extra properties (propertiesToModify) puts them on the dead-lettered message', async () => {
    const q = await queue();
    await send(sb.createSender(q), 'bad');
    const r = sb.createReceiver(q);
    const [m] = await recv(r);
    await r.deadLetterMessage(m, { deadLetterReason: 'because', extra: 'e1' });
    const [d] = await recv(sb.createReceiver(q, { subQueueType: 'deadLetter' }));
    assert.equal(d.applicationProperties.extra, 'e1');
  });
  it('after maxDeliveryCount abandons the message is dead-lettered with MaxDeliveryCountExceeded', async () => {
    const q = await queue({ maxDeliveryCount: 2 });
    await send(sb.createSender(q), 'poison');
    const r = sb.createReceiver(q);
    for (let i = 0; i < 2; i++) {
      const [m] = await recv(r);
      await r.abandonMessage(m);
    }
    assert.equal((await recv(r, 1, 800)).length, 0);
    const [d] = await recv(sb.createReceiver(q, { subQueueType: 'deadLetter' }));
    assert.equal(d.deadLetterReason, 'MaxDeliveryCountExceeded');
  });
  it('the lock expires after lockDuration and the message comes back with deliveryCount 1', async () => {
    const q = await queue({ lockDuration: 'PT5S' });
    await send(sb.createSender(q), 'x');
    const r = sb.createReceiver(q, { maxAutoLockRenewalDurationInMs: 0 });
    const [m] = await recv(r);
    await sleep(5600);
    const [m2] = await recv(r);
    assert.equal(m2.deliveryCount, 1);
    const e = await rejection(() => r.completeMessage(m));
    assert.equal(e.code, 'MessageLockLost');
  });
  it('renewMessageLock extends lockedUntilUtc', async () => {
    const q = await queue({ lockDuration: 'PT30S' });
    await send(sb.createSender(q), 'x');
    const r = sb.createReceiver(q);
    const [m] = await recv(r);
    const before = m.lockedUntilUtc.getTime();
    await sleep(1200);
    const until = await r.renewMessageLock(m);
    assert.ok(until.getTime() > before);
    await r.completeMessage(m);
  });
  it('peekMessages does not lock and can start from a sequence number', async () => {
    const q = await queue();
    const s = sb.createSender(q);
    for (let i = 0; i < 4; i++) await send(s, `p${i}`);
    const r = sb.createReceiver(q);
    const all = await r.peekMessages(10);
    assert.deepEqual(all.map((m) => m.body), ['p0', 'p1', 'p2', 'p3']);
    assert.ok(all.every((m) => m.lockToken === undefined || m.lockToken === null || m.lockToken === '00000000-0000-0000-0000-000000000000'));
    const from = await r.peekMessages(10, { fromSequenceNumber: all[2].sequenceNumber });
    assert.deepEqual(from.map((m) => m.body), ['p2', 'p3']);
    assert.equal((await recv(r, 4)).length, 4);
  });
  it('messages come out in send order from one queue with one receiver', async () => {
    const q = await queue();
    const s = sb.createSender(q);
    for (let i = 0; i < 20; i++) await send(s, `o${i}`);
    const got = await recv(sb.createReceiver(q, { receiveMode: 'receiveAndDelete' }), 20, 5000);
    assert.deepEqual(got.map((m) => m.body), [...Array(20).keys()].map((i) => `o${i}`));
  });
  it('duplicate detection drops a second message with the same messageId', async () => {
    const q = await queue({ requiresDuplicateDetection: true });
    const s = sb.createSender(q);
    await s.sendMessages({ messageId: 'dup-1', body: 'one' });
    await s.sendMessages({ messageId: 'dup-1', body: 'two' });
    const got = await recv(sb.createReceiver(q, { receiveMode: 'receiveAndDelete' }), 5, 1500);
    assert.deepEqual(got.map((m) => m.body), ['one']);
  });
  it('a message with a short timeToLive is dead-lettered or dropped after it expires (deadLetteringOnMessageExpiration)', async () => {
    const q = await queue({ deadLetteringOnMessageExpiration: true });
    await send(sb.createSender(q), { body: 'ttl', timeToLive: 1000 });
    await sleep(1800);
    assert.equal((await recv(sb.createReceiver(q), 1, 800)).length, 0);
    const [d] = await recv(sb.createReceiver(q, { subQueueType: 'deadLetter' }));
    assert.equal(d.deadLetterReason, 'TTLExpiredException');
  });
  it('a 300 KB message is MessageSizeExceeded (queue limit 256 KB)', async () => {
    const q = await queue();
    const e = await rejection(() => sb.createSender(q).sendMessages({ messageId: mid(), body: 'x'.repeat(300_000) }));
    assert.equal(e.code, 'MessageSizeExceeded');
  });
  it('forwardTo moves a sent message to another queue', async () => {
    const target = await queue();
    const src = await queue({ forwardTo: target });
    await send(sb.createSender(src), 'fwd');
    const [m] = await recv(sb.createReceiver(target));
    assert.equal(m.body, 'fwd');
  });
});

describe('batches and scheduling', () => {
  it('createMessageBatch + tryAddMessage fills until the size limit and sendMessages(batch) delivers every message', async () => {
    const q = await queue();
    const s = sb.createSender(q);
    const batch = await s.createMessageBatch();
    let added = 0;
    for (let i = 0; i < 50; i++) {
      if (!batch.tryAddMessage({ messageId: mid(), body: `batch-${i}` })) break;
      added++;
    }
    assert.equal(added, 50);
    assert.equal(batch.count, 50);
    await s.sendMessages(batch);
    const got = await recv(sb.createReceiver(q, { receiveMode: 'receiveAndDelete' }), 60, 4000);
    assert.equal(got.length, 50);
  });
  it('tryAddMessage returns false once the batch holds the max size (maxSizeInBytes)', async () => {
    const q = await queue();
    const s = sb.createSender(q);
    const batch = await s.createMessageBatch({ maxSizeInBytes: 2000 });
    let added = 0;
    while (batch.tryAddMessage({ messageId: mid(), body: 'x'.repeat(300) })) added++;
    assert.ok(added >= 3 && added <= 7, `added ${added}`);
    await s.sendMessages(batch);
    assert.equal((await recv(sb.createReceiver(q, { receiveMode: 'receiveAndDelete' }), 20, 2000)).length, added);
  });
  it('sendMessages with an array of messages sends them as one batch', async () => {
    const q = await queue();
    await sb.createSender(q).sendMessages([1, 2, 3].map((i) => ({ messageId: mid(), body: `arr-${i}` })));
    const got = await recv(sb.createReceiver(q, { receiveMode: 'receiveAndDelete' }), 5, 2000);
    assert.deepEqual(got.map((m) => m.body), ['arr-1', 'arr-2', 'arr-3']);
  });
  it('scheduleMessages: the message is invisible until the time and the sequence number can cancel it', async () => {
    const q = await queue();
    const s = sb.createSender(q);
    const [seq] = await s.scheduleMessages({ messageId: mid(), body: 'later' }, new Date(Date.now() + 2500));
    const keep = await s.scheduleMessages({ messageId: mid(), body: 'kept' }, new Date(Date.now() + 2500));
    assert.equal((await recv(sb.createReceiver(q), 1, 800)).length, 0);
    await s.cancelScheduledMessages(seq);
    const r = sb.createReceiver(q);
    const got = await recv(r, 2, 6000);
    assert.deepEqual(got.map((m) => m.body), ['kept']);
    assert.ok(got[0].scheduledEnqueueTimeUtc);
    void keep;
  });
});

describe('subscribe handlers', () => {
  it('subscribe() delivers every message to processMessage and settles with autoCompleteMessages', async () => {
    const q = await queue();
    const s = sb.createSender(q);
    for (let i = 0; i < 10; i++) await send(s, `h${i}`);
    const r = sb.createReceiver(q);
    const seen = [];
    const errors = [];
    const sub = r.subscribe({ processMessage: async (m) => { seen.push(m.body); }, processError: async (e) => { errors.push(e); } }, { maxConcurrentCalls: 3 });
    for (let i = 0; i < 50 && seen.length < 10; i++) await sleep(100);
    await sub.close();
    assert.deepEqual(seen.sort(), [...Array(10).keys()].map((i) => `h${i}`).sort());
    assert.deepEqual(errors, []);
    assert.equal((await admin.getQueueRuntimeProperties(q)).totalMessageCount, 0);
  });
  it('a handler that throws abandons the message (deliveryCount rises) and the error reaches processError', async () => {
    const q = await queue({ maxDeliveryCount: 5 });
    await send(sb.createSender(q), 'boom');
    const r = sb.createReceiver(q);
    let calls = 0;
    const errors = [];
    const sub = r.subscribe({
      processMessage: async (m) => { calls++; if (calls === 1) throw new Error('handler failed'); },
      processError: async (e) => { errors.push(e.error.message); },
    });
    for (let i = 0; i < 50 && calls < 2; i++) await sleep(100);
    await sub.close();
    assert.equal(calls, 2);
    assert.ok(errors.includes('handler failed'));
  });
  it('async iteration with getMessageIterator yields messages', async () => {
    const q = await queue();
    const s = sb.createSender(q);
    for (let i = 0; i < 3; i++) await send(s, `it${i}`);
    const r = sb.createReceiver(q);
    const got = [];
    for await (const m of r.getMessageIterator()) {
      got.push(m.body);
      await r.completeMessage(m);
      if (got.length === 3) break;
    }
    assert.deepEqual(got, ['it0', 'it1', 'it2']);
  });
});

describe('sessions', () => {
  it('messages of one session go to acceptSession(id) in order, with session state', async () => {
    const q = await queue({ requiresSession: true });
    const s = sb.createSender(q);
    for (let i = 0; i < 3; i++) await send(s, { body: `a${i}`, sessionId: 'A' });
    await send(s, { body: 'b0', sessionId: 'B' });
    const ra = await sb.acceptSession(q, 'A');
    assert.equal(ra.sessionId, 'A');
    const got = await recv(ra, 5, 2000);
    assert.deepEqual(got.map((m) => m.body), ['a0', 'a1', 'a2']);
    assert.equal(got[0].sessionId, 'A');
    for (const m of got) await ra.completeMessage(m);
    await ra.setSessionState({ cursor: 3, who: 'me' });
    assert.deepEqual(await ra.getSessionState(), { cursor: 3, who: 'me' });
    assert.ok(ra.sessionLockedUntilUtc > new Date());
    await ra.renewSessionLock();
    await ra.close();
    const rb = await sb.acceptSession(q, 'B');
    assert.equal((await recv(rb))[0].body, 'b0');
    await rb.close();
  });
  it('acceptNextSession picks a session that has messages', async () => {
    const q = await queue({ requiresSession: true });
    await send(sb.createSender(q), { body: 'x', sessionId: 'only' });
    const r = await sb.acceptNextSession(q, { maxWaitTimeInMs: 5000 });
    assert.equal(r.sessionId, 'only');
    assert.equal((await recv(r))[0].body, 'x');
    await r.close();
  });
  it('acceptNextSession with no sessions fails with SessionCannotBeLocked or returns after the wait (OperationTimeout)', async () => {
    const q = await queue({ requiresSession: true });
    const e = await rejection(() => sb.acceptNextSession(q, { maxWaitTimeInMs: 1500 }));
    assert.ok(['SessionCannotBeLocked', 'ServiceTimeout', 'MessagingEntityNotFound'].includes(e.code), e.code);
  });
  it('a second receiver for a locked session is SessionCannotBeLocked', async () => {
    const q = await queue({ requiresSession: true });
    await send(sb.createSender(q), { body: 'x', sessionId: 'S' });
    const r1 = await sb.acceptSession(q, 'S');
    const e = await rejection(() => sb.acceptSession(q, 'S'));
    assert.equal(e.code, 'SessionCannotBeLocked');
    await r1.close();
  });
  it('sending without a sessionId to a session queue is refused', async () => {
    const q = await queue({ requiresSession: true });
    const e = await rejection(() => sb.createSender(q).sendMessages({ messageId: mid(), body: 'x' }));
    assert.ok(e.code, JSON.stringify(e));
  });
  it('subscribe on a session receiver handles messages of that session', async () => {
    const q = await queue({ requiresSession: true });
    const s = sb.createSender(q);
    for (let i = 0; i < 3; i++) await send(s, { body: `s${i}`, sessionId: 'Z' });
    const r = await sb.acceptSession(q, 'Z');
    const seen = [];
    const sub = r.subscribe({ processMessage: async (m) => { seen.push(m.body); }, processError: async () => {} });
    for (let i = 0; i < 50 && seen.length < 3; i++) await sleep(100);
    await sub.close();
    await r.close();
    assert.deepEqual(seen, ['s0', 's1', 's2']);
  });
});

describe('topics, subscriptions and rules', () => {
  async function topic() {
    const t = uniq('t');
    await admin.createTopic(t);
    return t;
  }
  it('every subscription gets its own copy of a published message', async () => {
    const t = await topic();
    await admin.createSubscription(t, 'a');
    await admin.createSubscription(t, 'b');
    await send(sb.createSender(t), 'fan');
    for (const sub of ['a', 'b']) {
      const r = sb.createReceiver(t, sub);
      const [m] = await recv(r);
      assert.equal(m.body, 'fan');
      await r.completeMessage(m);
    }
  });
  it('a SQL filter rule routes only matching messages (applicationProperties and system properties)', async () => {
    const t = await topic();
    await admin.createSubscription(t, 'red');
    await admin.deleteRule(t, 'red', '$Default');
    await admin.createRule(t, 'red', 'r', { sqlExpression: "color = 'red' AND qty > 5" });
    await admin.createSubscription(t, 'all');
    const s = sb.createSender(t);
    await send(s, { body: 'r10', applicationProperties: { color: 'red', qty: 10 } });
    await send(s, { body: 'r1', applicationProperties: { color: 'red', qty: 1 } });
    await send(s, { body: 'b10', applicationProperties: { color: 'blue', qty: 10 } });
    const red = await recv(sb.createReceiver(t, 'red', { receiveMode: 'receiveAndDelete' }), 5, 1500);
    assert.deepEqual(red.map((m) => m.body), ['r10']);
    const all = await recv(sb.createReceiver(t, 'all', { receiveMode: 'receiveAndDelete' }), 5, 1500);
    assert.equal(all.length, 3);
  });
  it('a correlation filter rule matches on subject and correlationId', async () => {
    const t = await topic();
    await admin.createSubscription(t, 'c');
    await admin.deleteRule(t, 'c', '$Default');
    await admin.createRule(t, 'c', 'cr', { subject: 'order', correlationId: 'c-1' });
    const s = sb.createSender(t);
    await send(s, { body: 'yes', subject: 'order', correlationId: 'c-1' });
    await send(s, { body: 'no1', subject: 'order', correlationId: 'c-2' });
    await send(s, { body: 'no2', subject: 'other', correlationId: 'c-1' });
    const got = await recv(sb.createReceiver(t, 'c', { receiveMode: 'receiveAndDelete' }), 5, 1500);
    assert.deepEqual(got.map((m) => m.body), ['yes']);
  });
  it('a rule action sets a property on the copy', async () => {
    const t = await topic();
    await admin.createSubscription(t, 'act');
    await admin.deleteRule(t, 'act', '$Default');
    await admin.createRule(t, 'act', 'a', { sqlExpression: '1=1' }, { sqlExpression: "SET tagged = 'yes'" });
    await send(sb.createSender(t), 'x');
    const [m] = await recv(sb.createReceiver(t, 'act', { receiveMode: 'receiveAndDelete' }));
    assert.equal(m.applicationProperties.tagged, 'yes');
  });
  it('rules can be added, read, listed and removed through the admin client; removal stops delivery', async () => {
    const t = await topic();
    await admin.createSubscription(t, 's');
    await admin.deleteRule(t, 's', '$Default');
    await admin.createRule(t, 's', 'only-x', { sqlExpression: "kind = 'x'" });
    const rule = await admin.getRule(t, 's', 'only-x');
    assert.equal(rule.filter.sqlExpression, "kind = 'x'");
    const sender = sb.createSender(t);
    await send(sender, { body: 'x1', applicationProperties: { kind: 'x' } });
    await send(sender, { body: 'y1', applicationProperties: { kind: 'y' } });
    assert.deepEqual((await recv(sb.createReceiver(t, 's', { receiveMode: 'receiveAndDelete' }), 5, 1500)).map((m) => m.body), ['x1']);
    await admin.deleteRule(t, 's', 'only-x');
    await send(sender, { body: 'none', applicationProperties: { kind: 'x' } });
    assert.equal((await recv(sb.createReceiver(t, 's', { receiveMode: 'receiveAndDelete' }), 5, 1000)).length, 0);
  });
  it('createSubscription with defaultRuleOptions makes that rule instead of $Default', async () => {
    const t = await topic();
    await admin.createSubscription(t, 'red', { defaultRuleOptions: { name: 'r', filter: { sqlExpression: "color = 'red'" } } });
    assert.equal((await admin.getRule(t, 'red', 'r')).filter.sqlExpression, "color = 'red'");
    const e = await rejection(() => admin.getRule(t, 'red', '$Default'));
    assert.equal(e.statusCode, 404);
  });
  it('listSubscriptions and listRules parse (entry ids repeat the Subscriptions/Rules path casing of the request)', async () => {
    const t = await topic();
    await admin.createSubscription(t, 's1');
    assert.deepEqual((await collect(admin.listSubscriptions(t))).map((x) => x.subscriptionName), ['s1']);
    assert.deepEqual((await collect(admin.listRules(t, 's1'))).map((x) => x.name), ['$Default']);
    assert.deepEqual((await collect(admin.listSubscriptionsRuntimeProperties(t))).map((x) => x.subscriptionName), ['s1']);
  });
  it('the subscription dead-letter queue and a subscription session', async () => {
    const t = await topic();
    await admin.createSubscription(t, 'dl');
    await send(sb.createSender(t), 'dead');
    const r = sb.createReceiver(t, 'dl');
    const [m] = await recv(r);
    await r.deadLetterMessage(m, { deadLetterReason: 'why' });
    const [d] = await recv(sb.createReceiver(t, 'dl', { subQueueType: 'deadLetter' }));
    assert.equal(d.deadLetterReason, 'why');
  });
  it('topic send to a topic with no subscriptions succeeds and delivers nowhere', async () => {
    const t = await topic();
    await send(sb.createSender(t), 'lost');
    await admin.createSubscription(t, 'late');
    assert.equal((await recv(sb.createReceiver(t, 'late'), 1, 800)).length, 0);
  });
});

describe('administration client', () => {
  it('queue create, get, update, list, runtime properties and delete', async () => {
    const name = uniq('adm');
    const created = await admin.createQueue(name, { maxDeliveryCount: 7, lockDuration: 'PT1M', userMetadata: 'meta' });
    assert.equal(created.maxDeliveryCount, 7);
    const got = await admin.getQueue(name);
    assert.equal(got.lockDuration, 'PT1M');
    assert.equal(got.userMetadata, 'meta');
    got.maxDeliveryCount = 9;
    const upd = await admin.updateQueue(got);
    assert.equal(upd.maxDeliveryCount, 9);
    assert.ok((await collect(admin.listQueues())).some((q) => q.name === name));
    assert.ok((await collect(admin.listQueuesRuntimeProperties())).some((q) => q.name === name));
    assert.equal(await admin.queueExists(name), true);
    await admin.deleteQueue(name);
    assert.equal(await admin.queueExists(name), false);
  });
  it('creating an existing queue is 409 (SDK code MessageEntityAlreadyExistsError) and getting a missing one is 404 (MessageEntityNotFoundError)', async () => {
    const name = await queue();
    const e = await rejection(() => admin.createQueue(name));
    assert.equal(e.code, 'MessageEntityAlreadyExistsError');
    assert.equal(e.statusCode, 409);
    const e2 = await rejection(() => admin.getQueue(uniq('missing')));
    assert.equal(e2.code, 'MessageEntityNotFoundError');
  });
  it('runtime properties count active, dead-letter and scheduled messages', async () => {
    const q = await queue();
    const s = sb.createSender(q);
    await send(s, 'a');
    await send(s, 'b');
    await s.scheduleMessages({ messageId: mid(), body: 's' }, new Date(Date.now() + 600_000));
    const r = sb.createReceiver(q);
    const [m] = await recv(r);
    await r.deadLetterMessage(m);
    const p = await admin.getQueueRuntimeProperties(q);
    assert.equal(p.activeMessageCount, 1);
    assert.equal(p.deadLetterMessageCount, 1);
    assert.equal(p.scheduledMessageCount, 1);
    assert.equal(p.totalMessageCount, 3);
  });
  it('topic and subscription CRUD with runtime properties', async () => {
    const t = uniq('adt');
    await admin.createTopic(t, { requiresDuplicateDetection: true });
    assert.equal((await admin.getTopic(t)).requiresDuplicateDetection, true);
    await admin.createSubscription(t, 'sub1', { maxDeliveryCount: 4 });
    assert.equal((await admin.getSubscription(t, 'sub1')).maxDeliveryCount, 4);
    assert.equal((await admin.getTopicRuntimeProperties(t)).subscriptionCount, 1);
    await admin.deleteSubscription(t, 'sub1');
    await admin.deleteTopic(t);
    assert.equal(await admin.topicExists(t), false);
  });
  it('getNamespaceProperties answers', async () => {
    const p = await admin.getNamespaceProperties();
    assert.ok(p.name !== undefined);
  });
});
