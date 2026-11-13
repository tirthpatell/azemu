// Starts and stops an azemu process for the tests. Plain words, no magic.
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync, existsSync, createWriteStream, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname, resolve } from 'node:path';
import { createServer } from 'node:net';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
export const projectDir = resolve(here, '..');

export function binaryPath() {
  if (process.env.AZEMU_BIN) return process.env.AZEMU_BIN;
  const copy = join(projectDir, '.azemu-under-test');
  if (existsSync(copy)) return copy;
  const release = resolve(projectDir, '..', '..', 'target', 'release', 'azemu');
  const debug = resolve(projectDir, '..', '..', 'target', 'debug', 'azemu');
  return existsSync(debug) || !existsSync(release) ? debug : release;
}

export function freePort() {
  return new Promise((res, rej) => {
    const s = createServer();
    s.listen(0, '127.0.0.1', () => {
      const p = s.address().port;
      s.close(() => res(p));
    });
    s.on('error', rej);
  });
}

async function waitHealthy(port, proc, ms = 20000) {
  const end = Date.now() + ms;
  let last;
  while (Date.now() < end) {
    if (proc.exitCode !== null) throw new Error(`emulator exited early with code ${proc.exitCode}`);
    try {
      const r = await fetch(`http://127.0.0.1:${port}/_azemu/health`);
      if (r.status === 200) return;
    } catch (e) {
      last = e;
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`emulator not healthy on ${port}: ${last}`);
}

/**
 * Start an emulator on a free port.
 * opts: { args: [...extra cli args], env: {...}, certDir, logLevel, pins: {blob: port}, name }
 * Returns { port, gateway, certDir, caPath, logPath, stop(), reset(), url(label, account, scheme) }.
 */
export async function startEmulator(opts = {}) {
  const dir = mkdtempSync(join(opts.tmpBase ?? tmpdir(), 'azemu-js-'));
  const certDir = opts.certDir ?? join(dir, 'certs');
  const port = opts.port ?? (await freePort());
  const logPath = join(dir, 'azemu.log');
  const args = ['--port', String(port), '--cert-dir', certDir, '--log-level', opts.logLevel ?? process.env.AZEMU_TEST_LOG_LEVEL ?? 'debug'];
  for (const [svc, p] of Object.entries(opts.pins ?? {})) args.push('--pin', `${svc}=${p}`);
  args.push(...(opts.args ?? []));
  const log = createWriteStream(logPath);
  const proc = spawn(binaryPath(), args, {
    env: { ...process.env, ...(opts.env ?? {}) },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  proc.stdout.pipe(log, { end: false });
  proc.stderr.pipe(log, { end: false });
  await waitHealthy(port, proc);
  const emu = {
    port,
    gateway: `http://127.0.0.1:${port}`,
    certDir,
    caPath: join(certDir, 'ca.pem'),
    logPath,
    proc,
    dir,
    url(label, account, scheme = 'http') {
      const suffix = ['blob', 'queue', 'table', 'dfs'].includes(label) ? 'core.localhost' : 'localhost';
      return `${scheme}://${account}.${label}.${suffix}:${port}`;
    },
    async reset() {
      const r = await fetch(`${emu.gateway}/_azemu/reset`, { method: 'POST', body: '' });
      await r.arrayBuffer();
    },
    log() {
      try { return readFileSync(logPath, 'utf8'); } catch { return ''; }
    },
    async stop() {
      if (proc.exitCode === null) {
        proc.kill('SIGTERM');
        await new Promise((r) => {
          const t = setTimeout(() => { proc.kill('SIGKILL'); }, 3000);
          proc.once('exit', () => { clearTimeout(t); r(); });
        });
      }
      log.end();
      if (!process.env.AZEMU_TEST_KEEP_LOGS && !opts.keep) rmSync(dir, { recursive: true, force: true });
    },
  };
  return emu;
}

/**
 * Inside a test file: the emulator that run.mjs started (env AZEMU_PORT), or one started here when the file is run directly
 * (then NODE_EXTRA_CA_CERTS is missing, so only plain HTTP clients work).
 */
export function env() {
  const port = Number(process.env.AZEMU_PORT);
  if (!port) throw new Error('AZEMU_PORT is not set: run the tests through `npm test` (node run.mjs [name])');
  return {
    port,
    gateway: `http://127.0.0.1:${port}`,
    certDir: process.env.AZEMU_CERT_DIR,
    caPath: process.env.NODE_EXTRA_CA_CERTS,
    logPath: process.env.AZEMU_LOG_PATH,
    url(label, account, scheme = 'http') {
      const suffix = ['blob', 'queue', 'table', 'dfs'].includes(label) ? 'core.localhost' : 'localhost';
      return `${scheme}://${account}.${label}.${suffix}:${port}`;
    },
    async reset() {
      const r = await fetch(`http://127.0.0.1:${port}/_azemu/reset`, { method: 'POST', body: '' });
      await r.arrayBuffer();
    },
  };
}

let counter = 0;
/** A unique lower-case name, safe for containers, queues, vaults and so on. */
export function uniq(prefix = 't') {
  counter += 1;
  return `${prefix}${Date.now().toString(36)}${counter.toString(36)}${Math.random().toString(36).slice(2, 6)}`.toLowerCase();
}

export const DEV_ACCOUNT = 'devstoreaccount1';
export const DEV_KEY = 'Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==';

/** A token credential for services that accept any bearer token. */
export const fakeCredential = {
  async getToken() {
    return { token: 'any-token', expiresOnTimestamp: Date.now() + 3600_000 };
  },
};

export function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

/** Read a Node stream or Web stream body into a Buffer. */
export async function streamToBuffer(stream) {
  const chunks = [];
  for await (const c of stream) chunks.push(Buffer.isBuffer(c) ? c : Buffer.from(c));
  return Buffer.concat(chunks);
}

/** Collect an async iterable into an array. */
export async function collect(iter, limit = Infinity) {
  const out = [];
  for await (const x of iter) {
    out.push(x);
    if (out.length >= limit) break;
  }
  return out;
}

/** Run fn and return the thrown error (fails if nothing is thrown). */
export async function rejection(fn) {
  try {
    await fn();
  } catch (e) {
    return e;
  }
  throw new Error('expected the call to fail, but it succeeded');
}

/** Remember clients so a test file can close them all in `after`. Anything with close() or dispose() works. */
const tracked = [];
export function track(c) {
  tracked.push(c);
  return c;
}
export async function closeTracked() {
  const all = tracked.splice(0);
  await Promise.allSettled(all.map((c) => (c.close ? c.close() : c.dispose?.())));
}

/**
 * A storage account with blob versioning on, made the way a team does it: versioning is off until someone turns it
 * on. The account and its `blobServices/default` setting go through the management API (ARM is part of the emulator), then the
 * Blob endpoint of the account behaves like an Azure account with versioning. Returns the account name; use any key with it.
 */
export async function accountWithVersioning(emu, prefix = 'ver') {
  const { ResourceManagementClient } = await import('@azure/arm-resources');
  const { StorageManagementClient } = await import('@azure/arm-storage');
  const endpoint = `https://management.localhost:${emu.port}`;
  const opts = { endpoint, credentialScopes: [`${endpoint}/.default`] };
  const sub = '11111111-2222-3333-4444-555555555555';
  const group = uniq('rgver');
  await new ResourceManagementClient(fakeCredential, sub, opts).resourceGroups.createOrUpdate(group, { location: 'eastus' });
  const storage = new StorageManagementClient(fakeCredential, sub, opts);
  const account = uniq(prefix);
  await storage.storageAccounts.beginCreateAndWait(group, account, { location: 'eastus', sku: { name: 'Standard_LRS' }, kind: 'StorageV2' });
  const props = await storage.blobServices.setServiceProperties(group, account, { isVersioningEnabled: true });
  if (props.isVersioningEnabled !== true) throw new Error('versioning was not switched on');
  return account;
}
