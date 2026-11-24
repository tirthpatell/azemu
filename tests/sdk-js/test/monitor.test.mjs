import { describe, it, before } from 'node:test';
import assert from 'node:assert/strict';
import { env, uniq } from '../lib/emu.mjs';

// What the exporter would otherwise send to Microsoft's own hosts stays off. These are read when the package loads, so
// they are set before it is imported.
process.env.APPLICATION_INSIGHTS_NO_STATSBEAT = 'true';
process.env.APPLICATIONINSIGHTS_STATSBEAT_DISABLED_ALL = 'true';
delete process.env.APPLICATIONINSIGHTS_CONNECTION_STRING;

const { AzureMonitorTraceExporter, AzureMonitorMetricExporter, AzureMonitorLogExporter } = await import('@azure/monitor-opentelemetry-exporter');
const { BasicTracerProvider, SimpleSpanProcessor } = await import('@opentelemetry/sdk-trace-base');
const { MeterProvider, PeriodicExportingMetricReader } = await import('@opentelemetry/sdk-metrics');
const { LoggerProvider, SimpleLogRecordProcessor } = await import('@opentelemetry/sdk-logs');
const { resourceFromAttributes } = await import('@opentelemetry/resources');
const { SpanKind, SpanStatusCode } = await import('@opentelemetry/api');

const emu = env();
const IKEY = '11111111-2222-3333-4444-555555555555';
// the connection string as the docs show it: plain http, the emulator for both endpoints
const connectionString = `InstrumentationKey=${IKEY};IngestionEndpoint=http://127.0.0.1:${emu.port}/;LiveEndpoint=http://127.0.0.1:${emu.port}/`;
const resource = resourceFromAttributes({ 'service.name': 'azemu-js-test' });

async function admin(path) {
  const r = await fetch(`${emu.gateway}/_azemu/monitor${path}`);
  assert.equal(r.status, 200);
  return r.json();
}

/** Ask the admin API until `ok(items)` or give up after about 15 seconds. */
async function waitFor(query, ok) {
  let items = [];
  for (let i = 0; i < 150; i++) {
    items = (await admin(`/telemetry?${query}`)).items;
    if (ok(items)) return items;
    await new Promise((r) => setTimeout(r, 100));
  }
  assert.fail(`timed out waiting for telemetry (${query}); last: ${JSON.stringify(items.map((i) => i.type))}`);
}

describe('Azure Monitor OpenTelemetry exporter for JavaScript', () => {
  before(() => {
    // nothing to set up: the emulator is started by run.mjs
  });

  it('a server span and a client span reach the emulator as a request and a dependency of one operation', async () => {
    const provider = new BasicTracerProvider({
      resource,
      spanProcessors: [new SimpleSpanProcessor(new AzureMonitorTraceExporter({ connectionString, disableOfflineStorage: true }))],
    });
    const tracer = provider.getTracer('azemu-js');
    const server = tracer.startSpan('orders', { kind: SpanKind.SERVER, attributes: { 'http.request.method': 'GET', 'url.full': 'http://shop.example/orders', 'http.response.status_code': 200 } });
    const traceId = server.spanContext().traceId;
    const { trace, context } = await import('@opentelemetry/api');
    const client = tracer.startSpan('SELECT orders', { kind: SpanKind.CLIENT, attributes: { 'db.system': 'postgresql', 'db.statement': 'select 1' } }, trace.setSpan(context.active(), server));
    client.end();
    server.end();
    await provider.forceFlush();
    await provider.shutdown();

    const items = await waitFor(`operationId=${traceId}`, (i) => i.length >= 2);
    const byType = Object.fromEntries(items.map((i) => [i.type, i]));
    assert.deepEqual(Object.keys(byType).sort(), ['RemoteDependencyData', 'RequestData']);
    // the exporter names a request after its method and path, not after the span
    assert.equal(byType.RequestData.name, 'GET /orders');
    assert.equal(byType.RequestData.envelope.data.baseData.responseCode, '200');
    assert.equal(byType.RemoteDependencyData.name, 'SELECT orders');
    for (const i of items) {
      assert.equal(i.iKey, IKEY);
      assert.equal(i.envelope.tags['ai.operation.id'], traceId);
      assert.equal(i.envelope.tags['ai.cloud.role'], 'azemu-js-test');
    }
  });

  it('a span with an error status is a failed request, and a recorded exception is an exception item', async () => {
    const provider = new BasicTracerProvider({
      resource,
      spanProcessors: [new SimpleSpanProcessor(new AzureMonitorTraceExporter({ connectionString, disableOfflineStorage: true }))],
    });
    const span = provider.getTracer('azemu-js').startSpan('POST /pay', { kind: SpanKind.SERVER, attributes: { 'http.request.method': 'POST', 'url.full': 'http://shop.example/pay', 'http.response.status_code': 500 } });
    const traceId = span.spanContext().traceId;
    span.recordException(new TypeError('card declined'));
    span.setStatus({ code: SpanStatusCode.ERROR });
    span.end();
    await provider.forceFlush();
    await provider.shutdown();

    const items = await waitFor(`operationId=${traceId}`, (i) => i.length >= 2);
    const request = items.find((i) => i.type === 'RequestData');
    assert.equal(request.envelope.data.baseData.success, false);
    const exception = items.find((i) => i.type === 'ExceptionData');
    assert.equal(exception.envelope.data.baseData.exceptions[0].typeName, 'TypeError');
    assert.equal(exception.envelope.data.baseData.exceptions[0].message, 'card declined');
  });

  it('metrics', async () => {
    const reader = new PeriodicExportingMetricReader({
      exporter: new AzureMonitorMetricExporter({ connectionString, disableOfflineStorage: true }),
      exportIntervalMillis: 3_600_000,
    });
    const provider = new MeterProvider({ resource, readers: [reader] });
    const meter = provider.getMeter('azemu-js');
    const counter = uniq('orders.placed.');
    meter.createCounter(counter).add(3);
    await provider.forceFlush();
    await provider.shutdown();

    const items = await waitFor(`type=MetricData&name=${counter}`, (i) => i.length >= 1);
    assert.equal(items[0].envelope.data.baseData.metrics[0].value, 3);
  });

  it('log records', async () => {
    // this version of the logs SDK adds processors with a method (later versions take them in the constructor)
    const provider = new LoggerProvider({ resource });
    provider.addLogRecordProcessor(new SimpleLogRecordProcessor(new AzureMonitorLogExporter({ connectionString, disableOfflineStorage: true })));
    const text = uniq('disk is full ');
    provider.getLogger('azemu-js').emit({ body: text, severityNumber: 13, severityText: 'WARN' });
    await provider.forceFlush();
    await provider.shutdown();

    const items = await waitFor('type=MessageData', (i) => i.some((x) => x.envelope.data.baseData.message === text));
    const mine = items.find((x) => x.envelope.data.baseData.message === text);
    // this exporter writes the level as a word (Azure takes the word as well as the number)
    assert.equal(mine.envelope.data.baseData.severityLevel, 'Warning');
  });

  it('nothing the exporter sent was refused', async () => {
    const log = emu.logPath ? (await import('node:fs')).readFileSync(emu.logPath, 'utf8') : '';
    const refused = [...log.matchAll(/request service=monitor account=\S* method=(\S+) path=(\S+) status=(\d+)/g)].filter((m) => Number(m[3]) >= 400);
    assert.deepEqual(refused.map((m) => `${m[1]} ${m[2]} -> ${m[3]}`), []);
    assert.ok(/request service=monitor .*path=\/\/?v2\.1\/track status=200/.test(log), 'the exporter posted to /v2.1/track');
  });
});
