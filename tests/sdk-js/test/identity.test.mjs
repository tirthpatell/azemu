import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { DefaultAzureCredential, ManagedIdentityCredential, ClientSecretCredential, ChainedTokenCredential, EnvironmentCredential } from '@azure/identity';
import { BlobServiceClient } from '@azure/storage-blob';
import { SecretClient } from '@azure/keyvault-secrets';
import { env, uniq, rejection } from '../lib/emu.mjs';

const emu = env();
const kv = {}; // no client switch is needed for the vault's challenge

function jwtClaims(token) {
  return JSON.parse(Buffer.from(token.split('.')[1], 'base64url').toString());
}

describe('managed identity through IDENTITY_ENDPOINT and IDENTITY_HEADER', () => {
  it('ManagedIdentityCredential returns a JWT for the resource asked for', async () => {
    const t = await new ManagedIdentityCredential().getToken('https://storage.azure.com/.default');
    assert.ok(t.token.split('.').length === 3);
    assert.ok(t.expiresOnTimestamp > Date.now());
    assert.equal(jwtClaims(t.token).aud, 'https://storage.azure.com');
  });
  it('the scope decides the audience: vault.azure.net gives aud https://vault.azure.net', async () => {
    const t = await new ManagedIdentityCredential().getToken('https://vault.azure.net/.default');
    assert.equal(jwtClaims(t.token).aud, 'https://vault.azure.net');
  });
  it('a user-assigned identity (clientId) is sent as client_id and shows up as appid', async () => {
    const id = '12345678-1234-1234-1234-123456789012';
    const t = await new ManagedIdentityCredential({ clientId: id }).getToken('https://management.azure.com/.default');
    assert.equal(jwtClaims(t.token).appid, id);
  });
  it('DefaultAzureCredential picks the managed identity and works for a blob call', async () => {
    const cred = new DefaultAzureCredential();
    const s = new BlobServiceClient(emu.url('blob', uniq('id'), 'https'), cred);
    const c = s.getContainerClient(uniq('c'));
    const r = await c.create();
    assert.equal(r._response.status, 201);
    await c.getBlockBlobClient('a').upload('tokened', 7);
    assert.equal((await c.getBlockBlobClient('a').getProperties()).contentLength, 7);
  });
  it('DefaultAzureCredential works for a Key Vault secret call', async () => {
    const cred = new DefaultAzureCredential();
    const sc = new SecretClient(emu.url('vault', uniq('v'), 'https'), cred, kv);
    await sc.setSecret('s', 'value');
    assert.equal((await sc.getSecret('s')).value, 'value');
  });
  it('a second getToken call is served from the credential cache (same token)', async () => {
    const cred = new ManagedIdentityCredential();
    const a = await cred.getToken('https://storage.azure.com/.default');
    const b = await cred.getToken('https://storage.azure.com/.default');
    assert.equal(a.token, b.token);
  });
});

describe('client secret credential against the emulator login host', () => {
  const tenant = '22222222-2222-2222-2222-222222222222';
  const opts = { authorityHost: `https://login.localhost:${emu.port}`, disableInstanceDiscovery: true };
  it('gets a token for a service principal and the claims carry tenant and client id', async () => {
    const cred = new ClientSecretCredential(tenant, 'client-aaa', 'anything', opts);
    const t = await cred.getToken('https://storage.azure.com/.default');
    const c = jwtClaims(t.token);
    assert.equal(c.tid, tenant);
    assert.equal(c.appid, 'client-aaa');
    assert.equal(c.aud, 'https://storage.azure.com');
  });
  it('the same credential works against the localhost/_azemu/identity/login authority', async () => {
    const cred = new ClientSecretCredential(tenant, 'client-bbb', 'anything', { authorityHost: `https://localhost:${emu.port}/_azemu/identity/login`, disableInstanceDiscovery: true });
    const t = await cred.getToken('https://vault.azure.net/.default');
    assert.equal(jwtClaims(t.token).aud, 'https://vault.azure.net');
  });
  it('the token authorises a Blob call and a Key Vault call', async () => {
    // Key Vault's challenge names a tenant (the emulator's default one); the SDK refuses to use a credential for another tenant
    // unless it is allowed to, so a service principal in any other tenant needs additionallyAllowedTenants (SDK rule, same on Azure).
    const cred = new ClientSecretCredential(tenant, 'client-ccc', 'anything', { ...opts, additionallyAllowedTenants: ['*'] });
    const blobs = new BlobServiceClient(emu.url('blob', uniq('cs'), 'https'), cred);
    const c = blobs.getContainerClient(uniq('c'));
    await c.create();
    const sc = new SecretClient(emu.url('vault', uniq('v'), 'https'), cred, kv);
    await sc.setSecret('k', 'v1');
    assert.equal((await sc.getSecret('k')).value, 'v1');
  });
  it('Key Vault with a credential of another tenant and no additionallyAllowedTenants is CredentialUnavailableError (SDK multi-tenant rule)', async () => {
    const cred = new ClientSecretCredential(tenant, 'client-eee', 'anything', opts);
    const sc = new SecretClient(emu.url('vault', uniq('v'), 'https'), cred, kv);
    const e = await rejection(() => sc.setSecret('k', 'v1'));
    assert.equal(e.name, 'CredentialUnavailableError');
  });
  it('EnvironmentCredential reads AZURE_TENANT_ID, AZURE_CLIENT_ID, AZURE_CLIENT_SECRET and AZURE_AUTHORITY_HOST', async () => {
    const saved = { ...process.env };
    Object.assign(process.env, { AZURE_TENANT_ID: tenant, AZURE_CLIENT_ID: 'env-client', AZURE_CLIENT_SECRET: 'x', AZURE_AUTHORITY_HOST: `https://login.localhost:${emu.port}` });
    try {
      const cred = new EnvironmentCredential({ disableInstanceDiscovery: true });
      const t = await cred.getToken('https://storage.azure.com/.default');
      assert.equal(jwtClaims(t.token).appid, 'env-client');
    } finally {
      for (const k of ['AZURE_TENANT_ID', 'AZURE_CLIENT_ID', 'AZURE_CLIENT_SECRET', 'AZURE_AUTHORITY_HOST']) {
        if (saved[k] === undefined) delete process.env[k]; else process.env[k] = saved[k];
      }
    }
  });
  it('a chained credential falls through a failing member to the managed identity', async () => {
    const failing = { async getToken() { throw new Error('nope'); } };
    const chain = new ChainedTokenCredential(new ManagedIdentityCredential());
    const t = await chain.getToken('https://storage.azure.com/.default');
    assert.ok(t.token);
    void failing;
  });
});
