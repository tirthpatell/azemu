// Opens the inspector page (`/_azemu/ui`) in a headless Chrome against an emulator with a little data in it, visits one path of every
// service, clicks a few things, and fails on any error in the browser's console or on a page that shows an error where a node belongs.
// No packages: Node 22 or newer (the global WebSocket) and a Chrome or Chromium on the machine.
//
//   node tests/inspector-ui/check.mjs [--bin target/debug/azemu] [--chrome /usr/bin/google-chrome]
//
// `AZEMU_BIN` and `CHROME` do the same as the options.

import { spawn } from "node:child_process";
import { mkdtempSync, existsSync, rmSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

const args = process.argv.slice(2);
const opt = (name, env, fallback) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : process.env[env] || fallback;
};
const bin = opt("--bin", "AZEMU_BIN", "target/debug/azemu");
const chrome = opt("--chrome", "CHROME", ["/usr/bin/google-chrome", "/usr/bin/chromium", "/usr/bin/chromium-browser"].find(existsSync));
if (!existsSync(bin)) throw new Error(`no emulator at ${bin}: build it (cargo build -p azemu) or pass --bin`);
if (!chrome) throw new Error("no Chrome or Chromium found: pass --chrome");

const freePort = () =>
  new Promise((resolve) => {
    const s = createServer().listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const scratch = mkdtempSync(join(tmpdir(), "azemu-ui-"));
const children = [];
const failures = [];
const fail = (m) => {
  failures.push(m);
  console.error("FAIL " + m);
};

async function main() {
  const port = await freePort();
  const emu = spawn(bin, ["--port", String(port), "--cert-dir", join(scratch, "certs")], { stdio: "ignore" });
  children.push(emu);
  for (let i = 0; i < 100; i++) {
    try {
      if ((await fetch(`http://127.0.0.1:${port}/_azemu/health`)).ok) break;
    } catch {}
    await sleep(100);
  }
  await fill(port);
  await browse(port);
}

async function call(method, url, body, headers = {}) {
  const r = await fetch(url, { method, body, headers: { authorization: "Bearer x", ...headers } });
  if (r.status >= 300) throw new Error(`${method} ${url}: ${r.status} ${await r.text()}`);
}

async function fill(port) {
  const host = (label, account) => `http://${account}.${label}.localhost:${port}`;
  const shared = { authorization: "SharedKey acct:c2ln", "x-ms-date": new Date().toUTCString(), "x-ms-version": "2023-11-03" };
  const json = { "content-type": "application/json" };
  await call("PUT", `${host("blob", "acct")}/docs?restype=container`, "", shared);
  for (const name of ["a.txt", "dir%2Fb.txt", "%3Cscript%3Ealert(1)%3C%2Fscript%3E.txt"]) {
    await call("PUT", `${host("blob", "acct")}/docs/${name}`, "hello", { ...shared, "x-ms-blob-type": "BlockBlob" });
  }
  await call("PUT", `${host("queue", "acct")}/jobs`, "", shared);
  await call("POST", `${host("queue", "acct")}/jobs/messages`, "<QueueMessage><MessageText>hello &lt;b&gt;</MessageText></QueueMessage>", shared);
  await call("POST", `${host("table", "acct")}/Tables`, '{"TableName":"People"}', json);
  await call("POST", `${host("table", "acct")}/People`, '{"PartitionKey":"p","RowKey":"1","Name":"Ann"}', json);
  await call("PUT", `${host("vault", "vault1")}/secrets/db-password?api-version=7.6`, '{"value":"s3cr3t"}', json);
  await call("PUT", `${host("azconfig", "store1")}/kv/app%3Acolor?api-version=2024-09-01`, '{"value":"blue"}', json);
  const cosmos = { ...json, authorization: "type%3Dmaster%26ver%3D1.0%26sig%3Dx", "x-ms-date": "Wed, 11 Nov 2026 12:00:00 GMT", "x-ms-version": "2020-07-15" };
  await call("POST", `${host("documents", "acct")}/dbs`, '{"id":"shop"}', cosmos);
  await call("POST", `${host("documents", "acct")}/dbs/shop/colls`, '{"id":"orders","partitionKey":{"paths":["/c"],"kind":"Hash","version":2}}', cosmos);
  await call("POST", `${host("documents", "acct")}/dbs/shop/colls/orders/docs`, '{"id":"o1","c":"ann"}', { ...cosmos, "x-ms-documentdb-partitionkey": '["ann"]' });
  await call("PUT", `http://127.0.0.1:${port}/_azemu/eventgrid/topics/orders`, "{}", json);
  await call("PUT", `http://127.0.0.1:${port}/_azemu/eventgrid/topics/orders/subscriptions/hook`, '{"webhookUrl":"http://127.0.0.1:1/x","skipValidation":true}', json);
  await call("PUT", `${host("file.core", "acct")}/share1?restype=share`, "", shared).catch(() => {});
  const arm = (path, body) => call("PUT", `http://management.localhost:${port}${path}?api-version=2022-10-01-preview`, JSON.stringify(body), json);
  const g = "/subscriptions/11111111-2222-3333-4444-555555555555/resourceGroups/rg";
  await arm(g, { location: "eastus" });
  await arm(`${g}/providers/Microsoft.Web/sites/func`, { location: "eastus", properties: {} });
  await arm(`${g}/providers/Microsoft.ServiceBus/namespaces/sbns01`, { location: "eastus", sku: { name: "Standard" }, properties: {} });
  await arm(`${g}/providers/Microsoft.ServiceBus/namespaces/sbns01/queues/q`, { properties: {} });
  await arm(`${g}/providers/Microsoft.EventHub/namespaces/ehns01`, { location: "eastus", sku: { name: "Standard" }, properties: {} });
  await arm(`${g}/providers/Microsoft.EventHub/namespaces/ehns01/eventhubs/hub`, { properties: { partitionCount: 2 } });
}

// ---- a minimal DevTools client over the browser's WebSocket ---------------------------------------------------------

async function startChrome() {
  const proc = spawn(chrome, ["--headless=new", "--no-sandbox", "--disable-gpu", "--remote-debugging-port=0", `--user-data-dir=${join(scratch, "profile")}`, "about:blank"], {
    stdio: ["ignore", "ignore", "pipe"],
  });
  children.push(proc);
  const wsUrl = await new Promise((resolve, reject) => {
    let text = "";
    const timer = setTimeout(() => reject(new Error("Chrome did not say where it listens")), 20000);
    proc.stderr.on("data", (d) => {
      text += d;
      const m = text.match(/DevTools listening on (ws:\/\/\S+)/);
      if (m) {
        clearTimeout(timer);
        resolve(m[1]);
      }
    });
    proc.on("exit", () => reject(new Error("Chrome exited: " + text.slice(0, 500))));
  });
  return wsUrl;
}

function connect(url) {
  const ws = new WebSocket(url);
  let next = 1;
  const waiting = new Map();
  const listeners = [];
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    if (m.id && waiting.has(m.id)) {
      const { resolve, reject } = waiting.get(m.id);
      waiting.delete(m.id);
      m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result);
    } else listeners.forEach((l) => l(m));
  };
  const ready = new Promise((r) => (ws.onopen = r));
  return {
    ready,
    on: (fn) => listeners.push(fn),
    send: (method, params = {}, sessionId) => new Promise((resolve, reject) => {
      const id = next++;
      waiting.set(id, { resolve, reject });
      ws.send(JSON.stringify({ id, method, params, sessionId }));
    }),
    close: () => ws.close(),
  };
}

const PATHS = [
  "/", "/_emulator/config", "/_emulator/unsupported", "/_emulator/faults",
  "/blob/acct/containers/docs/blobs", "/blob/acct/containers/docs/blobs/a.txt",
  "/queue/acct/queues/jobs/messages", "/table/acct/tables/people/entities",
  "/keyvault/vault1/secrets/db-password", "/appconfig/store1/keyvalues",
  "/cosmos/acct/databases/shop/containers/orders/documents",
  "/eventgrid/orders/subscriptions/hook", "/servicebus/sbns01/queues/q", "/eventhubs/ehns01/hubs/hub/partitions/0",
  "/arm/11111111-2222-3333-4444-555555555555/resourcegroups/rg/resources", "/identity/signer", "/monitor/sink", "/auth/registry/roles",
];

async function browse(port) {
  const browserWs = connect(await startChrome());
  await browserWs.ready;
  const { targetId } = await browserWs.send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await browserWs.send("Target.attachToTarget", { targetId, flatten: true });
  const cdp = (method, params) => browserWs.send(method, params, sessionId);
  const consoleErrors = [];
  browserWs.on((m) => {
    if (m.sessionId !== sessionId) return;
    if (m.method === "Runtime.exceptionThrown") consoleErrors.push("exception: " + (m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text));
    if (m.method === "Runtime.consoleAPICalled" && m.params.type === "error") consoleErrors.push("console.error: " + m.params.args.map((a) => a.value ?? a.description).join(" "));
    if (m.method === "Log.entryAdded" && m.params.entry.level === "error") consoleErrors.push("log: " + m.params.entry.text + " " + (m.params.entry.url || ""));
  });
  await cdp("Runtime.enable");
  await cdp("Log.enable");
  await cdp("Page.enable");
  const base = `http://127.0.0.1:${port}/_azemu/ui`;
  await cdp("Page.navigate", { url: base });
  const evalJs = async (expression) => {
    const r = await cdp("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);
    return r.result.value;
  };
  // wait until the page has drawn the node (a heading) or an error banner, and nothing is loading
  const settle = async () => {
    for (let i = 0; i < 100; i++) {
      const done = await evalJs(`(document.querySelector("#loading") ? document.querySelector("#loading").hidden : false) && !!document.querySelector("#main h1, #main .error")`).catch(() => false);
      if (done) return;
      await sleep(100);
    }
    throw new Error("the page did not settle");
  };
  await settle();
  for (const path of PATHS) {
    await evalJs(`location.hash = ${JSON.stringify("#" + path)}; 0`);
    await sleep(150);
    await settle();
    const shown = await evalJs(`JSON.stringify({h1: (document.querySelector("#main h1")||{}).textContent, error: (document.querySelector("#main .error")||{}).textContent, crumbs: document.querySelector("#crumbs").textContent})`);
    const s = JSON.parse(shown);
    if (s.error) fail(`${path}: the page shows an error: ${s.error}`);
    else if (!s.h1) fail(`${path}: no heading`);
    else console.log(`ok  ${path}  ->  ${s.h1}`);
  }
  // the markup in a blob name is text and nothing else
  await evalJs(`location.hash = "#/blob/acct/containers/docs/blobs"; 0`);
  await sleep(200);
  await settle();
  const markup = await evalJs(`JSON.stringify({scripts: document.querySelectorAll("#main script, #tree script").length, text: document.querySelector("#main").textContent.includes("<script>alert(1)</script>.txt")})`);
  if (JSON.parse(markup).scripts !== 0 || !JSON.parse(markup).text) fail("a blob name with markup is not shown as text: " + markup);
  // the tree: open a service, then click a row and see the detail
  await evalJs(`document.querySelector("#tree a.tlink[href='#/queue']").click(); 0`);
  await sleep(400);
  await evalJs(`location.hash = "#/queue/acct/queues/jobs/messages"; 0`);
  await sleep(200);
  await settle();
  await evalJs(`document.querySelector("#main table.rows tbody tr").click(); 0`);
  if (!(await evalJs(`!!document.querySelector("#main .detail")`))) fail("a click on a row shows no detail");
  // the secret is shown only after the button
  await evalJs(`location.hash = "#/keyvault/vault1/secrets/db-password"; 0`);
  await sleep(200);
  await settle();
  if (await evalJs(`document.querySelector("#main").textContent.includes("s3cr3t")`)) fail("a secret value is on the page before the button");
  await evalJs(`[...document.querySelectorAll("#main button")].find((b) => b.textContent.includes("Reveal")).click(); 0`);
  await sleep(500);
  if (!(await evalJs(`document.querySelector("#main").textContent.includes("s3cr3t")`))) fail("the reveal button did not show the value");
  // auto refresh on, then off, and the refresh button
  await evalJs(`document.querySelector("#auto").click(); 0`);
  await sleep(2600);
  await evalJs(`document.querySelector("#auto").click(); document.querySelector("#refresh").click(); 0`);
  await sleep(300);
  for (const e of consoleErrors) fail(e);
  browserWs.close();
}

try {
  await main();
} catch (e) {
  fail(String(e.stack || e));
} finally {
  for (const c of children) c.kill();
  // Chrome is still writing its profile for a moment after the signal: try a few times, and never fail the check over a
  // scratch directory that could not be removed (it is under the system's temporary directory).
  try {
    rmSync(scratch, { recursive: true, force: true, maxRetries: 20, retryDelay: 250 });
  } catch (e) {
    console.error(`note: could not remove ${scratch}: ${e.code || e.message}`);
  }
}
if (failures.length) {
  console.error(`${failures.length} problem(s)`);
  process.exit(1);
}
console.log("the inspector page works in " + chrome);
