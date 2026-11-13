import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { AppConfigurationClient, featureFlagContentType, secretReferenceContentType, parseFeatureFlag, parseSecretReference, isFeatureFlag, isSecretReference } from '@azure/app-configuration';
import { env, uniq, fakeCredential, collect, rejection } from '../lib/emu.mjs';

const emu = env();
const conn = (store) => `Endpoint=${emu.url('azconfig', store, 'https')};Id=any;Secret=YXplbXU=`;
const client = () => new AppConfigurationClient(conn(uniq('ac')));
const code = (e) => e.code;

describe('settings', () => {
  it('addConfigurationSetting then getConfigurationSetting returns the value, etag, label, contentType and tags', async () => {
    const c = client();
    const a = await c.addConfigurationSetting({ key: 'app/color', value: 'blue', label: 'prod', contentType: 'text/plain', tags: { team: 'x' } });
    assert.ok(a.etag);
    const g = await c.getConfigurationSetting({ key: 'app/color', label: 'prod' });
    assert.equal(g.value, 'blue');
    assert.equal(g.label, 'prod');
    assert.equal(g.contentType, 'text/plain');
    assert.equal(g.tags.team, 'x');
    assert.equal(g.isReadOnly, false);
    assert.ok(g.lastModified instanceof Date);
    assert.equal(g.etag, a.etag);
  });
  it('adding the same key and label twice is a 412 error (add fails when it exists)', async () => {
    const c = client();
    await c.addConfigurationSetting({ key: 'k', value: '1' });
    const e = await rejection(() => c.addConfigurationSetting({ key: 'k', value: '2' }));
    assert.equal(e.statusCode, 412);
  });
  it('setConfigurationSetting upserts and gives a new etag', async () => {
    const c = client();
    const a = await c.setConfigurationSetting({ key: 'k', value: '1' });
    const b = await c.setConfigurationSetting({ key: 'k', value: '2' });
    assert.notEqual(a.etag, b.etag);
    assert.equal((await c.getConfigurationSetting({ key: 'k' })).value, '2');
  });
  it('a missing key is 404', async () => {
    const c = client();
    const e = await rejection(() => c.getConfigurationSetting({ key: 'nope' }));
    assert.equal(e.statusCode, 404);
  });
  it('no label and a label are different settings; the label-less one has label undefined', async () => {
    const c = client();
    await c.setConfigurationSetting({ key: 'k', value: 'nolabel' });
    await c.setConfigurationSetting({ key: 'k', value: 'dev', label: 'dev' });
    assert.equal((await c.getConfigurationSetting({ key: 'k' })).value, 'nolabel');
    assert.equal((await c.getConfigurationSetting({ key: 'k' })).label, undefined);
    assert.equal((await c.getConfigurationSetting({ key: 'k', label: 'dev' })).value, 'dev');
  });
  it('keys and labels with special characters round trip (slash, space, unicode, percent, plus)', async () => {
    const c = client();
    const keys = ['a/b c', 'ünï/日本', 'pct%25', 'plus+sign', 'colon:sep', 'dot.dot'];
    for (const k of keys) {
      await c.setConfigurationSetting({ key: k, value: `v-${k}`, label: 'l 1' });
      const g = await c.getConfigurationSetting({ key: k, label: 'l 1' });
      assert.equal(g.value, `v-${k}`);
      assert.equal(g.key, k);
    }
    const listed = (await collect(c.listConfigurationSettings())).map((s) => s.key).sort();
    assert.deepEqual(listed, [...keys].sort());
  });
  it('getConfigurationSetting with onlyIfChanged and the current etag returns statusCode 304', async () => {
    const c = client();
    const s = await c.setConfigurationSetting({ key: 'k', value: '1' });
    const r = await c.getConfigurationSetting({ key: 'k', etag: s.etag }, { onlyIfChanged: true });
    assert.equal(r.statusCode, 304);
  });
  it('set with onlyIfUnchanged and a stale etag is 412', async () => {
    const c = client();
    const s = await c.setConfigurationSetting({ key: 'k', value: '1' });
    await c.setConfigurationSetting({ key: 'k', value: '2' });
    const e = await rejection(() => c.setConfigurationSetting({ key: 'k', value: '3', etag: s.etag }, { onlyIfUnchanged: true }));
    assert.equal(e.statusCode, 412);
  });
  it('delete with the right etag succeeds; deleting a missing key returns statusCode 204 without error', async () => {
    const c = client();
    const s = await c.setConfigurationSetting({ key: 'k', value: '1' });
    const d = await c.deleteConfigurationSetting({ key: 'k', etag: s.etag }, { onlyIfUnchanged: true });
    assert.equal(d.statusCode, 200);
    const again = await c.deleteConfigurationSetting({ key: 'k' });
    assert.equal(again.statusCode, 204);
  });
  it('a read-only setting refuses writes with 409 and works again after setReadOnly(false)', async () => {
    const c = client();
    await c.setConfigurationSetting({ key: 'k', value: '1' });
    const ro = await c.setReadOnly({ key: 'k' }, true);
    assert.equal(ro.isReadOnly, true);
    const e = await rejection(() => c.setConfigurationSetting({ key: 'k', value: '2' }));
    assert.equal(e.statusCode, 409);
    const e2 = await rejection(() => c.deleteConfigurationSetting({ key: 'k' }));
    assert.equal(e2.statusCode, 409);
    await c.setReadOnly({ key: 'k' }, false);
    await c.setConfigurationSetting({ key: 'k', value: '2' });
  });
  it('a value of 10 KB and unicode round trips', async () => {
    const c = client();
    const v = '日本語-'.repeat(1500);
    await c.setConfigurationSetting({ key: 'big', value: v });
    assert.equal((await c.getConfigurationSetting({ key: 'big' })).value, v);
  });
});

describe('listing', () => {
  async function seeded() {
    const c = client();
    for (let i = 0; i < 12; i++) await c.setConfigurationSetting({ key: `app/${String(i).padStart(2, '0')}`, value: String(i), label: i % 2 ? 'odd' : 'even', tags: { n: String(i % 3) } });
    await c.setConfigurationSetting({ key: 'other', value: 'o' });
    return c;
  }
  it('keyFilter with a prefix wildcard and labelFilter select the right settings', async () => {
    const c = await seeded();
    const a = await collect(c.listConfigurationSettings({ keyFilter: 'app/*', labelFilter: 'odd' }));
    assert.deepEqual(a.map((s) => s.value), ['1', '3', '5', '7', '9', '11']);
    const b = await collect(c.listConfigurationSettings({ keyFilter: 'other' }));
    assert.equal(b.length, 1);
  });
  it('tagsFilter selects by tag', async () => {
    const c = await seeded();
    const a = await collect(c.listConfigurationSettings({ tagsFilter: ['n=0'] }));
    assert.deepEqual(a.map((s) => s.value), ['0', '3', '6', '9']);
  });
  it('fields limits the returned properties', async () => {
    const c = await seeded();
    const a = await collect(c.listConfigurationSettings({ keyFilter: 'other', fields: ['key', 'value'] }));
    assert.equal(a[0].key, 'other');
    assert.equal(a[0].value, 'o');
    assert.equal(a[0].etag, undefined);
  });
  it('byPage returns pages of at most the requested size and continuation covers every setting', async () => {
    const c = client();
    for (let i = 0; i < 250; i++) await c.setConfigurationSetting({ key: `k${String(i).padStart(3, '0')}`, value: String(i) });
    const sizes = [];
    const keys = [];
    for await (const p of c.listConfigurationSettings().byPage()) {
      sizes.push(p.items.length);
      keys.push(...p.items.map((x) => x.key));
    }
    assert.ok(sizes.length >= 3, `pages: ${sizes}`);
    assert.ok(sizes.every((n) => n <= 100));
    assert.equal(keys.length, 250);
    assert.equal(new Set(keys).size, 250);
  });
  it('listLabels and listConfigurationSettings with labelFilter "\\0" (null label)', async () => {
    const c = await seeded();
    // the null label is listed as an item with a null name (recorded from Azure)
    const all = (await collect(c.listLabels())).map((l) => l.name ?? null);
    assert.ok(all.includes(null));
    assert.deepEqual(all.filter((x) => x !== null).sort(), ['even', 'odd']);
    assert.deepEqual((await collect(c.listLabels({ nameFilter: 'ev*' }))).map((l) => l.name), ['even']);
    const nullLabel = await collect(c.listConfigurationSettings({ labelFilter: '\0' }));
    assert.deepEqual(nullLabel.map((s) => s.key), ['other']);
  });
  it('listRevisions returns every version of a key, newest first, and after delete the revision stays', async () => {
    const c = client();
    for (let i = 0; i < 3; i++) await c.setConfigurationSetting({ key: 'rev', value: `v${i}` });
    await c.deleteConfigurationSetting({ key: 'rev' });
    const revs = await collect(c.listRevisions({ keyFilter: 'rev' }));
    assert.deepEqual(revs.map((r) => r.value), ['v2', 'v1', 'v0']);
  });
  it('acceptDateTime returns the setting as it was at that time', async () => {
    const c = client();
    await c.setConfigurationSetting({ key: 'k', value: 'old' });
    await new Promise((r) => setTimeout(r, 1100));
    const mid = new Date();
    await new Promise((r) => setTimeout(r, 1100));
    await c.setConfigurationSetting({ key: 'k', value: 'new' });
    const old = await c.getConfigurationSetting({ key: 'k' }, { acceptDateTime: mid });
    assert.equal(old.value, 'old');
  });
});

describe('feature flags and secret references', () => {
  it('a feature flag is stored under .appconfig.featureflag/ with the feature flag content type and parses back', async () => {
    const c = client();
    const flag = { key: '.appconfig.featureflag/beta', isEnabled: true, value: JSON.stringify({ id: 'beta', enabled: true, description: 'd', conditions: { client_filters: [{ name: 'Microsoft.Percentage', parameters: { Value: 50 } }] } }), contentType: featureFlagContentType };
    await c.setConfigurationSetting(flag);
    const g = await c.getConfigurationSetting({ key: '.appconfig.featureflag/beta' });
    assert.equal(g.contentType, featureFlagContentType);
    assert.equal(isFeatureFlag(g), true);
    const f = parseFeatureFlag(g);
    assert.equal(f.value.enabled, true);
    assert.equal(f.value.conditions.clientFilters[0].name, 'Microsoft.Percentage');
    assert.equal(f.value.description, 'd');
  });
  it('a Key Vault reference round trips and parses', async () => {
    const c = client();
    await c.setConfigurationSetting({ key: 'secret', value: JSON.stringify({ uri: 'https://v.vault.azure.net/secrets/s' }), contentType: secretReferenceContentType });
    const g = await c.getConfigurationSetting({ key: 'secret' });
    assert.equal(isSecretReference(g), true);
    assert.equal(parseSecretReference(g).value.secretId, 'https://v.vault.azure.net/secrets/s');
  });
  it('listConfigurationSettings with keyFilter on the feature flag prefix finds flags', async () => {
    const c = client();
    await c.setConfigurationSetting({ key: '.appconfig.featureflag/a', value: JSON.stringify({ id: 'a', enabled: false }), contentType: featureFlagContentType });
    await c.setConfigurationSetting({ key: 'plain', value: 'x' });
    const r = await collect(c.listConfigurationSettings({ keyFilter: '.appconfig.featureflag/*' }));
    assert.equal(r.length, 1);
  });
});

describe('snapshots', () => {
  it('beginCreateSnapshot copies the settings at that time and the snapshot can be listed', async () => {
    const c = client();
    await c.setConfigurationSetting({ key: 'a', value: '1' });
    await c.setConfigurationSetting({ key: 'b', value: '2' });
    const name = uniq('snap');
    const poller = await c.beginCreateSnapshot({ name, retentionPeriodInSeconds: 3600, filters: [{ keyFilter: '*' }], compositionType: 'key' });
    const snap = await poller.pollUntilDone();
    assert.equal(snap.name, name);
    assert.equal(snap.status, 'ready');
    await c.setConfigurationSetting({ key: 'a', value: 'changed' });
    const items = await collect(c.listConfigurationSettingsForSnapshot(name));
    assert.deepEqual(items.map((s) => [s.key, s.value]), [['a', '1'], ['b', '2']]);
    assert.equal((await c.getSnapshot(name)).itemCount, 2);
    assert.ok((await collect(c.listSnapshots())).some((s) => s.name === name));
  });
  it('archiveSnapshot and recoverSnapshot change the status', async () => {
    const c = client();
    await c.setConfigurationSetting({ key: 'a', value: '1' });
    const name = uniq('snap');
    await (await c.beginCreateSnapshot({ name, filters: [{ keyFilter: '*' }] })).pollUntilDone();
    assert.equal((await c.archiveSnapshot(name)).status, 'archived');
    assert.equal((await c.recoverSnapshot(name)).status, 'ready');
  });
  it('a snapshot that does not exist is 404', async () => {
    const c = client();
    const e = await rejection(() => c.getSnapshot('nope'));
    assert.equal(e.statusCode, 404);
  });
});

describe('auth', () => {
  it('a bearer token (any token) over https works', async () => {
    const c = new AppConfigurationClient(emu.url('azconfig', uniq('tok'), 'https'), fakeCredential);
    await c.setConfigurationSetting({ key: 'k', value: 'tok' });
    assert.equal((await c.getConfigurationSetting({ key: 'k' })).value, 'tok');
  });
  it('the connection string over https works too (HMAC)', async () => {
    const c = new AppConfigurationClient(`Endpoint=${emu.url('azconfig', uniq('h'), 'https')};Id=any;Secret=YXplbXU=`);
    await c.setConfigurationSetting({ key: 'k', value: 'h' });
    assert.equal((await c.getConfigurationSetting({ key: 'k' })).value, 'h');
  });
});

describe('plain http', () => {
  it('the SDK refuses http unless allowInsecureConnection is set (SDK rule), and then the connection string works', async () => {
    const cs = `Endpoint=${emu.url('azconfig', uniq('p'))};Id=any;Secret=YXplbXU=`;
    const e = await rejection(() => new AppConfigurationClient(cs).setConfigurationSetting({ key: 'k', value: '1' }));
    assert.match(e.message, /allowInsecureConnection/);
    const c = new AppConfigurationClient(cs, { allowInsecureConnection: true });
    await c.setConfigurationSetting({ key: 'k', value: '1' });
    assert.equal((await c.getConfigurationSetting({ key: 'k' })).value, '1');
  });
});
