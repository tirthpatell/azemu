# azemu tests with the Azure SDK for Java

Tests that drive the emulator with the official `com.azure:*` libraries. They exercise what the other suites cannot: the Netty HTTP client (its own pooling,
chunking and `Expect: 100-continue`), `qpid-proton-j` (the AMQP 1.0 library under Service Bus and Event Hubs, and the only SDK with Service Bus transactions), the
Cosmos client with its own query planning and bulk executor, and the `azure-resourcemanager` fluent API. What the Java SDKs need set to reach the emulator is in
[`docs/sdk-notes.md`](../../docs/sdk-notes.md#java).

## What you need

* A JDK 17 or newer (`maven.compiler.release` is 17).
* [Apache Maven](https://maven.apache.org/) 3.9 or newer on the `PATH`, or the path to it in `MVN`. Maven Central is the only network access (about 700 MB in `~/.m2`).
* A built emulator binary: `cargo build -p azemu` gives `target/debug/azemu`, or use a release binary.

## Run

```bash
cd tests/sdk-java
./run.sh                                  # everything, one test JVM (-Xmx1g), about 4 minutes
./run.sh -Dtest=ServiceBusTest            # one class (several: -Dtest='ServiceBus*,EventHubs*')
./run.sh -Dtest='ServiceBusTest#abandon*' # tests by name
AZEMU_BIN=/path/to/azemu ./run.sh         # another emulator binary
PN_TRACE_FRM=1 ./run.sh -Dtest=...        # qpid-proton-j prints every AMQP frame the SDK sends and receives
AZURE_HTTP_LOG_DETAIL_LEVEL=body_and_headers ./run.sh -Dslf4j.level=info -Dtest=...   # the SDK's own HTTP log
./run.sh -DtrimStackTrace=false ...       # full stack traces
```

* Without `AZEMU_BIN`, `run.sh` uses `.azemu-under-test`, a copy of `../../target/debug/azemu` that it makes the first time (refresh it by deleting it). Set `MVN` to use a
  Maven that is not on the `PATH`.
* Every test class starts its own emulator (`@EmuTest`, `Emu.java`): a free port, a temporary `--cert-dir`, `--log-level debug`, the log in `target/emu-logs/{Class}.log`.
  It stops the emulator after the class. `@EmuTest(env = {"AZEMU_SERVICEBUS_AUTOCREATE=0"})` sets variables.
* TLS: the first emulator of the JVM builds a trust store (the JDK's `cacerts` plus the emulator's `ca.pem`) and sets `javax.net.ssl.trustStore` before any client exists.
  `SSL_CERT_FILE` is never touched.
* Names are unique per test (`Emu.name`, `Emu.dashed`); Service Bus, Event Hubs, Cosmos and ARM tests use a fresh namespace or account per class.
* Temporary files go to `target/tmp`.
* A test that shows a known difference from Azure is marked `@Disabled("azemu: ...")` and skipped; `./run.sh --findings` runs those too. There are none at present.
  A test skipped by an aborted assumption (for example `Cos.assumeGatewayRoutingWorks`) says why in the report.

## The tests create what they use

The emulator does not create queues, topics, event hubs, consumer groups or Event Grid topics because a client names them, as Azure does not (`AZEMU_SERVICEBUS_AUTOCREATE`,
`AZEMU_EVENTHUBS_AUTOCREATE`, `AZEMU_EVENTGRID_AUTOCREATE`, or `AZEMU_LENIENT=1` for all, turn it on). The tests create what they use: queues through
`ServiceBusAdministrationClient`, event hubs and consumer groups through the admin API (`Eh.newHub`). `NoAutoCreateTest` checks what Azure answers for something missing
and pins `AZEMU_*_AUTOCREATE=0`; `KafkaAutoCreateTest` is about auto-creation itself and starts its emulator with `AZEMU_EVENTHUBS_AUTOCREATE=1`.
Blob versioning is off until someone turns it on: `Versioning.account` makes a storage account through the management API and switches it on.

## Libraries

Versions come from `azure-sdk-bom` and are pinned in `pom.xml`: `azure-identity`, `azure-storage-blob` (and `-batch`), `azure-storage-file-datalake`, `azure-storage-queue`,
`azure-data-tables`, `azure-security-keyvault-secrets` / `-keys` / `-certificates`, `azure-data-appconfiguration`, `azure-cosmos`, `azure-messaging-servicebus`,
`azure-messaging-eventhubs` (and `-checkpointstore-blob`), `azure-messaging-eventgrid` / `-eventgrid-namespaces`, `azure-resourcemanager-resources` / `-storage` /
`-servicebus`, JUnit Jupiter 5.12.

## Coverage

| Class | What it covers |
|---|---|
| `BlobTest`, `BlobSasBatchAsyncTest`, `BlobQueryTest` | containers, block/append/page blobs, leases, snapshots, versions, soft delete, tags, tiers, copy, SAS (account, service, user delegation), 64 MB Netty uploads, batch delete and set tier, quick query, async clients |
| `DataLakeTest` | file systems, directories, rename, ACLs, append and flush, parallel upload, recursive delete |
| `QueueTest`, `TableTest` | queues and messages (visibility, update, peek, TTL, delayed, base64 option, SAS); tables, all EDM types, filters and paging, upsert/merge/replace, ETags, transactions, SAS |
| `ServiceBusTest`, `ServiceBusSessionsTopicsTest`, `ServiceBusProcessorTest` | send and receive with every property, batches, peek-lock settlement, delivery count, lock expiry and renewal, scheduled messages, TTL and dead-lettering, duplicate detection, forwarding, sessions, topics, subscriptions, rules, the administration client, the processor clients, AMQP over WebSocket |
| `ServiceBusTransactionTest`, `ServiceBusTransactionWireTest` | transactions (commit, rollback, across two queues) and what the SDK sends for them, seen by a small AMQP server (`MockAmqpServer`) |
| `EventHubsTest`, `EventHubsProcessorTest`, `KafkaTest` | producer and consumer clients, positions, consumer groups, owner level, runtime metrics, WebSocket, `EventProcessorClient` with `BlobCheckpointStore`, a Kafka client |
| `NoAutoCreateTest`, `KafkaAutoCreateTest` | what Azure answers for a missing entity, and auto-creation |
| `CosmosTest`, `CosmosQueryTest`, `CosmosStartupTest` | databases, containers, items, ETags, patch, TTL, hierarchical keys, readMany, queries (parameters, cross partition, ORDER BY, aggregates, GROUP BY, paging), transactional batch, bulk, change feed, direct mode (what happens), sync and async clients |
| `KeyVaultTest`, `AppConfigTest`, `EventGridTest`, `ArmTest`, `IdentityTest` | secrets, keys, certificates; settings, snapshots, feature flags; Event Grid and CloudEvents; resource groups, storage, Service Bus through `azure-resourcemanager`; `ManagedIdentityCredential`, `DefaultAzureCredential`, `ClientSecretCredential` |

## Reading the results

Surefire prints one line per class. A skipped test is one that did not apply: an aborted assumption names its reason, and `@Disabled("azemu: ...")` marks a known difference.
Nothing should fail against a correct build of the emulator.
