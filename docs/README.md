# Documentation

The front page is [`../README.md`](../README.md): read that first. This directory is the reference.

## Using it

| Document | What it tells you |
|---|---|
| [`install.md`](install.md) | the image, the binary, building from source, supported platforms |
| [`reaching-the-emulator.md`](reaching-the-emulator.md) | host names, standard ports, containers, CI, https and the certificate authority |
| [`rust.md`](rust.md) | for a Rust team: the binary, which crates and versions per service, a client for each, services with no crate, tests with `cargo test`, CI |
| [`sdk-notes.md`](sdk-notes.md) | what each SDK needs set to talk to the emulator, and what the SDKs do that surprises people |
| [`SETTINGS.md`](SETTINGS.md) | every `AZEMU_*` setting with its default (generated from the code) |
| [`../SECURITY.md`](../SECURITY.md) | who is trusted, what it defends against, what is on disk, how to report a problem |
| [`../examples/README.md`](../examples/README.md) | a Compose project, CI arrangements, and a small Rust project with tests |
| [`../CHANGELOG.md`](../CHANGELOG.md) | what changed between versions |

One page per service: what it emulates, how to reach it and what is not built, then the operations, the known differences from Azure, and the behaviour that could not be
confirmed against Azure.

| Service | Document |
|---|---|
| Blob Storage | [`services/blob.md`](services/blob.md); quick query: [`services/blob-query.md`](services/blob-query.md) |
| Data Lake Storage Gen2 | [`services/datalake.md`](services/datalake.md) |
| Queue Storage | [`services/queue.md`](services/queue.md) |
| Table Storage | [`services/table.md`](services/table.md) |
| Key Vault | [`services/keyvault.md`](services/keyvault.md) |
| App Configuration | [`services/appconfig.md`](services/appconfig.md) |
| Cosmos DB | [`services/cosmos.md`](services/cosmos.md); the query language: [`services/cosmos-sql.md`](services/cosmos-sql.md); stored procedures, triggers, functions: [`services/cosmos-scripts.md`](services/cosmos-scripts.md) |
| Service Bus | [`services/servicebus.md`](services/servicebus.md); AMQP over WebSocket: [`services/websocket.md`](services/websocket.md) |
| Event Hubs | [`services/eventhubs.md`](services/eventhubs.md); the Kafka endpoint: [`services/kafka.md`](services/kafka.md) |
| Event Grid | [`services/eventgrid.md`](services/eventgrid.md); events of blobs, Data Lake, Key Vault and App Configuration: [`services/storage-events.md`](services/storage-events.md) |
| Identity (token endpoints) | [`services/identity.md`](services/identity.md) |
| Resource Manager | [`services/arm.md`](services/arm.md) |
| Keeping data between runs (`--data-dir`) | [`services/persistence.md`](services/persistence.md) |

## How it is checked

| Document | What it tells you |
|---|---|
| [`../tests/README.md`](../tests/README.md) | the SDK suites, the Azure Functions, Terraform and container tests, and how to run them |
| [`../fidelity/SCORES.md`](../fidelity/SCORES.md) | the replay of Microsoft's recordings, per service: the scores and the most frequent differences |
| [`../tools/replay/README.md`](../tools/replay/README.md) | what the replay tool compares, which differences it excuses and why |

## Working on it

| Document | What it tells you |
|---|---|
| [`../CONTRIBUTING.md`](../CONTRIBUTING.md) | how to build and test, and what a change must come with |
| [`releasing.md`](releasing.md) | the release build, the image, cutting a release |
