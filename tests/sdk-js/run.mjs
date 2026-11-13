// Test runner: for each test file, start an emulator, set NODE_EXTRA_CA_CERTS to its CA, run `node --test <file>`, stop the emulator.
// Usage: node run.mjs [name-filter ...]     (npm test runs all files; at most JOBS in parallel, default 3)
import { spawn } from 'node:child_process';
import { readdirSync, mkdirSync, statSync, writeFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { startEmulator, binaryPath, projectDir } from './lib/emu.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const filters = process.argv.slice(2).filter((a) => !a.startsWith('--'));
const jobs = Number(process.env.JOBS ?? 3);
const files = readdirSync(join(here, 'test'))
  .filter((f) => f.endsWith('.test.mjs'))
  .filter((f) => filters.length === 0 || filters.some((x) => f.includes(x)))
  .sort();

console.log(`binary: ${binaryPath()} (${statSync(binaryPath()).size} bytes)`);
mkdirSync(join(here, '.tmp'), { recursive: true });

async function runFile(file) {
  const started = Date.now();
  const emu = await startEmulator({ tmpBase: join(here, '.tmp') });
  const env = {
    ...process.env,
    AZEMU_PORT: String(emu.port),
    AZEMU_CERT_DIR: emu.certDir,
    AZEMU_LOG_PATH: emu.logPath,
    NODE_EXTRA_CA_CERTS: emu.caPath,
    IDENTITY_ENDPOINT: `http://localhost:${emu.port}/_azemu/identity/msi/token`,
    IDENTITY_HEADER: 'azemu',
  };
  const out = [];
  const code = await new Promise((resolve) => {
    const p = spawn(process.execPath, ['--test', '--test-reporter=spec', '--test-force-exit', `--test-timeout=${process.env.TEST_TIMEOUT_MS ?? 120000}`, join(here, 'test', file)], { env, stdio: ['ignore', 'pipe', 'pipe'] });
    // a file that hangs is stopped after FILE_TIMEOUT_S seconds (default 600) so the run always ends
    const killer = setTimeout(() => { out.push(Buffer.from(`\nrunner: ${file} killed after ${process.env.FILE_TIMEOUT_S ?? 600}s\n`)); p.kill('SIGKILL'); }, Number(process.env.FILE_TIMEOUT_S ?? 600) * 1000);
    p.on('exit', () => clearTimeout(killer));
    p.stdout.on('data', (d) => out.push(d));
    p.stderr.on('data', (d) => out.push(d));
    p.on('exit', (c) => resolve(c));
  });
  await emu.stop();
  const text = Buffer.concat(out).toString();
  const num = (k) => Number((text.match(new RegExp(`ℹ ${k} (\\d+)`)) ?? [0, 0])[1]);
  return { file, code, text, pass: num('pass'), fail: num('fail'), skipped: num('skipped'), secs: ((Date.now() - started) / 1000).toFixed(1) };
}

const results = [];
let next = 0;
async function worker() {
  while (next < files.length) {
    const f = files[next++];
    try {
      const r = await runFile(f);
      results.push(r);
      console.log(r.text);
      console.log(`== ${f}: pass ${r.pass} fail ${r.fail} skipped ${r.skipped} (${r.secs}s)`);
    } catch (e) {
      results.push({ file: f, code: 1, pass: 0, fail: 1, skipped: 0, secs: '0', text: String(e) });
      console.log(`== ${f}: runner error ${e.stack ?? e}`);
    }
  }
}
await Promise.all(Array.from({ length: Math.min(jobs, files.length) }, worker));

console.log('\n==== summary');
let p = 0, f = 0, s = 0;
for (const r of results.sort((a, b) => a.file.localeCompare(b.file))) {
  console.log(`${r.file.padEnd(34)} pass ${String(r.pass).padStart(4)} fail ${String(r.fail).padStart(3)} skipped ${String(r.skipped).padStart(3)}  ${r.secs}s`);
  p += r.pass; f += r.fail; s += r.skipped;
}
console.log(`TOTAL pass ${p} fail ${f} skipped ${s}`);
process.exit(f > 0 ? 1 : 0);
