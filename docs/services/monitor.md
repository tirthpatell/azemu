# Monitor (Application Insights and Azure Monitor telemetry)

**What it is.** A sink. It takes what an application sends to Application Insights and to the Logs Ingestion API, keeps it in memory, and lets a test read it back
through the admin API. An application with `APPLICATIONINSIGHTS_CONNECTION_STRING` set runs against the emulator without logging errors, and a test can check what
the application reported (the names, the types, the operation ids). It is **not** a query engine: nothing here runs a Kusto query, draws a chart or raises an alert.

**What it takes.**

- Application Insights ingestion (the "Breeze" endpoint that the SDKs and the OpenTelemetry exporters post to): `POST /v2/track` and `POST /v2.1/track`.
- What the SDKs call besides `track`, so that nothing logs an error: the application id lookup and Live Metrics (QuickPulse), answered as "nobody is watching".
- The Logs Ingestion API of Data Collection Rules (the `azure-monitor-ingestion` clients): `POST /dataCollectionRules/{rule}/streams/{stream}`.

**How to reach it.** The connection string names the endpoint. The instrumentation key can be any GUID.

| Where the application runs | Connection string |
|---|---|
| gateway, names under `.localhost` | `InstrumentationKey=11111111-2222-3333-4444-555555555555;IngestionEndpoint=http://eastus.in.localhost:4577/;LiveEndpoint=http://eastus.livediagnostics.localhost:4577/` |
| gateway, no host name that means anything (`localhost`, an address) | `InstrumentationKey=...;IngestionEndpoint=http://127.0.0.1:4577/;LiveEndpoint=http://127.0.0.1:4577/` |
| another container, with `--standard-ports` | `InstrumentationKey=...;IngestionEndpoint=http://azemu:10015/;LiveEndpoint=http://azemu:10015/` |

The region (`eastus`) is only a name. The real hosts are `{region}.in.applicationinsights.azure.com` and `{region}.livediagnostics.monitor.azure.com`. The
second row works because the emulator takes the paths of these endpoints (`/v2/track`, `/api/profiles/...`, `/QuickPulseService.svc/...`) on a host that names no
service. Set it as `APPLICATIONINSIGHTS_CONNECTION_STRING` and the SDKs find it.

The Logs Ingestion client sends a bearer token, so it needs `https` and the emulator's certificate authority (`docs/reaching-the-emulator.md`). The endpoint is
`https://dce1.ingest.localhost:4577` (the first label is a data collection endpoint name, kept for nothing), `https://127.0.0.1:4577`, or `https://azemu:10015` with
`--standard-ports` and `AZEMU_TLS_NAME=azemu`. The rule id (`dcr-...`) and the stream name (`Custom-Table_CL`) can be anything.

Application Insights ingestion takes plain `http` in `IngestionEndpoint` in the two exporters that were tried (below); no token goes with the instrumentation key.
An application that signs in with a token credential has the same `http` problem as everywhere else: the Azure SDKs refuse to send a token over plain `http`.

**Not built.** The query APIs (`api.applicationinsights.io`, `api.loganalytics.io`: `/v1/workspaces/{id}/query`, `/v1/apps/{id}/query`, metrics, events), metrics
queries, alerts, and the custom metrics ingestion of `monitoring.azure.com` answer 501 and name the call. Resource Manager makes an Application Insights component and a Log Analytics
workspace (below, "Resource Manager"); it does not make a Data Collection Rule or endpoint here: write those by hand.

Crate `azemu-monitor` (`crates/services/monitor`), service name `monitor`. Mounts: `AccountHost("in")`, `AccountHost("livediagnostics")`, `AccountHost("ingest")`,
`Host("api")`, `PathPrefix("/_azemu/monitor")`, and the paths above at the root of a host that names no service (`Service::claims_root_path`). Standard port 10015.

## Operations

| Operation | Status |
|---|---|
| `POST /v2/track`, `POST /v2.1/track`: a JSON array of envelopes, one JSON object, or one value per line; plain or `Content-Encoding: gzip`; answers `200`, `206` or `400` with `itemsReceived`, `itemsAccepted`, `appId` and `errors` | built |
| `OPTIONS /v2/track` (the preflight of a browser SDK): `204` with Azure's `Access-Control-*` headers | built |
| `GET /api/profiles/{ikey}/appId`: the application id as plain text | built |
| `POST /QuickPulseService.svc/ping`, `POST /QuickPulseService.svc/post` (Live Metrics): `200` and `x-ms-qps-subscribed: false`, in the old form (`?ikey=`, data in `x-ms-qps-*` headers) and the new one (`?api-version=2024-04-01-preview&ikey=`, data in a JSON body) | built |
| `POST /dataCollectionRules/{rule}/streams/{stream}?api-version=...` (Logs Ingestion): a JSON array of records, optional gzip, `204` | built |
| `GET /_azemu/monitor/`, `GET` and `DELETE /_azemu/monitor/telemetry`, `GET` and `DELETE /_azemu/monitor/logs` | built (emulator only) |
| the query APIs, metrics, alerts, custom metrics, any other path on an ingestion host | 501 `NotImplemented` naming the call (`Monitor_Unknown (POST /v1/workspaces/{}/query)` in `GET /_azemu/unsupported`) |

Every call is accepted with a bearer token, an instrumentation key, or neither: nothing is checked beyond what Azure checks without knowing the key. The Logs
Ingestion call needs a bearer token (not decoded), as the documentation says.

### What an envelope has to be

Checked against Azure in November 2026 (raw requests to a real Application Insights resource; the messages below are Azure's own). Azure reports the first
problem it finds in an item, in this order, and the emulator does the same:

1. `iKey`: a GUID in any form (upper or lower case, with or without dashes, in braces). Anything else, or nothing, is `Invalid instrumentation key`. Azure also
   refuses a GUID it does not know; **the emulator cannot tell which keys exist, so it takes every GUID**.
2. `data` and `data.baseData` must be objects: `InvalidJsonObject: Not a valid JSON object`.
3. `ver`, if it is a string: `EnvelopeSchemaVersionUnsupported: Envelope Schema version 'x' is not supported.`
4. `name`: `109: Field 'name' on type 'Envelope' is required but missing or empty. Expected: string`.
5. `time`: `109` when missing, `102: ... is not a valid time string. Expected: date`, `106` when it is not a string. Older than 48 hours:
   `103: Field 'time' on type 'Envelope' is older than the allowed min date. Expected: now - 172800000ms`. More than 2 hours ahead:
   `104: Field 'time' on type 'Envelope' is newer than the allowed max date. Expected: now + 7200000ms`. A time with no fraction, no zone, or a date alone is taken.
6. `sampleRate` a number, `flags` an integer, `tags` an object (`106: Field 'tags' on type 'Envelope' is of incorrect type. Expected: object`).
7. `data.baseType` one of `RequestData`, `RemoteDependencyData`, `ExceptionData`, `MessageData`, `EventData`, `MetricData`, `PageViewData`, `AvailabilityData`, with
   exactly this case: `Unknown domain type "NopeData"`.
8. The fields of `baseData` that Azure insists on: `RequestData` needs `id`, `duration` and `responseCode`; `RemoteDependencyData`, `PageViewData` and `EventData`
   need `name`; `AvailabilityData` needs `duration`; `MessageData` needs `message`; `ExceptionData` needs an `exceptions` array whose items have `typeName` and
   `message`; `MetricData` needs a `metrics` array whose items have `name` and a numeric `value`. A `duration` is `[d.]hh:mm:ss[.fffffff]` (`108` when it is not).
   `severityLevel` is 0 to 4 or the name of a level (`107`). `properties` and `measurements` must be objects, `success` a boolean.
9. An item over 1,258,291 bytes: `Telemetry item length must not exceed 1258291`.

Each refused item is an entry of `errors` with its `index`, `statusCode` 400 and `message`. When some items are refused and some taken, the answer is `206`; when none is
taken, it is `400`; a body with nothing in it is `200` with `itemsReceived: 0`. Only the accepted items are kept.

A body that Azure cannot read: text that is not JSON, and values that are not objects, are skipped (nothing received, `200`). A value that starts like JSON and is
cut off is `400` with `Invalid JSON specified in the request`. A body that says gzip and is not gzip is `400` with `{"message":"Malformed content","statusCode":400}`
(a different shape). A `Content-Encoding` other than gzip is not undone. A body over 64 MiB once undone is `413`.

## Reading it back (the admin API)

Listed in `GET /_azemu/`. Telemetry is kept as it came: the whole envelope.

| Request | Answer |
|---|---|
| `GET /_azemu/monitor/` | counts: telemetry stored, dropped, the last sequence number, and the count per instrumentation key and base type; the same for log rows per rule and stream; the limits |
| `GET /_azemu/monitor/telemetry` | `{"count", "stored", "dropped", "lastSequence", "truncated", "items": [...]}`, oldest first. Each item: `sequence`, `receivedAt`, `iKey`, `type` (the base type), `name`, `operationId`, `envelope` (what was sent) |
| `GET /_azemu/monitor/logs` | the same for Logs Ingestion: `sequence`, `receivedAt`, `dcr`, `stream`, `row` |
| `DELETE /_azemu/monitor/telemetry`, `DELETE /_azemu/monitor/logs` | empty one list: `{"deleted": n}` |
| `POST /_azemu/reset` (or `?service=monitor`) | empties both |

Filters on `telemetry` (query parameters, all optional, combined with "and"; an unknown parameter or a value that is not a number is a `400`):

| Parameter | Matches |
|---|---|
| `ikey` | the instrumentation key, in any GUID form |
| `type` | the base type: `RequestData`, `RemoteDependencyData`, `ExceptionData`, `MessageData`, `EventData`, `MetricData`, `PageViewData`, `AvailabilityData` |
| `name` | `baseData.name` (a request, a dependency, an event, a page view), the name of any metric of a `MetricData`, or the envelope's own `name` |
| `operationId` | the `ai.operation.id` tag (with OpenTelemetry, the trace id) |
| `since` | only items with a sequence number above this; read `lastSequence` first, do the work, then ask for what came after |
| `limit` | at most this many (the oldest first); `truncated` says there were more |

`logs` takes `dcr`, `stream`, `since` and `limit`.

```bash
curl -s "localhost:4577/_azemu/monitor/telemetry?type=RequestData&operationId=0af7651916cd43dd8448eb211c80319c" | jq '.items[].name'
mark=$(curl -s localhost:4577/_azemu/monitor/telemetry?limit=0 | jq .lastSequence)     # do something, then:
curl -s "localhost:4577/_azemu/monitor/telemetry?since=$mark"
```

Sequence numbers rise for ever: a `DELETE` or a reset does not start them again.

### Bounds

Telemetry is noise, so each list keeps a fixed number of items and bytes and drops the oldest. `AZEMU_MONITOR_MAX_ITEMS` (telemetry, default 50,000),
`AZEMU_MONITOR_MAX_LOG_ROWS` (log rows, 50,000) and `AZEMU_MONITOR_MAX_MB` (megabytes of each list, 128). The answers of the admin API say how many were dropped
(`dropped`; it counts from the last empty). Each item is kept as the text it came as (compact JSON, read again when you ask for it), and the byte limit counts that text plus the
strings next to it and a flat 256 bytes for the item itself, so the limit is what is held (a parsed JSON value takes about thirty times the size of its text). A request is read one
item at a time: nothing is made of an item that is longer than the item limit (1,258,291 bytes, measured without the white space between its tokens), and **a request with more than
65,536 items is `400`** ("The request has more than 65536 items, the most one request may carry."), refused before the items are held. The 65,536 is the documented maximum of a batch;
the answer for more is the emulator's own (Azure's was not tried). Measured with a debug build: three requests of about 19 KB of gzip (ten items of 1.2 MB each, 18 MB undone) took the
process from 39 MB to 76 MB, the size of the text kept. **Nothing is written to `--data-dir`**: a restart empties both lists.

## Using it from the SDKs

Tried in November 2026, each with the emulator's request log at `--log-level debug` to see that no request was refused:

| SDK | What was tried | Notes |
|---|---|---|
| Python `azure-monitor-opentelemetry-exporter` 1.0.0b59 | traces (server and client spans, recorded exceptions, error status), metrics (counter, histogram), logs (also inside a span), Live Metrics (`enable_live_metrics`) | plain `http` in `IngestionEndpoint` and `LiveEndpoint` works. It posts to `/v2.1/track` and pings `/QuickPulseService.svc/ping?api-version=2024-04-01-preview&ikey=...` with a JSON body. Turn off what it would send to Microsoft's own hosts: `APPLICATIONINSIGHTS_STATSBEAT_DISABLED_ALL=true`, `APPLICATIONINSIGHTS_CONTROLPLANE_DISABLED=true`, `APPLICATIONINSIGHTS_SDKSTATS_DISABLED=true`, and `disable_offline_storage=True` in a test |
| JavaScript `@azure/monitor-opentelemetry-exporter` 1.0.0-beta.32 | traces, metrics, logs | plain `http` works. Turn off its statistics with `APPLICATION_INSIGHTS_NO_STATSBEAT=true`. It names a request after the method and path (`GET /orders`), not after the span; it writes a log's severity as a word (`Warning`), which Azure takes too |
| Python `azure-monitor-ingestion` 1.1.0 | `upload` of a few rows, of 3 MB (cut into calls of about 1 MB), of a stream that is already gzip, and the async client | needs `https` and `connection_verify=<ca.pem>` on the client; any credential object that returns a token |
| .NET `Azure.Monitor.OpenTelemetry.Exporter` 1.10.0 (with `UseAzureMonitorExporter`, which turns Live Metrics on) and `Microsoft.ApplicationInsights` 2.23.0 | spans, runtime metrics, logs, and `TrackEvent`, `TrackTrace`, `TrackMetric`, `TrackException`, `TrackRequest`, `TrackDependency` | tried once by hand, not part of the test suites. Plain `http` works. The exporter pings `/QuickPulseService.svc/ping?api-version=2024-04-01-preview&ikey=...` every 5 seconds with the data in `x-ms-qps-*` headers and an empty body. The classic SDK posts to `/v2/track`. Every request was answered 200 |

Not tried: the Java agent, the Go and Java and .NET ingestion clients, and the browser SDK. From their public documentation: the classic .NET (with its Windows Server
package) and Java SDKs also call `GET /api/profiles/{ikey}/appId` and Live Metrics; both are built and were checked against Azure with raw requests, but not with
those SDKs. If an SDK does not take `http` in `IngestionEndpoint`, use `https` and the emulator's certificate authority for that SDK
(`docs/reaching-the-emulator.md`).

## Known differences from Azure

- **Which keys exist.** Azure refuses a GUID it does not know (`Invalid instrumentation key`, and `404 Profile Not Found` for the application id). The emulator has no
  list of resources, so every GUID is a key and every GUID has an application id (made from the key, the same every time).
- **Tokens.** Azure answered a made-up bearer token with `Authorization not supported` (the resource did not have Entra ID sign-in turned on); a token from Entra for
  `https://monitor.azure.com//.default` was taken, and the instrumentation key was still needed. The emulator takes any token or none.
- **The regional redirect.** The global endpoint (`dc.services.visualstudio.com`) answered `v2.1/track` with `307` ("Ingestion is allowed only from stamp specific
  endpoint") and the address of the regional host. The emulator has one address and never redirects.
- **Live Metrics.** Azure's answer carries `x-ms-qps-service-endpoint-redirect-v2`, the address of the regional Live Metrics host; the emulator leaves it out (the
  Python exporter refuses a redirect to a host that is not one of Microsoft's). Nobody ever watches, so `post` is never asked for by the SDK that follows `x-ms-qps-subscribed`.
- **No sampling, no daily cap, no throttling** (`429`, `439`), no `Retry-After`.
- **An empty `metrics` list.** Azure took a `MetricData` with an empty `metrics` list alone, and answered `500 Internal server error` for it inside a batch of eleven
  items. The emulator takes it.
- **Not found.** Azure answers `404` with no body for a path it does not have; the emulator answers `501` and names the call, as everywhere in this project.
- **Time.** The window of a `time` is measured from the emulator's own clock.

## Behaviour that could not be confirmed against Azure

There was no Data Collection Rule on the Azure side, so **everything about the Logs Ingestion API comes from the documentation and the SDKs' source, not from a
recording**:

- The documented limit is 1 MB per call, "compressed or uncompressed". The emulator refuses a body as sent that is over 1 MiB with `413` and
  `{"error":{"code":"ContentLengthLimitExceeded",...}}`, and a gzip body that is over 1.25 MiB once undone. The extra quarter is on purpose: the Python client cuts its
  calls at 1 MiB of JSON counted **without** the separators between the records, so a call can be a little over 1 MiB, and Azure takes what the SDK sends. The code name is a guess.
- A call without `Authorization: Bearer ...` is `401` with code `Unauthorized`; a body that is not a JSON array of objects, or gzip that is not gzip, is `400` with
  code `InvalidRequest`. The documentation names the status codes (400 for malformed JSON, 403, 413, 429), not the codes inside the error body.
- The emulator accepts any rule id and any stream name and keeps them as they came. Azure answers an unknown rule or stream with an error (`404`, or `InvalidStream`);
  the emulator cannot, because rules and streams are not declared anywhere. It does not look at `api-version`, the content type or what is in the records.
- The answer to a good call is `204` with `x-ms-request-id`; the other headers of Azure's answer are not known.

For Application Insights, what was **not** tried:

- The `x-ms-qps-*` answer of a ping when someone is watching, and what `post` carries then (the emulator never says "subscribed").
- The size at which Azure refuses a whole request (the emulator's `413` for a body over 64 MiB once undone is a guess), and the answer for more than 65,536 items in one
  batch (the documented maximum; the emulator answers `400` with its own text).
- Whether `deflate` is taken as an encoding (the emulator does not undo it).

## Resource Manager

`Microsoft.Insights/components` made through Resource Manager (the Terraform resource `azurerm_application_insights`, a template, an SDK) has an `InstrumentationKey`, an `AppId` and a `ConnectionString`
whose `IngestionEndpoint` and `LiveEndpoint` are this service: `http://{location}.in.localhost:{port}/` and `http://{location}.livediagnostics.localhost:{port}/`. A program that takes the connection string from the
resource can send telemetry with it, and a test reads it back with `GET /_azemu/monitor/telemetry?ikey=...`. `GET /api/profiles/{ikey}/appId` answers the `AppId` of the resource for its key (the same
function makes both). `Microsoft.OperationalInsights/workspaces` is stored with the fields Terraform reads and hands out shared keys. Details, and what was tried, are in [`arm.md`](arm.md#application-insights-and-log-analytics).

Not built: `Microsoft.Insights/dataCollectionRules` and `dataCollectionEndpoints` (they would need `immutableId` and `properties.endpoints.logsIngestion`); ARM stores them without acting on them.

## Strict authorisation (`--strict-auth`)

With `--strict-auth` ([`../strict-auth.md`](../strict-auth.md)) this service does **not** check roles: it takes any token as before, and the start-up log names it among the services that do not.
