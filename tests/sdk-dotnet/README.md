# .NET SDK tests for azemu

Tests that drive the emulator with the official Azure SDK for .NET (xUnit, one project: `Azemu.DotnetTests`). They were written from the SDK documentation and
Microsoft's REST documentation, not from the emulator's source. What the .NET SDKs need set to reach the emulator is in [`docs/sdk-notes.md`](../../docs/sdk-notes.md#net).

## What you need

* The .NET 10 SDK, on the `PATH` or in the directory `DOTNET_ROOT` names. Without root: `curl -sSL https://dot.net/v1/dotnet-install.sh -o dotnet-install.sh` then
  `bash dotnet-install.sh --channel 10.0 --install-dir ~/dotnet` and `export DOTNET_ROOT=~/dotnet`.
* NuGet access for the packages in `Azemu.DotnetTests.csproj` (newest stable at the time the project was written).
* A built emulator binary: `cargo build -p azemu` gives `target/debug/azemu`, or use a release binary.

## Run it

```bash
tests/sdk-dotnet/run.sh                          # everything, about 3 minutes
tests/sdk-dotnet/run.sh --filter "FullyQualifiedName~ServiceBus"      # any `dotnet test` arguments
tests/sdk-dotnet/run.sh --refresh                # copy target/debug/azemu to .azemu-under-test first
tests/sdk-dotnet/run.sh --findings               # also run the tests skipped as known differences
AZEMU_BIN=/path/to/azemu tests/sdk-dotnet/run.sh # another emulator binary
```

`run.sh` builds with one build node (`-m:1 -nodeReuse:false`), shuts the build servers down, then runs the tests with a workstation GC and a 768 MB heap limit. xUnit runs one
test class at a time (`xunit.runner.json`). The emulator under test is a copy of `target/debug/azemu` in `tests/sdk-dotnet/.azemu-under-test` (made on the first run, or with
`--refresh`) unless `AZEMU_BIN` names another. Every test class starts its own emulator on a free port with a temporary certificate directory and `HOME` pointing at it (nothing is
written to `~/.azemu`), and stops it at the end. The emulator's log (debug level) of the last run of each class is in `logs/<name>.log`.

A test skipped with a reason that starts with `azemu:` shows a known difference from Azure and runs only with `--findings`. The tests that need an emulator started with auto-creation
on, or off, are marked and skipped when the emulator does not match.

## Entities are created by the tests

The emulator does not create queues, topics, event hubs, consumer groups or Event Grid topics because a client names them, as Azure does not (`AZEMU_SERVICEBUS_AUTOCREATE`,
`AZEMU_EVENTHUBS_AUTOCREATE`, `AZEMU_EVENTGRID_AUTOCREATE`, or `AZEMU_LENIENT=1` for all, turn it on). The tests create what they use: queues through `ServiceBusAdministrationClient`
(`NewQueue` in `SbBase`), event hubs through the admin API (`CreateHub`). Blob versioning is off until someone turns it on: `NewVersionedContainer()` in `Storage/StorageBase.cs`
makes a storage account through the management API with `IsVersioningEnabled = true`.

## How the clients trust the emulator

The emulator's CA signs only for `localhost` names. The tests do not touch `SSL_CERT_FILE`; each client gets the CA through its own setting (`Infra/Emu.cs`):

* HTTP clients: an `HttpClientTransport` over a `SocketsHttpHandler` whose certificate validation callback builds a chain with the CA as the only root
  (`X509ChainPolicy.CustomTrustStore`, `TrustMode = CustomRootTrust`). Cosmos takes it through `HttpClientFactory`.
* AMQP clients (Service Bus, Event Hubs): `CertificateValidationCallback` in the client options. Plain AMQP uses `UseDevelopmentEmulator=true` and needs no certificate.
* Tokens: a small `TokenCredential` that returns a fixed string (the emulator checks only that a bearer token is present), or the real `ManagedIdentityCredential` /
  `ClientSecretCredential` against the emulator's token endpoints (`Identity/`).

Helpers in `Infra/`: `Emu` (process, ports, CA), `Finding` (the skip attributes), `Wait`, `SdkLog` (the SDK's event listener written to `logs/`), `WireTap` (a TCP relay that records bytes),
`CapturePolicy`.

## Coverage

| Area | What |
|---|---|
| Service Bus (`ServiceBus/`) | administration of queues, topics, subscriptions and rules; send, receive, peek, settlement, delivery count and dead-lettering, lock expiry and renewal, TTL, duplicate detection, forwarding, scheduling, batches; topics with SQL, correlation and boolean filters; `ServiceBusRuleManager`; sessions; `ServiceBusProcessor`; transactions with `TransactionScope`; AMQP over TCP (plain and TLS) and WebSockets |
| Event Hubs (`EventHubs/`) | producer (batches, partition key and id, buffered producer), consumer from every position, consumer groups, owner level, `PartitionReceiver`, `EventProcessorClient` with the blob checkpoint store, WebSockets, TLS with a token credential |
| Cosmos DB (`Cosmos/`) | databases, containers, items, ETags, stream APIs, patch, hierarchical partition keys, delete by partition key, queries (LINQ, cross-partition, ORDER BY, aggregates, GROUP BY, paging), change feed (pull model, feed ranges, all versions and deletes, processor), transactional batch and bulk, scripts, direct mode |
| Blob, Data Lake, Queue, Table (`Storage/`) | blob lifecycle, parallel upload and ranged download, append and page blobs, leases, snapshots, versions, tags, soft delete, copy, SAS (service, container, account, stored policy, user delegation), batch; Data Lake paths, rename, ACLs; queues; tables (EDM types, LINQ filters, transactions, SAS); `--pin` ports with Azurite-style connection strings |
| Key Vault, App Configuration, Event Grid, Identity, ARM | secrets, keys, certificates; settings, snapshots, feature flags; Event Grid and CloudEvents, namespace topics; managed identity and client secret credentials; `ArmClient` for resource groups, storage and Service Bus, a template deployment |
