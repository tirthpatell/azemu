# Azure Functions host against azemu

Does the real Azure Functions host (`func start`, Azure Functions Core Tools 4) run against azemu? This directory is the
test: a small function app with one function per trigger or binding, and a runner that starts a private copy of the
emulator, creates the queues, topics, hubs and containers a team would create, starts the host, drives every function and
prints one line per function.

The result to expect: **every function works**, including the classic (polling) blob trigger with host-name style storage endpoints (`group blobhost`) and identity-based Service Bus
with `--standard-ports` (port 5671). Cosmos DB direct mode is not built: use gateway mode. The settings each extension needs are listed below and in
[`docs/sdk-notes.md`](../../docs/sdk-notes.md#azure-functions).

This suite is not part of `tests/run_sdk_suites.py` or `tests/run_all.sh`.

## Run it

```bash
tests/functions/run.sh                       # .NET app, every group, about 1 minute
tests/functions/run.sh --only timer,sb,eh    # some groups: http timer queue blob blobeg eg table sb eh cosmos restart blobhost
tests/functions/run.sh --identity            # identity-based connections instead of connection strings
tests/functions/run.sh --lenient             # emulator started with AZEMU_LENIENT=1, nothing created first
tests/functions/run.sh --app node            # Node worker: queue and Service Bus triggers
tests/functions/run.sh --app durable         # Durable Functions
tests/functions/run.sh --storage-style host  # every group with storage endpoints as http://{account}.blob.core.localhost:{port}
STANDARD_PORTS=1 tests/functions/run.sh --identity --only sb   # emulator started with --standard-ports (opens 5671) instead of --pin servicebus=5671
AZEMU_FUNC_SCRATCH=~/.cache/somewhere tests/functions/run.sh   # scratch directory (default ~/.cache/azemu-functions)
REBUILD=1 tests/functions/run.sh             # rebuild the app first
```

`run.sh` builds the app with the .NET SDK (one build node, small heap), then runs `run.py` with `$PYTHON`, `.venv/bin/python` or `python3`.
`run.py` copies `target/debug/azemu` to `~/.cache/azemu-functions/azemu` (again whenever `target/debug/azemu` is newer; it needs `target/debug/azemu`, so build it or put a copy there), starts
it on free ports with a temporary certificate directory and `HOME`, starts the host on a free port, stops both at the end and
deletes its scratch directory `~/.cache/azemu-functions/run-<pid>`. Logs of the last run are in `tests/functions/logs/`
(`azemu.log` at debug level, `host.log`, `host-restart.log`, `unsupported.json` = `GET /_azemu/unsupported` at the end).

The exit code is 0 when every line says `works`.

**Needs the network** only for: the first install (below), NuGet restore at the first build, `npm install` for the Node app, and
the extension bundle that the host downloads for the Node app (about 220 MB into `~/.azure-functions-core-tools/`). Runs after that need no network.

## What you need

* The .NET 10 SDK (`DOTNET_ROOT`, or `dotnet` on the `PATH`; see [`tests/sdk-dotnet`](../sdk-dotnet/README.md) for installing it without root).
* Azure Functions Core Tools 4: `npm install -g azure-functions-core-tools@4` (or in a directory of your own, with `FUNC_BIN=/path/to/func`). With npm 11 the package's
  `postinstall` script does not run unless you approve it: run `node lib/install.js` in the package directory by hand. If that downloads the zip into `bin/` and exits without
  unpacking it, unpack it yourself (`unzip -q -o Azure.Functions.Cli.linux-x64.*.zip` in `bin/`, then `chmod +x func in-proc8/func in-proc6/func`).
* Node and npm for the Node app (`--app node`), Python 3 with the packages in `../sdk-python/requirements.txt` for the runner (`.venv/bin/python`, or `PYTHON=/path/to/python`).
* A built emulator: `cargo build -p azemu`.
* Network access for the first run only: NuGet restore, `npm install` for the Node app, and the extension bundle the host downloads for it (about 220 MB, into
  `~/.azure-functions-core-tools/`).

Run it alone: it starts the host, a .NET worker, the emulator and a Python process at once.

## Tested with

| | |
|---|---|
| Core Tools | 4.16.0 (npm `azure-functions-core-tools@4`) |
| Functions host runtime | 4.1054.250.26428 |
| .NET SDK / runtime | 10.0.404 / 10.0.15 (`net10.0`, isolated worker; `Microsoft.Azure.Functions.Worker` 2.52.0, `.Sdk` 2.1.0) |
| Extensions (from the app's NuGet packages) | Service Bus 5.17.0, Event Hubs 6.5.2, Cosmos DB 4.16.1, Storage Blobs 5.3.8, Storage Queues 5.3.8, Tables 1.4.0, Event Grid 3.5.0, Timer and HTTP are in the host, Durable Task 1.22.0 (separate app) |
| Node app | Node v26.13.0 (the Node worker 3.16.0 in Core Tools accepted it), `@azure/functions` ^4.7, extension bundle 4.39.1 |
| Python worker | Core Tools 4.16.0 contains workers for Python 3.9 to 3.14, so 3.13 would be supported; not run (the Node app is the second data point) |

Language workers: **.NET isolated (net10.0): yes. Node: yes. Python: not tried.** The in-process .NET model
is in Core Tools (`in-proc8`) but was not tried.

## The settings that were needed, per extension

All of this is set through environment variables by `run.py` (`settings()` and `host_settings()`); in a team's project they go
into `local.settings.json` (`Values`) with the same names. Port numbers below are the emulator's.

**Always:** `FUNCTIONS_WORKER_RUNTIME=dotnet-isolated` (or `node`), `DOTNET_ROOT` and `dotnet` on `PATH` for the worker. For
HTTPS to the emulator (Cosmos, Event Grid output, identity-based connections) the host process needs the emulator's CA:
`SSL_CERT_FILE=<system roots + ca.pem>` (one file; build it as in the main README, "A bundle for tools that read one file").
`SSL_CERT_FILE` only for the host process, never for the whole shell. Without it the Cosmos trigger fails with "The SSL connection
could not be established".

**`AzureWebJobsStorage`** (host bookkeeping, timer lock, queue, blob, table bindings, Durable Functions, Event Hubs checkpoints). Start
the emulator with pinned ports for the three storage services and use **Azurite-style path endpoints**:

```
azemu --pin blob=B --pin queue=Q --pin table=T
AzureWebJobsStorage=DefaultEndpointsProtocol=http;AccountName=myacct;AccountKey=<any base64>;BlobEndpoint=http://127.0.0.1:B/myacct;QueueEndpoint=http://127.0.0.1:Q/myacct;TableEndpoint=http://127.0.0.1:T/myacct
```

`UseDevelopmentStorage=true` only fits the fixed Azurite ports 10000 to 10002 (`--pin blob=10000 --pin queue=10001 --pin table=10002`)
and the account `devstoreaccount1`. The host decides "development storage" from the shape of the blob URL (`{base}/{account}`)
and only then scans containers every 2 seconds for the classic blob trigger. With host-name endpoints
(`http://myacct.blob.core.localhost:4577`, the style the main README shows) it picks the Storage Analytics log strategy: it switches Blob logging on,
lists `$logs`, and reads the log blobs for new blobs (every 10 seconds, plus the delay of the emulator, 3 seconds by default,
`AZEMU_BLOB_LOG_DELAY`). The emulator writes those (`docs/services/blob.md`, "Storage Analytics logging"), so **both styles work**
(group `blobhost`); the path style is faster (the container scan runs every 2 seconds; the log scan every 10 seconds plus the delay), but needs pinned ports. Everything else works with either style.

**Service Bus** (`ServiceBusConnection`): `Endpoint=sb://myns.servicebus.localhost:PORT;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=<any>;UseDevelopmentEmulator=true`.
The SDK keeps the port and speaks plain AMQP; no TLS setup. Queues, topics and subscriptions must exist first (administration client or ARM); a missing one gives
"The messaging entity ... could not be found" in the host log, as on Azure. `--lenient` creates them (with Azure's defaults, e.g. max delivery count 10).

**Event Hubs** (`EventHubConnection`): `Endpoint=sb://myns.eventhub.localhost:PORT;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=<any>;UseDevelopmentEmulator=true`
(the label `eventhub` makes it Event Hubs). The hub must exist (`PUT /_azemu/eventhubs/{ns}/hubs/{hub}` or ARM). The checkpoint store is a container `azure-webjobs-eventhub` in `AzureWebJobsStorage`; it works as is, including a restart.

**Cosmos DB** (`CosmosConnection`): `AccountEndpoint=https://myacct.documents.localhost:PORT/;AccountKey=<any>;` with `SSL_CERT_FILE` as above, **or**
`AccountEndpoint=http://myacct.documents.localhost:PORT/;...` which needs no certificate at all (tested). host.json: `"extensions": {"cosmosDB": {"connectionMode": "Gateway"}}`
(Gateway is the default of extension 4.16.1 and works with or without the line; `Direct` fails: the SDK asks `GET //addresses/` and azemu answers 501, whose message says to use gateway mode; there is no direct mode). Database and monitored container must exist; the lease container is made by
`CreateLeaseContainerIfNotExists = true`. Cosmos is not covered by `--lenient`.

**Event Grid:** an Event Grid output binding needs `TopicEndpointUri=https://mytopic.eventgrid.localhost:PORT/api/events` and any `TopicKeySetting` value (TLS: `SSL_CERT_FILE`). An Event Grid trigger or the Event Grid based
blob trigger is a webhook: after the host is up, read the key (`GET {host}/admin/host/systemkeys/eventgrid_extension` or `blobs_extension`; `func start` answers this without credentials) and subscribe
`{host}/runtime/webhooks/EventGrid?functionName=<name>&code=<key>` (blob trigger: `{host}/runtime/webhooks/blobs?functionName=Host.Functions.<name>&code=<key>`) through
`PUT /_azemu/eventgrid/topics/<topic>/subscriptions/<name>` or `/_azemu/eventgrid/sources/storage/<account>/subscriptions/<name>`. The validation handshake succeeds. The loopback webhook target needs no `--allow-webhook-host`.

**Identity-based connections** (`--identity`): `IDENTITY_ENDPOINT=http://127.0.0.1:PORT/_azemu/identity/msi/token` and `IDENTITY_HEADER=azemu` (then `DefaultAzureCredential` picks the managed identity), plus

| Setting | Value |
|---|---|
| `AzureWebJobsStorage__blobServiceUri`, `__queueServiceUri`, `__tableServiceUri` | `https://myacct.blob.core.localhost:PORT` (and `.queue.`, `.table.`); has to be https because the Azure SDKs do not send a bearer token over http |
| `ServiceBusConnection__fullyQualifiedNamespace` | `myns.servicebus.localhost` **without a port** (a port gives "not a well-formed Service Bus fully qualified namespace") |
| `EventHubConnection__fullyQualifiedNamespace` | `myns.eventhub.localhost` without a port, plus `AzureFunctionsJobHost__extensions__eventHubs__customEndpointAddress=sb://myns.eventhub.localhost:PORT` (the host.json setting `extensions.eventHubs.customEndpointAddress`) |
| `CosmosConnection__accountEndpoint` | `https://myacct.documents.localhost:PORT/` |

Service Bus has no such custom address setting in the host: the SDK dials port 5671 (TLS). `--standard-ports` opens 5671 (and 5672), so inside a container nothing else is needed;
outside one, start the emulator with `--pin servicebus=5671` (TLS works on a pinned port) or `--standard-ports`. With the port-less names the host-name resolves through `*.localhost`.
The classic polling blob trigger also works with these https host-name URLs (`--identity --only blob,blobhost`).

**Queue messages** sent by hand to a queue that a trigger reads must be base64 text (the host's default `messageEncoding`); the output binding and the Azure SDK with `TextBase64EncodePolicy` do that.

## What the test app covers (about a minute for one full run)

| Function | Trigger or binding | Result |
|---|---|---|
| `ping` | HTTP trigger | works |
| `timer` | Timer trigger (singleton lock = blob lease), Blob output | works (fired twice, 5 s apart) |
| `queue_to_blob` | HTTP -> Queue output; Queue trigger -> Blob output | works |
| `queue_poison` | Queue trigger that throws | works: ran 3 times (`maxDequeueCount` 3), message in `q-fail-poison` |
| `blob_poll` | Blob trigger, polling, path-style endpoints | works (also with host-name and identity endpoints: `--storage-style host`, `--identity`) |
| `blob_poll_host` | Blob trigger, polling, host-name style endpoints (group `blobhost`: the host is restarted with them) | works: Blob logging switched on by the host, `$logs` listed, the new blob found in a log blob (with `HOST_LOGLEVEL=Debug` the host log says `Log scan ... found 1 blobs` and `Source: 'LogScan'`) |
| `blob_eventgrid` | Blob trigger, Event Grid source, storage events -> webhook | works (also with host-name and identity endpoints) |
| `eg_trigger`, `http_eg_send` | Event Grid output -> custom topic -> webhook -> Event Grid trigger | works |
| `http_table_put`, `http_table_get` | Table output and input binding | works |
| `sb_queue` | HTTP -> Service Bus output; queue trigger, auto-complete, Blob output | works, queue empty afterwards |
| `sb_topic` | Service Bus topic output; subscription trigger | works |
| `sb_poison` | Service Bus trigger that throws | works: ran 3 times, dead-lettered (`MaxDeliveryCountExceeded`) |
| `eh_batch` | HTTP -> Event Hubs output; Event Hubs trigger (batch of 3), Blob output | works; checkpoint blob written; after a host restart it resumes from the checkpoint (1 run) |
| `cosmos_changefeed` | HTTP -> Cosmos output; Cosmos DB trigger with lease container, Blob output | works; after a restart it resumes from the lease (1 run) |
| `http_cosmos_get` | Cosmos DB input binding | works |
| blob receipts | restart: blobs already processed do not run again | works |
| Durable Functions | chaining, durable timer, fan-out/fan-in (`--app durable`) | works (path-style, host-name and identity endpoints) |
| `nodeQueueToBlob`, `nodeSbQueue` | Node 26 worker, extension bundle 4.39.1 (`--app node`) | works |
| identity-based connections | `--identity`: storage, Service Bus, Event Hubs, Cosmos | works with the settings above |
| `--lenient`, nothing created first | queues, topics, hubs auto-created | works for Storage, Service Bus, Event Hubs (Cosmos is not auto-created: the trigger reports "Both containers must exist") |
| strict (default), nothing created first | | host reports the missing entities as Azure does (`could not be found`) |

`GET /_azemu/unsupported` was empty after a full run (no operation answered 501).
