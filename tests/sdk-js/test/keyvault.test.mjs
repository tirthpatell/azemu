import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { createHash, createPublicKey, verify as nodeVerify, constants, publicEncrypt, randomBytes } from 'node:crypto';
import { SecretClient } from '@azure/keyvault-secrets';
import { KeyClient, CryptographyClient } from '@azure/keyvault-keys';
import { CertificateClient, DefaultCertificatePolicy } from '@azure/keyvault-certificates';
import { env, uniq, fakeCredential, collect, rejection } from '../lib/emu.mjs';

const emu = env();
// No client switch: the challenge names the domain of the request (vault.localhost:<port>), which the SDK's own check accepts.
const opts = {};
const vaultUrl = () => emu.url('vault', uniq('v'), 'https');
const code = (e) => e.code ?? e.details?.error?.code;

describe('secrets', () => {
  it('setSecret then getSecret returns the value, a version id and content type', async () => {
    const sc = new SecretClient(vaultUrl(), fakeCredential, opts);
    const s = await sc.setSecret('db-pass', 'hunter2', { contentType: 'text/plain', tags: { env: 'dev' } });
    assert.equal(s.value, 'hunter2');
    assert.ok(s.properties.version?.length === 32);
    const g = await sc.getSecret('db-pass');
    assert.equal(g.value, 'hunter2');
    assert.equal(g.properties.contentType, 'text/plain');
    assert.equal(g.properties.tags.env, 'dev');
    assert.equal(g.properties.enabled, true);
    assert.equal(g.name, 'db-pass');
    assert.ok(g.properties.vaultUrl.includes('.vault.localhost'));
  });
  it('getSecret of a missing secret is SecretNotFound 404', async () => {
    const sc = new SecretClient(vaultUrl(), fakeCredential, opts);
    const e = await rejection(() => sc.getSecret('nope'));
    assert.equal(e.statusCode, 404);
    assert.equal(code(e), 'SecretNotFound');
  });
  it('each setSecret makes a new version and getSecret can read an older one', async () => {
    const sc = new SecretClient(vaultUrl(), fakeCredential, opts);
    const v1 = await sc.setSecret('n', 'one');
    const v2 = await sc.setSecret('n', 'two');
    assert.notEqual(v1.properties.version, v2.properties.version);
    assert.equal((await sc.getSecret('n')).value, 'two');
    assert.equal((await sc.getSecret('n', { version: v1.properties.version })).value, 'one');
    const versions = await collect(sc.listPropertiesOfSecretVersions('n'));
    assert.equal(versions.length, 2);
  });
  it('updateSecretProperties changes tags and disables the secret (get then is 403 SecretDisabled)', async () => {
    const sc = new SecretClient(vaultUrl(), fakeCredential, opts);
    await sc.setSecret('u', 'x');
    const u = await sc.updateSecretProperties('u', '', { tags: { a: 'b' }, enabled: false });
    assert.equal(u.tags.a, 'b');
    assert.equal(u.enabled, false);
    const e = await rejection(() => sc.getSecret('u'));
    assert.equal(e.statusCode, 403);
  });
  it('listPropertiesOfSecrets pages through more than one page (25 per page) without loss', async () => {
    const sc = new SecretClient(vaultUrl(), fakeCredential, opts);
    for (let i = 0; i < 30; i++) await sc.setSecret(`s${String(i).padStart(2, '0')}`, 'v');
    const pages = [];
    for await (const p of sc.listPropertiesOfSecrets().byPage()) pages.push(p.length);
    assert.ok(pages.length >= 2);
    assert.equal(pages.reduce((a, b) => a + b, 0), 30);
    const names = (await collect(sc.listPropertiesOfSecrets())).map((x) => x.name);
    assert.equal(new Set(names).size, 30);
  });
  it('beginDeleteSecret finishes with a recovery id, getDeletedSecret works, recover brings it back', async () => {
    const sc = new SecretClient(vaultUrl(), fakeCredential, opts);
    await sc.setSecret('d', 'x');
    const del = await (await sc.beginDeleteSecret('d')).pollUntilDone();
    assert.ok(del.recoveryId);
    assert.ok(del.deletedOn instanceof Date);
    assert.ok((await sc.getDeletedSecret('d')).scheduledPurgeDate);
    const e = await rejection(() => sc.getSecret('d'));
    assert.equal(e.statusCode, 404);
    await (await sc.beginRecoverDeletedSecret('d')).pollUntilDone();
    assert.equal((await sc.getSecret('d')).value, 'x');
  });
  it('purgeDeletedSecret removes a deleted secret for good and the name can be reused', async () => {
    const sc = new SecretClient(vaultUrl(), fakeCredential, opts);
    await sc.setSecret('p', 'x');
    await (await sc.beginDeleteSecret('p')).pollUntilDone();
    await sc.purgeDeletedSecret('p');
    const e = await rejection(() => sc.getDeletedSecret('p'));
    assert.equal(e.statusCode, 404);
    assert.equal((await sc.setSecret('p', 'again')).value, 'again');
  });
  it('listDeletedSecrets shows the deleted ones only', async () => {
    const sc = new SecretClient(vaultUrl(), fakeCredential, opts);
    await sc.setSecret('a', 'x');
    await sc.setSecret('b', 'x');
    await (await sc.beginDeleteSecret('a')).pollUntilDone();
    assert.deepEqual((await collect(sc.listDeletedSecrets())).map((x) => x.name), ['a']);
    assert.deepEqual((await collect(sc.listPropertiesOfSecrets())).map((x) => x.name), ['b']);
  });
  it('backup and restore bring a secret with its versions into another vault', async () => {
    const a = new SecretClient(vaultUrl(), fakeCredential, opts);
    const b = new SecretClient(vaultUrl(), fakeCredential, opts);
    await a.setSecret('bk', 'one');
    await a.setSecret('bk', 'two');
    const blob = await a.backupSecret('bk');
    assert.ok(blob.length > 100);
    await b.restoreSecretBackup(blob);
    assert.equal((await b.getSecret('bk')).value, 'two');
    assert.equal((await collect(b.listPropertiesOfSecretVersions('bk'))).length, 2);
  });
  it('a name with characters outside 0-9a-zA-Z- is refused with BadParameter 400', async () => {
    const sc = new SecretClient(vaultUrl(), fakeCredential, opts);
    const e = await rejection(() => sc.setSecret('bad_name', 'x'));
    assert.equal(e.statusCode, 400);
  });
  it('the SDK challenge handshake works for the first call (401 then retry with the bearer token)', async () => {
    let calls = 0;
    const cred = { async getToken(...a) { calls++; return fakeCredential.getToken(...a); } };
    const sc = new SecretClient(vaultUrl(), cred, opts);
    await sc.setSecret('c', 'x');
    assert.ok(calls >= 1);
  });
  it('the SDK checks the challenge resource against the vault host and accepts it (no switch needed)', async () => {
    const sc = new SecretClient(vaultUrl(), fakeCredential, { disableChallengeResourceVerification: false });
    await sc.setSecret('c', 'x');
    assert.equal((await sc.getSecret('c')).value, 'x');
    const kc = new KeyClient(vaultUrl(), fakeCredential);
    assert.equal((await kc.createEcKey('e')).name, 'e');
    const cc = new CertificateClient(vaultUrl(), fakeCredential);
    assert.deepEqual(await collect(cc.listPropertiesOfCertificates()), []);
  });
  it('the old switch disableChallengeResourceVerification: true still works', async () => {
    const sc = new SecretClient(vaultUrl(), fakeCredential, { disableChallengeResourceVerification: true });
    await sc.setSecret('c', 'x');
    assert.equal((await sc.getSecret('c')).value, 'x');
  });
});

describe('keys', () => {
  it('createRsaKey returns a public JWK with n and e and key ops', async () => {
    const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
    const k = await kc.createRsaKey('r', { keySize: 2048 });
    assert.equal(k.keyType, 'RSA');
    assert.ok(k.key.n && k.key.e);
    assert.equal(k.key.n.length, 256);
    assert.ok(!k.key.d, 'private part must not be returned');
    assert.ok(k.key.keyOps.includes('sign'));
  });
  it('createEcKey with P-256 returns x and y coordinates of 32 bytes', async () => {
    const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
    const k = await kc.createEcKey('e', { curve: 'P-256' });
    assert.equal(k.keyType, 'EC');
    assert.equal(k.key.crv, 'P-256');
    assert.equal(k.key.x.length, 32);
    assert.equal(k.key.y.length, 32);
  });
  it('getKey, listPropertiesOfKeys, versions, update, delete and recover', async () => {
    const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
    const k1 = await kc.createKey('lk', 'RSA');
    const k2 = await kc.createKey('lk', 'RSA');
    assert.notEqual(k1.properties.version, k2.properties.version);
    assert.equal((await kc.getKey('lk')).properties.version, k2.properties.version);
    assert.equal((await collect(kc.listPropertiesOfKeyVersions('lk'))).length, 2);
    const u = await kc.updateKeyProperties('lk', k2.properties.version, { tags: { t: '1' }, enabled: true });
    assert.equal(u.properties.tags.t, '1');
    await (await kc.beginDeleteKey('lk')).pollUntilDone();
    assert.equal((await collect(kc.listPropertiesOfKeys())).length, 0);
    await (await kc.beginRecoverDeletedKey('lk')).pollUntilDone();
    assert.equal((await collect(kc.listPropertiesOfKeys())).length, 1);
  });
  it('importKey with an RSA JWK keeps the key material usable', async () => {
    const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
    const { generateKeyPairSync } = await import('node:crypto');
    const { privateKey } = generateKeyPairSync('rsa', { modulusLength: 2048 });
    const jwk = privateKey.export({ format: 'jwk' });
    const imp = await kc.importKey('imp', { kty: 'RSA', n: Buffer.from(jwk.n, 'base64url'), e: Buffer.from(jwk.e, 'base64url'), d: Buffer.from(jwk.d, 'base64url'), p: Buffer.from(jwk.p, 'base64url'), q: Buffer.from(jwk.q, 'base64url'), dp: Buffer.from(jwk.dp, 'base64url'), dq: Buffer.from(jwk.dq, 'base64url'), qi: Buffer.from(jwk.qi, 'base64url') });
    assert.equal(Buffer.from(imp.key.n).toString('base64url'), jwk.n);
    const cc = new CryptographyClient(imp.id, fakeCredential, opts);
    const digest = createHash('sha256').update('hi').digest();
    const sig = await cc.sign('RS256', digest);
    assert.ok(nodeVerify('sha256', Buffer.from('hi'), createPublicKey({ key: privateKey }), Buffer.from(sig.result)));
  });
  it('backupKey then restoreKeyBackup in another vault keeps the key id version and material', async () => {
    const a = new KeyClient(vaultUrl(), fakeCredential, opts);
    const b = new KeyClient(vaultUrl(), fakeCredential, opts);
    const k = await a.createRsaKey('bk');
    const back = await a.backupKey('bk');
    const r = await b.restoreKeyBackup(back);
    assert.equal(r.properties.version, k.properties.version);
    assert.equal(Buffer.from(r.key.n).toString('hex'), Buffer.from(k.key.n).toString('hex'));
  });
  it('rotateKey makes a new version', async () => {
    const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
    const k = await kc.createRsaKey('rot');
    const r = await kc.rotateKey('rot');
    assert.notEqual(r.properties.version, k.properties.version);
  });
  it('getRandomBytes returns the number of bytes asked for', async () => {
    const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
    const r = await kc.getRandomBytes(32);
    assert.equal(r.length, 32);
  });
  it('getKey on a missing key is KeyNotFound 404', async () => {
    const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
    const e = await rejection(() => kc.getKey('nope'));
    assert.equal(e.statusCode, 404);
    assert.equal(code(e), 'KeyNotFound');
  });
});

describe('cryptography with remote keys (client built from the key id)', () => {
  for (const alg of ['RS256', 'RS384', 'RS512', 'PS256', 'PS384', 'PS512']) {
    it(`RSA sign ${alg} makes a signature that Node verifies with the downloaded public key, and verify() says so`, async () => {
      const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
      const k = await kc.createRsaKey(`k-${alg.toLowerCase()}`);
      const cc = new CryptographyClient(k.id, fakeCredential, opts);
      const hash = { 256: 'sha256', 384: 'sha384', 512: 'sha512' }[alg.slice(2)];
      const data = Buffer.from('payload');
      const sig = await cc.signData(alg, data);
      assert.equal(sig.algorithm, alg);
      const pub = createPublicKey({ key: { kty: 'RSA', n: Buffer.from(k.key.n).toString('base64url'), e: Buffer.from(k.key.e).toString('base64url') }, format: 'jwk' });
      const padding = alg.startsWith('PS') ? { padding: constants.RSA_PKCS1_PSS_PADDING, saltLength: constants.RSA_PSS_SALTLEN_DIGEST } : {};
      assert.ok(nodeVerify(hash, data, { key: pub, ...padding }, Buffer.from(sig.result)));
      // verify() with a digest always goes to the vault; verifyData() is done by the SDK itself from the public key
      // (and the SDK's local code checks PS256/384/512 with PKCS#1 v1.5 padding, so it cannot be used for them).
      const digest = createHash(hash).update(data).digest();
      assert.equal((await cc.verify(alg, digest, sig.result)).result, true);
      assert.equal((await cc.verify(alg, createHash(hash).update('other').digest(), sig.result)).result, false);
    });
  }
  for (const [crv, alg, hash] of [['P-256', 'ES256', 'sha256'], ['P-384', 'ES384', 'sha384'], ['P-521', 'ES512', 'sha512'], ['P-256K', 'ES256K', 'sha256']]) {
    it(`EC ${crv} ${alg} signature (raw r||s) verifies in Node`, async () => {
      const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
      const k = await kc.createEcKey(`ec-${alg.toLowerCase()}`, { curve: crv });
      const cc = new CryptographyClient(k.id, fakeCredential, opts);
      const data = Buffer.from('payload');
      const sig = await cc.signData(alg, data);
      if (crv !== 'P-256K') {
        const pub = createPublicKey({ key: { kty: 'EC', crv, x: Buffer.from(k.key.x).toString('base64url'), y: Buffer.from(k.key.y).toString('base64url') }, format: 'jwk' });
        assert.ok(nodeVerify(hash, data, { key: pub, dsaEncoding: 'ieee-p1363' }, Buffer.from(sig.result)));
      }
      assert.equal((await cc.verifyData(alg, data, sig.result)).result, true);
    });
  }
  for (const alg of ['RSA1_5', 'RSA-OAEP', 'RSA-OAEP-256']) {
    it(`encrypt then decrypt with ${alg} gives the plaintext back`, async () => {
      const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
      const k = await kc.createRsaKey(`enc-${alg.toLowerCase().replace(/_/g, '')}`);
      const cc = new CryptographyClient(k.id, fakeCredential, opts);
      const pt = Buffer.from('secret message');
      const ct = await cc.encrypt({ algorithm: alg, plaintext: pt });
      assert.equal(ct.result.length, 256);
      assert.ok(Buffer.from((await cc.decrypt({ algorithm: alg, ciphertext: ct.result })).result).equals(pt));
    });
    it(`wrapKey then unwrapKey with ${alg} round trips a 32 byte key`, async () => {
      const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
      const k = await kc.createRsaKey(`wrap-${alg.toLowerCase().replace(/_/g, '')}`);
      const cc = new CryptographyClient(k.id, fakeCredential, opts);
      const cek = randomBytes(32);
      const w = await cc.wrapKey(alg, cek);
      assert.ok(Buffer.from((await cc.unwrapKey(alg, w.result)).result).equals(cek));
    });
  }
  it('decrypting garbage is a 400 error', async () => {
    const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
    const k = await kc.createRsaKey('garb');
    const cc = new CryptographyClient(k.id, fakeCredential, opts);
    const e = await rejection(() => cc.decrypt({ algorithm: 'RSA-OAEP', ciphertext: randomBytes(256) }));
    assert.equal(e.statusCode, 400);
  });
  it('a key without the sign operation: the vault answers 403 Forbidden to a sign request (raw HTTPS; the SDK checks key_ops itself first)', async () => {
    const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
    const k = await kc.createRsaKey('noops', { keyOps: ['encrypt', 'decrypt'] });
    const cc = new CryptographyClient(k.id, fakeCredential, opts);
    const e = await rejection(() => cc.sign('RS256', createHash('sha256').update('x').digest()));
    assert.match(e.message, /not supported/);
    const r = await fetch(`${k.id}/sign?api-version=7.5`, { method: 'POST', headers: { Authorization: 'Bearer x', 'content-type': 'application/json' }, body: JSON.stringify({ alg: 'RS256', value: createHash('sha256').update('x').digest().toString('base64url') }) });
    assert.equal(r.status, 403);
    assert.equal((await r.json()).error.code, 'Forbidden');
  });
});

describe('cryptography run locally from the public key', () => {
  it('a CryptographyClient built from a JsonWebKey encrypts locally and the vault decrypts', async () => {
    const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
    const k = await kc.createRsaKey('loc');
    const local = new CryptographyClient(k.key);
    const ct = await local.encrypt({ algorithm: 'RSA-OAEP', plaintext: Buffer.from('local') });
    const remote = new CryptographyClient(k.id, fakeCredential, opts);
    assert.equal(Buffer.from((await remote.decrypt({ algorithm: 'RSA-OAEP', ciphertext: ct.result })).result).toString(), 'local');
  });
  it('a vault RS256 signature verifies locally from the public JWK', async () => {
    const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
    const k = await kc.createRsaKey('locv');
    const remote = new CryptographyClient(k.id, fakeCredential, opts);
    const sig = await remote.signData('RS256', Buffer.from('m'));
    const local = new CryptographyClient(k.key);
    assert.equal((await local.verifyData('RS256', Buffer.from('m'), sig.result)).result, true);
    assert.equal((await local.verifyData('RS256', Buffer.from('x'), sig.result)).result, false);
  });
  it('a CryptographyClient built from the KeyVaultKey object works end to end', async () => {
    const kc = new KeyClient(vaultUrl(), fakeCredential, opts);
    const k = await kc.createRsaKey('obj');
    const cc = new CryptographyClient(k, fakeCredential, opts);
    const ct = await cc.encrypt({ algorithm: 'RSA-OAEP', plaintext: Buffer.from('x') });
    assert.equal(Buffer.from((await cc.decrypt({ algorithm: 'RSA-OAEP', ciphertext: ct.result })).result).toString(), 'x');
  });
});

describe('certificates', () => {
  it('beginCreateCertificate with the default self-signed policy finishes and has a cer and a thumbprint', async () => {
    const cc = new CertificateClient(vaultUrl(), fakeCredential, opts);
    const policy = { ...DefaultCertificatePolicy, subject: 'CN=azemu-test' };
    const poller = await cc.beginCreateCertificate('c1', policy);
    const cert = await poller.pollUntilDone();
    assert.equal(cert.name, 'c1');
    assert.ok(cert.cer.length > 200);
    assert.ok(cert.properties.x509Thumbprint.length === 20);
    assert.equal(cert.policy.subject, 'CN=azemu-test');
    assert.equal(cert.policy.issuerName, 'Self');
    const x = new (await import('node:crypto')).X509Certificate(Buffer.from(cert.cer));
    assert.equal(x.subject, 'CN=azemu-test');
    assert.equal(x.issuer, 'CN=azemu-test');
  });
  it('getCertificate, getCertificateVersion, list and versions agree', async () => {
    const cc = new CertificateClient(vaultUrl(), fakeCredential, opts);
    const policy = { ...DefaultCertificatePolicy, subject: 'CN=azemu-list' };
    const c = await (await cc.beginCreateCertificate('c2', policy)).pollUntilDone();
    const got = await cc.getCertificate('c2');
    assert.equal(got.properties.version, c.properties.version);
    assert.equal((await cc.getCertificateVersion('c2', c.properties.version)).name, 'c2');
    assert.deepEqual((await collect(cc.listPropertiesOfCertificates())).map((x) => x.name), ['c2']);
    assert.equal((await collect(cc.listPropertiesOfCertificateVersions('c2'))).length, 1);
  });
  it('the certificate has a key and a secret of the same name that hold the private key (PKCS12 by default)', async () => {
    const v = vaultUrl();
    const cc = new CertificateClient(v, fakeCredential, opts);
    await (await cc.beginCreateCertificate('c3', { ...DefaultCertificatePolicy, subject: 'CN=azemu-sec' })).pollUntilDone();
    const sc = new SecretClient(v, fakeCredential, opts);
    const s = await sc.getSecret('c3');
    assert.equal(s.properties.contentType, 'application/x-pkcs12');
    assert.ok(Buffer.from(s.value, 'base64').length > 500);
    const kc = new KeyClient(v, fakeCredential, opts);
    assert.equal((await kc.getKey('c3')).keyType, 'RSA');
  });
  async function pemImport(value) {
    const v = vaultUrl();
    const cc = new CertificateClient(v, fakeCredential, opts);
    await (await cc.beginCreateCertificate('src', { ...DefaultCertificatePolicy, subject: 'CN=azemu-pem', contentType: 'application/x-pem-file' })).pollUntilDone();
    const pem = (await new SecretClient(v, fakeCredential, opts).getSecret('src')).value;
    assert.ok(pem.includes('BEGIN CERTIFICATE'));
    const r = await fetch(`${v}/certificates/imported/import?api-version=7.5`, {
      method: 'POST', headers: { Authorization: 'Bearer x', 'content-type': 'application/json' },
      body: JSON.stringify({ value: value(pem), policy: { secret_props: { contentType: 'application/x-pem-file' } } }),
    });
    return { r, cc };
  }
  it('importCertificate of a PEM bundle sent as base64 of the PEM text (what the Python SDK sends) shows the same subject (raw HTTPS)', async () => {
    const { r, cc } = await pemImport((pem) => Buffer.from(pem).toString('base64'));
    assert.equal(r.status, 200, await r.clone().text());
    const imp = await cc.getCertificate('imported');
    assert.equal(new (await import('node:crypto')).X509Certificate(Buffer.from(imp.cer)).subject, 'CN=azemu-pem');
  });
  it('importCertificate of a PEM bundle sent as plain PEM text in value (what the JavaScript SDK sends for a PEM content type) (raw HTTPS)', async () => {
    const { r, cc } = await pemImport((pem) => pem);
    assert.equal(r.status, 200, await r.clone().text());
    const imp = await cc.getCertificate('imported');
    assert.equal(new (await import('node:crypto')).X509Certificate(Buffer.from(imp.cer)).subject, 'CN=azemu-pem');
  });
  it('importCertificate of a PFX (from another certificate) through the SDK keeps the subject', async () => {
    const v = vaultUrl();
    const cc = new CertificateClient(v, fakeCredential, opts);
    await (await cc.beginCreateCertificate('src', { ...DefaultCertificatePolicy, subject: 'CN=azemu-pfx' })).pollUntilDone();
    const pfx = Buffer.from((await new SecretClient(v, fakeCredential, opts).getSecret('src')).value, 'base64');
    const imp = await cc.importCertificate('imported', pfx);
    assert.equal(new (await import('node:crypto')).X509Certificate(Buffer.from(imp.cer)).subject, 'CN=azemu-pfx');
  });
  // @azure/keyvault-certificates 4.11.0 turns options.policy.contentType into plain PEM text in `value` but does not send `policy` itself,
  // so the emulator has to tell from the value that it is PEM.
  it('importCertificate of PEM text through the SDK (value is the PEM text, no policy in the request)', async () => {
    const v = vaultUrl();
    const cc = new CertificateClient(v, fakeCredential, opts);
    await (await cc.beginCreateCertificate('src', { ...DefaultCertificatePolicy, subject: 'CN=azemu-sdkpem', contentType: 'application/x-pem-file' })).pollUntilDone();
    const pem = (await new SecretClient(v, fakeCredential, opts).getSecret('src')).value;
    const imp = await cc.importCertificate('imported', Buffer.from(pem), { policy: { contentType: 'application/x-pem-file' } });
    assert.equal(new (await import('node:crypto')).X509Certificate(Buffer.from(imp.cer)).subject, 'CN=azemu-sdkpem');
    assert.equal(imp.policy.contentType, 'application/x-pem-file');
    assert.ok((await new SecretClient(v, fakeCredential, opts).getSecret('imported')).value.includes('BEGIN'));
  });
  it('delete, getDeletedCertificate, recover and purge', async () => {
    const cc = new CertificateClient(vaultUrl(), fakeCredential, opts);
    await (await cc.beginCreateCertificate('c4', { ...DefaultCertificatePolicy, subject: 'CN=azemu-del' })).pollUntilDone();
    const d = await (await cc.beginDeleteCertificate('c4')).pollUntilDone();
    assert.ok(d.recoveryId);
    assert.equal((await cc.getDeletedCertificate('c4')).name, 'c4');
    await (await cc.beginRecoverDeletedCertificate('c4')).pollUntilDone();
    await (await cc.beginDeleteCertificate('c4')).pollUntilDone();
    await cc.purgeDeletedCertificate('c4');
    const e = await rejection(() => cc.getDeletedCertificate('c4'));
    assert.equal(e.statusCode, 404);
  });
  it('an unknown-issuer request stays pending: its operation says inProgress and carries a CSR', async () => {
    const v = vaultUrl();
    const cc = new CertificateClient(v, fakeCredential, opts);
    await cc.beginCreateCertificate('pend', { issuerName: 'Unknown', subject: 'CN=azemu-pend', keyType: 'RSA', keySize: 2048 });
    const r = await fetch(`${v}/certificates/pend/pending?api-version=7.5`, { headers: { Authorization: 'Bearer x' } });
    const body = await r.json();
    assert.equal(body.status, 'inProgress');
    assert.ok(body.csr.length > 100);
    await cc.deleteCertificateOperation('pend');
  });
  it('getCertificateOperation of a pending certificate returns the operation (the SDK first reads the certificate, which Azure answers with a bundle without cer)', async () => {
    const cc = new CertificateClient(vaultUrl(), fakeCredential, opts);
    await cc.beginCreateCertificate('pend2', { issuerName: 'Unknown', subject: 'CN=azemu-pend', keyType: 'RSA', keySize: 2048 });
    const poller = await cc.getCertificateOperation('pend2');
    assert.equal(poller.getOperationState().isCompleted, undefined);
  });
  it('getCertificate of a missing certificate is CertificateNotFound 404', async () => {
    const cc = new CertificateClient(vaultUrl(), fakeCredential, opts);
    const e = await rejection(() => cc.getCertificate('nope'));
    assert.equal(e.statusCode, 404);
  });
  it('issuers and contacts can be set and read', async () => {
    const cc = new CertificateClient(vaultUrl(), fakeCredential, opts);
    await cc.createIssuer('iss', 'Test', { accountId: 'acct' });
    const iss = await cc.getIssuer('iss');
    assert.equal(iss.provider, 'Test');
    assert.equal(iss.accountId, 'acct');
    // the SDK derives `name` from the id as the path segment after /certificates/, which is "issuers" for issuer ids (SDK quirk)
    assert.ok(iss.id.endsWith('/certificates/issuers/iss'));
    assert.equal((await collect(cc.listPropertiesOfIssuers())).length, 1);
    await cc.setContacts([{ email: 'a@example.com', name: 'A' }]);
    assert.equal((await cc.getContacts())[0].email, 'a@example.com');
  });
});
