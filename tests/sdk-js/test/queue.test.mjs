import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { QueueServiceClient, QueueClient, StorageSharedKeyCredential, QueueSASPermissions, generateQueueSASQueryParameters, AccountSASPermissions, generateAccountSASQueryParameters } from '@azure/storage-queue';
import { env, uniq, DEV_ACCOUNT, DEV_KEY, collect, rejection, sleep, fakeCredential } from '../lib/emu.mjs';

const emu = env();
const cred = new StorageSharedKeyCredential(DEV_ACCOUNT, DEV_KEY);
const svc = new QueueServiceClient(emu.url('queue', DEV_ACCOUNT), cred);
const code = (e) => e.details?.errorCode ?? e.code;
async function newQueue() {
  const q = svc.getQueueClient(uniq('q'));
  await q.create();
  return q;
}

describe('queue lifecycle', () => {
  it('create returns 201; creating again with the same metadata is 204 (no error) and with other metadata is QueueAlreadyExists 409', async () => {
    const q = svc.getQueueClient(uniq('q'));
    const a = await q.create({ metadata: { a: '1' } });
    assert.equal(a._response.status, 201);
    const b = await q.create({ metadata: { a: '1' } });
    assert.equal(b._response.status, 204);
    const e = await rejection(() => q.create({ metadata: { a: '2' } }));
    assert.equal(e.statusCode, 409);
    assert.equal(code(e), 'QueueAlreadyExists');
  });
  it('createIfNotExists and exists and deleteIfExists behave', async () => {
    const q = svc.getQueueClient(uniq('q'));
    assert.equal(await q.exists(), false);
    assert.equal((await q.createIfNotExists()).succeeded, true);
    assert.equal((await q.createIfNotExists()).succeeded, false);
    assert.equal(await q.exists(), true);
    assert.equal((await q.deleteIfExists()).succeeded, true);
    assert.equal((await q.deleteIfExists()).succeeded, false);
  });
  it('list queues with prefix, metadata and paging', async () => {
    const pre = uniq('lq');
    for (let i = 0; i < 5; i++) await svc.getQueueClient(`${pre}${i}`).create({ metadata: { n: String(i) } });
    const names = [];
    for await (const p of svc.listQueues({ prefix: pre, includeMetadata: true }).byPage({ maxPageSize: 2 })) {
      assert.ok(p.queueItems.length <= 2);
      names.push(...p.queueItems.map((x) => `${x.name}:${x.metadata.n}`));
    }
    assert.deepEqual(names, [0, 1, 2, 3, 4].map((i) => `${pre}${i}:${i}`));
  });
  it('metadata set and read back, getProperties reports approximateMessagesCount', async () => {
    const q = await newQueue();
    await q.setMetadata({ Env: 'dev' });
    await q.sendMessage('a');
    await q.sendMessage('b');
    const p = await q.getProperties();
    assert.equal(p.metadata.env, 'dev');
    assert.equal(p.approximateMessagesCount, 2);
  });
  it('a missing queue is QueueNotFound 404', async () => {
    const q = svc.getQueueClient(uniq('nope'));
    const e = await rejection(() => q.sendMessage('x'));
    assert.equal(e.statusCode, 404);
    assert.equal(code(e), 'QueueNotFound');
  });
  it('an invalid queue name (upper case) is rejected with 400 InvalidResourceName', async () => {
    const q = svc.getQueueClient('BadName');
    const e = await rejection(() => q.create());
    assert.equal(e.statusCode, 400);
  });
  it('queue access policy round trips', async () => {
    const q = await newQueue();
    await q.setAccessPolicy([{ id: 'p', accessPolicy: { startsOn: new Date(Date.now() - 1000), expiresOn: new Date(Date.now() + 3600_000), permissions: 'ra' } }]);
    const r = await q.getAccessPolicy();
    assert.equal(r.signedIdentifiers[0].id, 'p');
    assert.equal(r.signedIdentifiers[0].accessPolicy.permissions, 'ra');
  });
});

describe('queue messages', () => {
  it('send then receive returns the text, a dequeue count of 1 and pop receipt', async () => {
    const q = await newQueue();
    const s = await q.sendMessage('hello');
    assert.ok(s.messageId && s.popReceipt && s.insertedOn && s.expiresOn && s.nextVisibleOn);
    const r = await q.receiveMessages();
    assert.equal(r.receivedMessageItems.length, 1);
    const m = r.receivedMessageItems[0];
    assert.equal(m.messageText, 'hello');
    assert.equal(m.messageId, s.messageId);
    assert.equal(m.dequeueCount, 1);
  });
  it('a received message is invisible for the visibility timeout and then comes back with dequeueCount 2', async () => {
    const q = await newQueue();
    await q.sendMessage('x');
    const first = await q.receiveMessages({ visibilityTimeout: 1 });
    assert.equal(first.receivedMessageItems.length, 1);
    assert.equal((await q.receiveMessages()).receivedMessageItems.length, 0);
    await sleep(1300);
    const again = await q.receiveMessages();
    assert.equal(again.receivedMessageItems.length, 1);
    assert.equal(again.receivedMessageItems[0].dequeueCount, 2);
  });
  it('receive up to 32 messages in one call returns them in send order', async () => {
    const q = await newQueue();
    for (let i = 0; i < 10; i++) await q.sendMessage(`m${i}`);
    const r = await q.receiveMessages({ numberOfMessages: 10 });
    assert.deepEqual(r.receivedMessageItems.map((m) => m.messageText), [...Array(10).keys()].map((i) => `m${i}`));
  });
  it('numberOfMessages above 32 is OutOfRangeQueryParameterValue 400', async () => {
    const q = await newQueue();
    const e = await rejection(() => q.receiveMessages({ numberOfMessages: 33 }));
    assert.equal(e.statusCode, 400);
  });
  it('peek does not change visibility or dequeueCount', async () => {
    const q = await newQueue();
    await q.sendMessage('p');
    const a = await q.peekMessages({ numberOfMessages: 5 });
    assert.equal(a.peekedMessageItems.length, 1);
    assert.equal(a.peekedMessageItems[0].dequeueCount, 0);
    const b = await q.peekMessages();
    assert.equal(b.peekedMessageItems[0].messageText, 'p');
    assert.equal((await q.receiveMessages()).receivedMessageItems[0].dequeueCount, 1);
  });
  it('deleteMessage removes it; a second delete with the same receipt is MessageNotFound 404', async () => {
    const q = await newQueue();
    await q.sendMessage('d');
    const m = (await q.receiveMessages()).receivedMessageItems[0];
    await q.deleteMessage(m.messageId, m.popReceipt);
    assert.equal((await q.peekMessages()).peekedMessageItems.length, 0);
    const e = await rejection(() => q.deleteMessage(m.messageId, m.popReceipt));
    assert.equal(e.statusCode, 404);
  });
  it('deleting with a stale pop receipt is PopReceiptMismatch 400', async () => {
    const q = await newQueue();
    await q.sendMessage('d');
    const m = (await q.receiveMessages({ visibilityTimeout: 1 })).receivedMessageItems[0];
    await sleep(1200);
    await q.receiveMessages();
    const e = await rejection(() => q.deleteMessage(m.messageId, m.popReceipt));
    assert.equal(e.statusCode, 400);
    assert.equal(code(e), 'PopReceiptMismatch');
  });
  it('updateMessage changes the text and returns a new pop receipt that can delete it', async () => {
    const q = await newQueue();
    await q.sendMessage('old');
    const m = (await q.receiveMessages()).receivedMessageItems[0];
    const u = await q.updateMessage(m.messageId, m.popReceipt, 'new', 0);
    assert.notEqual(u.popReceipt, m.popReceipt);
    const again = (await q.receiveMessages()).receivedMessageItems[0];
    assert.equal(again.messageText, 'new');
    await q.deleteMessage(again.messageId, again.popReceipt);
  });
  it('updateMessage with a visibility timeout extends the invisibility', async () => {
    const q = await newQueue();
    await q.sendMessage('v');
    const m = (await q.receiveMessages({ visibilityTimeout: 1 })).receivedMessageItems[0];
    await q.updateMessage(m.messageId, m.popReceipt, undefined, 30);
    await sleep(1300);
    assert.equal((await q.receiveMessages()).receivedMessageItems.length, 0);
  });
  it('sendMessage with a visibilityTimeout stays hidden until it expires', async () => {
    const q = await newQueue();
    await q.sendMessage('later', { visibilityTimeout: 1 });
    assert.equal((await q.receiveMessages()).receivedMessageItems.length, 0);
    await sleep(1300);
    assert.equal((await q.receiveMessages()).receivedMessageItems.length, 1);
  });
  it('messageTimeToLive of 1 second removes the message', async () => {
    const q = await newQueue();
    await q.sendMessage('ttl', { messageTimeToLive: 1 });
    await sleep(2200);
    assert.equal((await q.peekMessages()).peekedMessageItems.length, 0);
  });
  it('messageTimeToLive -1 means the message never expires (expiresOn is far in the future)', async () => {
    const q = await newQueue();
    const s = await q.sendMessage('forever', { messageTimeToLive: -1 });
    assert.ok(s.expiresOn.getUTCFullYear() >= 9999 || s.expiresOn.getTime() > Date.now() + 365 * 86400_000 * 100);
  });
  it('clearMessages empties the queue', async () => {
    const q = await newQueue();
    for (let i = 0; i < 3; i++) await q.sendMessage('x');
    await q.clearMessages();
    assert.equal((await q.peekMessages({ numberOfMessages: 5 })).peekedMessageItems.length, 0);
  });
  it('messages with XML-special characters, unicode and emoji round trip', async () => {
    const q = await newQueue();
    const text = '<a href="x">&amp; é 日本語 😀 </a>\n  leading';
    await q.sendMessage(text);
    assert.equal((await q.receiveMessages()).receivedMessageItems[0].messageText, text);
  });
  it('a message larger than 64 KiB is RequestBodyTooLarge 413 (as recorded from Azure)', async () => {
    const q = await newQueue();
    const e = await rejection(() => q.sendMessage('x'.repeat(70_000)));
    assert.equal(e.statusCode, 413);
    assert.equal(code(e), 'RequestBodyTooLarge');
  });
  it('queue and message names with a base64 payload are stored as sent', async () => {
    const q = await newQueue();
    const b64 = Buffer.from('binary\u0000data').toString('base64');
    await q.sendMessage(b64);
    assert.equal((await q.receiveMessages()).receivedMessageItems[0].messageText, b64);
  });
});

describe('queue SAS', () => {
  it('a queue SAS with add+process (ap) lets another client send and receive', async () => {
    const q = await newQueue();
    const sas = generateQueueSASQueryParameters({ queueName: q.name, permissions: QueueSASPermissions.parse('rap'), expiresOn: new Date(Date.now() + 3600_000) }, cred).toString();
    const other = new QueueClient(`${q.url}?${sas}`);
    await other.sendMessage('via sas');
    assert.equal((await other.receiveMessages()).receivedMessageItems[0].messageText, 'via sas');
  });
  it('a read-only queue SAS cannot send (AuthorizationPermissionMismatch 403)', async () => {
    const q = await newQueue();
    const sas = generateQueueSASQueryParameters({ queueName: q.name, permissions: QueueSASPermissions.parse('r'), expiresOn: new Date(Date.now() + 3600_000) }, cred).toString();
    const e = await rejection(() => new QueueClient(`${q.url}?${sas}`).sendMessage('no'));
    assert.equal(e.statusCode, 403);
  });
  it('an account SAS for the queue service lists queues', async () => {
    const sas = generateAccountSASQueryParameters({ services: 'q', resourceTypes: 'sco', permissions: AccountSASPermissions.parse('rwdlacup'), expiresOn: new Date(Date.now() + 3600_000) }, cred).toString();
    const s = new QueueServiceClient(`${emu.url('queue', DEV_ACCOUNT)}?${sas}`);
    const q = s.getQueueClient(uniq('asq'));
    await q.create();
    assert.ok((await collect(s.listQueues({ prefix: q.name }))).length === 1);
  });
  it('a SAS signed with a wrong key is AuthenticationFailed 403', async () => {
    const q = await newQueue();
    const bad = new StorageSharedKeyCredential(DEV_ACCOUNT, Buffer.alloc(64, 3).toString('base64'));
    const sas = generateQueueSASQueryParameters({ queueName: q.name, permissions: QueueSASPermissions.parse('rap'), expiresOn: new Date(Date.now() + 3600_000) }, bad).toString();
    const e = await rejection(() => new QueueClient(`${q.url}?${sas}`).sendMessage('x'));
    assert.equal(e.statusCode, 403);
    assert.equal(code(e), 'AuthenticationFailed');
  });
});

describe('queue auth and service', () => {
  it('a bearer token over https works (any token)', async () => {
    const s = new QueueServiceClient(emu.url('queue', uniq('tok'), 'https'), fakeCredential);
    const q = s.getQueueClient(uniq('q'));
    await q.create();
    await q.sendMessage('x');
    assert.equal((await q.receiveMessages()).receivedMessageItems.length, 1);
  });
  it('a wrong shared key for devstoreaccount1 is AuthenticationFailed 403', async () => {
    const bad = new QueueServiceClient(emu.url('queue', DEV_ACCOUNT), new StorageSharedKeyCredential(DEV_ACCOUNT, Buffer.alloc(64, 9).toString('base64')));
    const e = await rejection(() => collect(bad.listQueues()));
    assert.equal(e.statusCode, 403);
    assert.equal(code(e), 'AuthenticationFailed');
  });
  it('service properties round trip (logging and CORS)', async () => {
    const acct = uniq('qsp');
    const s = new QueueServiceClient(emu.url('queue', acct), new StorageSharedKeyCredential(acct, DEV_KEY));
    await s.setProperties({ cors: [{ allowedOrigins: '*', allowedMethods: 'GET', allowedHeaders: '*', exposedHeaders: '*', maxAgeInSeconds: 5 }] });
    assert.equal((await s.getProperties()).cors[0].allowedMethods, 'GET');
  });
});
