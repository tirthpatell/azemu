// ServiceBusRuleManager uses the AMQP $management node (add-rule, remove-rule, enumerate-rules). The emulator's docs say this encoding was
// "reconstructed from memory" and only tested against its own client, so these tests are the first check with a real SDK.
import { describe, it, after } from 'node:test';
import assert from 'node:assert/strict';
import { ServiceBusClient, ServiceBusAdministrationClient } from '@azure/service-bus';
import { env, uniq, rejection, track, closeTracked } from '../lib/emu.mjs';

const emu = env();
const ns = uniq('rm');
const cs = `Endpoint=sb://${ns}.servicebus.localhost:${emu.port};SharedAccessKeyName=k;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true`;
const sb = track(new ServiceBusClient(cs));
const admin = new ServiceBusAdministrationClient(cs);
after(() => closeTracked());
let n = 0;
const mid = () => `r-${++n}`;

async function topicSub() {
  const t = uniq('t');
  await admin.createTopic(t);
  await admin.createSubscription(t, 's');
  return { t, s: 's' };
}
const names = async (rm) => { const out = []; for await (const r of rm.listRules()) out.push(r.name); return out.sort(); };

describe('ServiceBusRuleManager', () => {
  it('deleteRule removes a rule through the AMQP $management node (remove-rule)', async () => {
    const { t, s } = await topicSub();
    await sb.createRuleManager(t, s).deleteRule('$Default');
    const e = await rejection(() => admin.getRule(t, s, '$Default'));
    assert.ok(e.statusCode === 404 || e.code === 'MessageEntityNotFoundError', `${e.code} ${e.statusCode}`);
  });
  it('deleteRule of a rule that does not exist is an error', async () => {
    const { t, s } = await topicSub();
    const e = await rejection(() => sb.createRuleManager(t, s).deleteRule('nope'));
    assert.ok(e instanceof Error);
  });
  it('listRules shows the $Default rule of a new subscription as a true filter', async () => {
    const { t, s } = await topicSub();
    const rm = sb.createRuleManager(t, s);
    const rules = [];
    for await (const r of rm.listRules()) rules.push(r);
    assert.equal(rules.length, 1);
    assert.equal(rules[0].name, '$Default');
    assert.ok(rules[0].filter.sqlExpression === '1=1' || rules[0].filter.sqlExpression === undefined);
  });
  it('createRule with a SQL filter, a correlation filter and a SQL action, then deleteRule', async () => {
    const { t, s } = await topicSub();
    const rm = sb.createRuleManager(t, s);
    await rm.deleteRule('$Default');
    await rm.createRule('sql', { sqlExpression: "color = 'red'", sqlParameters: {} }, { sqlExpression: "SET touched = 'yes'" });
    await rm.createRule('corr', { subject: 'order', correlationId: 'c1', applicationProperties: { kind: 'x' } });
    assert.deepEqual(await names(rm), ['corr', 'sql']);
    const byName = {};
    for await (const r of rm.listRules()) byName[r.name] = r;
    assert.equal(byName.sql.filter.sqlExpression, "color = 'red'");
    assert.equal(byName.sql.action.sqlExpression, "SET touched = 'yes'");
    assert.equal(byName.corr.filter.subject, 'order');
    assert.equal(byName.corr.filter.correlationId, 'c1');
    assert.equal(byName.corr.filter.applicationProperties.kind, 'x');
    await rm.deleteRule('sql');
    assert.deepEqual(await names(rm), ['corr']);
  });
  it('rules added through the rule manager route messages (and the admin client sees them)', async () => {
    const { t, s } = await topicSub();
    const rm = sb.createRuleManager(t, s);
    await rm.deleteRule('$Default');
    await rm.createRule('red', { sqlExpression: "color = 'red'" });
    assert.equal((await admin.getRule(t, s, 'red')).filter.sqlExpression, "color = 'red'");
    const sender = sb.createSender(t);
    await sender.sendMessages({ messageId: mid(), body: 'r', applicationProperties: { color: 'red' } });
    await sender.sendMessages({ messageId: mid(), body: 'b', applicationProperties: { color: 'blue' } });
    const got = await sb.createReceiver(t, s, { receiveMode: 'receiveAndDelete' }).receiveMessages(5, { maxWaitTimeInMs: 1500 });
    assert.deepEqual(got.map((m) => m.body), ['r']);
  });
  it('a rule created with the admin client is listed by the rule manager', async () => {
    const { t, s } = await topicSub();
    await admin.createRule(t, s, 'viaAdmin', { sqlExpression: 'qty > 3' });
    assert.ok((await names(sb.createRuleManager(t, s))).includes('viaAdmin'));
  });
  it('creating a rule with an existing name fails with MessageEntityAlreadyExistsError / 409', async () => {
    const { t, s } = await topicSub();
    const rm = sb.createRuleManager(t, s);
    await rm.createRule('dup', { sqlExpression: '1=1' });
    const e = await rejection(() => rm.createRule('dup', { sqlExpression: '1=1' }));
    assert.ok(e.code === 'MessageEntityAlreadyExistsError' || e.code === 'MessagingEntityAlreadyExists' || e.statusCode === 409, `${e.code} ${e.message}`);
  });
  it('a SQL filter with parameters (sqlParameters) matches', { skip: 'sdk: ServiceBusRuleManager.createRule never sends sqlParameters (core/managementClient.js addRule sends only {expression}), so the rule "qty > @min AND label = @label" has no parameter values on any service; the HTTP admin client does send them' }, async () => {
    const { t, s } = await topicSub();
    const rm = sb.createRuleManager(t, s);
    await rm.deleteRule('$Default');
    await rm.createRule('p', { sqlExpression: 'qty > @min AND label = @label', sqlParameters: { '@min': 5, '@label': 'a' } });
    const sender = sb.createSender(t);
    await sender.sendMessages({ messageId: mid(), body: 'yes', applicationProperties: { qty: 9, label: 'a' } });
    await sender.sendMessages({ messageId: mid(), body: 'no', applicationProperties: { qty: 9, label: 'b' } });
    const got = await sb.createReceiver(t, s, { receiveMode: 'receiveAndDelete' }).receiveMessages(5, { maxWaitTimeInMs: 1500 });
    assert.deepEqual(got.map((m) => m.body), ['yes']);
  });
});
