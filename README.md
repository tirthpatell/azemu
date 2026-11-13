<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/logo-dark.svg">
    <img src="docs/assets/logo-light.svg" alt="azemu" width="300">
  </picture>
</p>

# azemu

A local emulator of Azure services, in one program. Point your Azure SDK at it and write tests against Blob, Queue, Table, Key Vault, App
Configuration, Cosmos DB, Service Bus, Event Hubs, Event Grid and Resource Manager without an Azure account, on your machine or in CI. It is for building
and testing code. It is not for production, and it is not a place to test security: it accepts every credential. State is in memory, or in a data directory
if you ask. It starts in about 10 ms and uses about 15 MB when idle (release build).

azemu is an independent open-source project. It is not made, sponsored or endorsed by Microsoft. Microsoft, Azure, Azure Cosmos DB, Azure Event Hubs, Azure Service Bus, Azure Key Vault and Microsoft Entra are trademarks of the Microsoft group of companies; Apache Kafka is a trademark of the Apache Software Foundation; Terraform is a trademark of HashiCorp. The names are used here only to say what azemu works with.

More detail is under [`docs/`](docs/README.md). What it cannot tell you is listed near the end of this page: read that part before you rely on a green test.

**Contents:** [Start it](#start-it) · [A first request](#a-first-request) (Python, [Rust](#rust)) · [What is emulated](#what-is-emulated) · [Reaching the emulator](#reaching-the-emulator) ·
[Behaves like Azure by default](#behaves-like-azure-by-default) · [Using it in tests](#using-it-in-tests) · [Works with](#works-with) · [SDK notes](docs/sdk-notes.md) ·
[How it is checked](#how-it-is-checked) · [How it was made](#how-it-was-made) · [What it cannot tell you](#what-it-cannot-tell-you) · [Security](#security) · [Options](#options) ·
[Build, test, contribute, licence](#build-test-contribute-licence)

## Start it

The first release has not been published yet. When it is, the container image will be on the GitHub Container Registry and the binaries on the GitHub release page
([`docs/install.md`](docs/install.md) has the details):

```bash
docker run --rm -p 127.0.0.1:4577:4577 -p 127.0.0.1:10000:10000 ghcr.io/tirthpatell/azemu:latest     # not published yet
```

What works today is to build it yourself. With Docker (about 3 minutes):

```bash
docker build -t azemu:latest .
docker run --rm -p 127.0.0.1:4577:4577 -p 127.0.0.1:10000:10000 azemu:latest
```

Or from source: Rust 1.91 or newer and a C compiler (no OpenSSL, no cmake). `cargo build --release -p azemu`, then `target/release/azemu`. `azemu --version` prints the
version and the commit it was built from. Platforms and details: [`docs/install.md`](docs/install.md).

When it is ready it prints one line to standard output:

```
azemu ready gateway=http://127.0.0.1:4577 ca=/home/me/.azemu/ca.pem services=blob,dfs,queue,table,keyvault,appconfig,cosmos,eventgrid,identity,servicebus,eventhubs,kafka,arm version=0.1.0 commit=unknown
```

`gateway` is the address of the main port (4577), `ca` is the certificate authority for https (see below), `services` is what is mounted. Logs go to standard
error. Stop it with Ctrl-C or SIGTERM. **Publish a container's ports to `127.0.0.1` only**, as above: the emulator has no sign-in (see "Security").

## A first request

Blob, with the Python SDK or the Azure SDK for Rust. Start the emulator first (above): `azemu`, or the container.

### Python

`pip install azure-storage-blob`:

```python
from azure.storage.blob import BlobServiceClient

# Host-name form: the account "myacct" is the first part of the host name. Any key works.
conn = ("DefaultEndpointsProtocol=http;AccountName=myacct;AccountKey=bm90LWEta2V5;"
        "BlobEndpoint=http://myacct.blob.core.localhost:4577;")
container = BlobServiceClient.from_connection_string(conn).create_container("demo")
container.upload_blob("hello.txt", b"hello")
print(container.download_blob("hello.txt").readall())      # b'hello'
```

Or, after starting with `--standard-ports` (the Docker image already does), the Azurite address, which works anywhere:

```python
BlobServiceClient.from_connection_string("UseDevelopmentStorage=true")      # http://127.0.0.1:10000/devstoreaccount1
```

The host-name form works where the system resolves any name under `.localhost` to your machine: a Linux desktop. It does not work inside most containers,
on Windows, or on macOS before 26. The standard-port form works everywhere. Details: "Reaching the emulator".

### Rust

In a new project (`cargo new hello && cd hello`): `cargo add azure_core azure_identity azure_storage_blob`, and `cargo add tokio --features macros,rt-multi-thread`. The Rust crates differ from the Python example in three ways, and each of them stops a first program:

1. **Use https.** The Azure SDK for Rust sends a token and refuses to do so over plain http (`... must use https`). The emulator answers https on the same port.
2. **Use a credential that does not need the Azure CLI.** `DeveloperToolsCredential`, which the crates' own READMEs use, runs the `az` program (`az not found on PATH`). `ManagedIdentityCredential`
   asks the emulator for a token when you set two variables (below). Rust has no `DefaultAzureCredential`.
3. **Trust the emulator's certificate authority**, for this one program: put the system's certificate list and the emulator's `ca.pem` in one file and point `SSL_CERT_FILE` at it (below; the Rust SDK reads it).
   **Without it the SDK does not fail at once**: it retries the certificate error for more than a minute (81 seconds when tried) before it reports `InvalidCertificate(UnknownIssuer)`, and the emulator
   logs nothing. `RetryOptions::none()` in the client options, as in the program below, makes it fail in milliseconds.

```bash
cat /etc/ssl/certs/ca-certificates.crt ~/.azemu/ca.pem > ~/.azemu/bundle.pem     # Linux; the ready line says where ca.pem is
export SSL_CERT_FILE=~/.azemu/bundle.pem
export IDENTITY_ENDPOINT=http://localhost:4577/_azemu/identity/msi/token IDENTITY_HEADER=azemu
cargo run                                     # with the program below in src/main.rs
```

```rust
use azure_core::http::{ClientOptions, RequestContent, RetryOptions, Url};
use azure_identity::ManagedIdentityCredential;
use azure_storage_blob::{BlobServiceClient, BlobServiceClientOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Reads IDENTITY_ENDPOINT and IDENTITY_HEADER: no `az` program, no sign-in.
    let credential = ManagedIdentityCredential::new(None)?;
    // No retries: a certificate problem then fails at once, not after more than a minute.
    let options = BlobServiceClientOptions {
        client_options: ClientOptions { retry: RetryOptions::none(), ..Default::default() },
        ..Default::default()
    };
    let url = Url::parse("https://myacct.blob.core.localhost:4577/")?;
    let blobs = BlobServiceClient::new(url, Some(credential), Some(options))?;

    let container = blobs.blob_container_client("demo");
    container.create(None).await?;
    let blob = container.blob_client("hello.txt");
    blob.upload(RequestContent::from(b"hello".to_vec()), None).await?;
    let body = blob.download(None).await?.body.collect().await?;
    println!("{}", String::from_utf8(body.to_vec())?); // hello
    Ok(())
}
```

Started the emulator in a container? Fetch its authority first: `mkdir -p ~/.azemu && curl -s localhost:4577/_azemu/ca.pem > ~/.azemu/ca.pem`.
A second run says `409 ContainerAlreadyExists`, as Azure would (`curl -X POST localhost:4577/_azemu/reset` empties the emulator). Which crates and versions, the other services (Queue, Key Vault, Cosmos DB, Event Hubs, Service Bus), tests with `cargo test`
and CI: [`docs/rust.md`](docs/rust.md). A small project with tests: [`examples/rust/`](examples/rust/README.md).

## What is emulated

Standard ports are those of `--standard-ports`. Every service has a page under `docs/services/` with the operations, the known differences from Azure and the
behaviour that could not be confirmed against Azure.

| Service | Reach it (gateway host, standard port) | What is covered | What is not |
|---|---|---|---|
| [Blob](docs/services/blob.md) | `{acct}.blob.core.localhost`, 10000 | containers, block, append and page blobs, leases, snapshots, tags, SAS, batch, soft delete, versions (off until turned on), immutability, quick query (CSV, JSON) | rename container, incremental copy, Parquet and Arrow, encryption, a static website endpoint |
| [Data Lake Gen2](docs/services/datalake.md) | `{acct}.dfs.core.localhost`, 10003 | filesystems, directories, rename, ACLs, same data as Blob | container sessions |
| [Queue](docs/services/queue.md) | `{acct}.queue.core.localhost`, 10001 | all message operations, visibility, SAS | quotas |
| [Table](docs/services/table.md) | `{acct}.table.core.localhost`, 10002 | `$filter`, batches, SAS | the Cosmos Table API |
| [Key Vault](docs/services/keyvault.md) | `{vault}.vault.localhost`, 10010 (https) | secrets, keys with real cryptography, certificates, soft delete | key release and attestation, HSM administration, symmetric key operations |
| [App Configuration](docs/services/appconfig.md) | `{store}.azconfig.localhost`, 10011 (https) | key-values, labels, revisions, locks, snapshots | throttling |
| [Cosmos DB](docs/services/cosmos.md) (NoSQL API, gateway mode) | `{acct}.documents.localhost`, 8081 (https) | queries, patch, batch and bulk, change feed, TTL, stored procedures, triggers, functions | direct mode, vector and full-text search, 429, consistency levels |
| [Service Bus](docs/services/servicebus.md) (AMQP) | `{ns}.servicebus.localhost`, 5672 | queues, topics, rules, sessions, dead-lettering, scheduling, transactions, management API, AMQP over WebSocket | authentication, partitioned entities |
| [Event Hubs](docs/services/eventhubs.md) (AMQP and [Kafka](docs/services/kafka.md)) | `{ns}.eventhub.localhost`, 5672 (Kafka 9093) | send, partition keys, consumer groups, checkpoints in Blob, Kafka producers and consumers | Schema Registry, Capture |
| [Event Grid](docs/services/eventgrid.md) | `{topic}.eventgrid.localhost`, 10012 (https) | publish, pull, push to webhooks, Storage queues, Service Bus, event hubs; [blob events](docs/services/storage-events.md) | partner topics in ARM, blob dead-letter |
| [Identity](docs/services/identity.md) | `login.localhost` or `/_azemu/identity`, 10014 | token endpoints for `DefaultAzureCredential` and the Entra flows | validating anything |
| [Resource Manager](docs/services/arm.md) | `management.localhost`, 10013 (https) | resource groups, deployments, templates, the resource types above | every other resource type is stored only (below) |

Any account, vault, store or namespace name is a separate account, made on first use. Anything below the account is not (see "Behaves like Azure").

## Reaching the emulator

Three ways. The first is the default; the other two exist because the first needs the system to resolve names under `.localhost`.

| Way | Example | Works where |
|---|---|---|
| Names under `.localhost` on the gateway port | `http://myacct.blob.core.localhost:4577` | a Linux desktop; not in most containers, on Windows, or on macOS before 26 |
| `--standard-ports` | `http://azemu:10000/devstoreaccount1`, `https://azemu:10010` | everywhere; the port picks the service, any host name works |
| `--pin blob=10000` (repeatable) | the same, for the services you name | everywhere |

- **Another container:** use the service name and the standard ports, and tell the emulator its name for https: `AZEMU_TLS_NAME=azemu`. Run the tested example
  with `cd examples/compose && docker compose up --build --abort-on-container-exit --exit-code-from client`. **CI:** `examples/ci/` has a GitLab and a GitHub
  Actions arrangement. The port table, both examples and a hosts-file recipe for real Azure names are in
  [`docs/reaching-the-emulator.md`](docs/reaching-the-emulator.md).
- **https:** Azure SDKs refuse to send a token over plain http, and Key Vault, App Configuration, Cosmos DB and Event Grid are https services, so most clients need https. The emulator makes
  its own certificate authority at the first start (`ca.pem`, also served at `GET /_azemu/ca.pem` on any port). It can sign only for `localhost`, names under
  `.localhost` and loopback addresses; `--tls-name <suffix>` (or `AZEMU_TLS_NAME`) adds a name, such as the Compose service name. Make **one client** trust it
  and do not replace the system's list (`SSL_CERT_FILE` and `REQUESTS_CA_BUNDLE` replace it for the whole shell):

| Client | How (all tried except where marked) |
|---|---|
| Python Azure SDKs | `connection_verify="/path/ca.pem"` on the client. Cosmos ignores a CA path there: use `connection_verify=False` |
| Node | `NODE_EXTRA_CA_CERTS=/path/ca.pem` (adds to the system list) |
| curl | `--cacert /path/ca.pem` |
| Java | a copy of the JDK's `cacerts` with the CA added (`keytool -importcert -noprompt -alias azemu -file ca.pem -keystore my-cacerts -storepass changeit`), then `-Djavax.net.ssl.trustStore=my-cacerts -Djavax.net.ssl.trustStorePassword=changeit` |
| Go, Rust, Terraform, and tools that read one file | `SSL_CERT_FILE` pointing at a bundle: the system list plus `ca.pem` (`cat /etc/ssl/certs/ca-certificates.crt ca.pem > bundle.pem`), for that one process. Go and Rust tried (Rust: [`docs/rust.md`](docs/rust.md); without it the Rust SDK retries the certificate error for over a minute before it reports it); .NET not tried |

## Behaves like Azure by default

Nothing below the account comes into being by being named, because on Azure it does not, and a forgotten queue is one of the bugs an emulator should catch.
A send to a queue, topic or event hub nobody created, a reader on a consumer group nobody created, a publish to an Event Grid topic nobody created: all fail
as they do on Azure (tried for a Service Bus queue, a Storage queue and an Event Grid topic). Create what a test needs first, the way a team does against Azure:

- with the SDK: Blob, Queue and Table create calls; the Service Bus administration client (`ServiceBusAdministrationClient.create_queue`);
- with Resource Manager: Terraform, Bicep templates or the management SDKs (below);
- for Event Hubs and Event Grid, whose SDK administration clients cannot create anything here, with the emulator's own admin API
  (`curl -X PUT localhost:4577/_azemu/eventhubs/myns/hubs/orders -d '{"partitionCount":2}'`; tried) or Resource Manager;
- from a file, loaded at start and again after every reset: `AZEMU_SERVICEBUS_CONFIG`, `AZEMU_EVENTHUBS_CONFIG`, `AZEMU_EVENTGRID_CONFIG`.

**Blob versioning is off** for a storage account until it is turned on (`isVersioningEnabled` through Resource Manager), as on Azure.

**`--lenient`** (or `AZEMU_LENIENT=1`) turns on the conveniences Azure does not have: queues, topics, subscriptions, event hubs, consumer groups and Event Grid topics
that a client names come into being on first use, and new storage accounts keep blob versions. Each of these also has a variable of its own that wins over
`--lenient` (`AZEMU_SERVICEBUS_AUTOCREATE`, `AZEMU_EVENTHUBS_AUTOCREATE`, `AZEMU_EVENTGRID_AUTOCREATE`, `AZEMU_BLOB_VERSIONING`). Cosmos databases and containers
are never made for you.

**Credentials are checked loosely.** Precisely:

- A bearer token is never decoded or checked: any token passes. A request with no credentials, where Azure would refuse one, is refused (Blob 401, Queue and Table
  403, Key Vault, Cosmos, App Configuration, Event Grid and Resource Manager 401). Blob has three more answers, as Azure does: an anonymous read in an account
  that does not allow public access is 409 `PublicAccessNotPermitted`, an anonymous read of a container that does not exist is 404, and an `Authorization`
  header that cannot be read is 400 `InvalidAuthenticationInfo` ([details](docs/services/blob.md)).
- Storage keys and SAS signatures are **verified only for the well-known development account** `devstoreaccount1`. For any other account the key is invented by the
  emulator, so a wrong key, a changed signature, or a SAS made for another blob is accepted.
- A SAS is checked for expiry, start time, permissions, resource type and protocol for **every** account.
- Service Bus, Event Hubs and Kafka accept every credential and mechanism. Event Grid needs a key or token but does not look at it. Cosmos and App Configuration
  accept any key (the Rust Cosmos SDK checks that the key is valid base64 before it sends anything: use the well-known emulator key in [`docs/services/cosmos.md`](docs/services/cosmos.md#using-it)).
- No role assignments exist, so a missing role, a wrong key or a wrong tenant never fails here.

## Using it in tests

- **Reset between tests.** `POST /_azemu/reset` empties everything. `?service=queue` empties one service, `?account=myacct` one account in every service that has
  accounts (both together are allowed; the answer lists `reset` and `notSupported`; an unknown parameter or service is a 400). Resource Manager's "account" is a
  subscription id. Tried: one blob account emptied, the other kept.
- **Parallel runs** can share one emulator if each uses its own account names (any name is a fresh account) and resets only its own.
- **Keep data between runs:** `--data-dir ~/.azemu-data`. It is saved every 15 seconds when something changed (`--save-interval`), on `POST /_azemu/save`, and on Ctrl-C
  and SIGTERM (and so on `docker stop`). **A crash loses what changed since the last save**, and services are saved at different moments, so two can be a few seconds
  apart. The files are not encrypted: Key Vault secrets and keys, and the identity signing key, are in them (modes 0700 and 0600). One emulator uses a directory at a
  time. Tried: blob, queue and table data survived a restart. [`docs/services/persistence.md`](docs/services/persistence.md).
- **Did a test use something that is not emulated?** An operation that is not built answers `501` and says which. `GET /_azemu/unsupported` lists them with counts, by
  service and operation (`DELETE` empties the list; a summary is logged at shutdown, tried). Resource Manager *stores* a resource type that no provider emulates
  (`Microsoft.Web/sites`) and answers `Succeeded`, though nothing exists behind it: those answers carry `x-azemu-stored-only: true`, one warning is logged per type,
  they appear under `storedOnly` in the same list, and `AZEMU_ARM_STRICT=1` makes them answer 501. Tried all four.
- **See what is running:** `GET /_azemu/health` (version, commit, services), `GET /_azemu/config` (the settings in force, where each came from, and any `AZEMU_*`
  variable nobody reads: a typing mistake is also warned about at start).
- **The log** is one line per request on standard error. At `--log-level debug` it adds the query and headers, with `Authorization`, SAS signatures and other
  credentials replaced by `<redacted>` (tried). A request refused because it carries no credentials is logged, at the default level, with the reason (`refused PUT /c1 (restype=container): no Authorization header and no SAS in the query: the Blob SDK was probably built without a credential`; tried).

## Works with

- **`DefaultAzureCredential`, no code change** (tried with Python; the JavaScript suite works the same way):

  ```bash
  export IDENTITY_ENDPOINT=http://localhost:4577/_azemu/identity/msi/token
  export IDENTITY_HEADER=azemu
  ```

  ```python
  cred = DefaultAzureCredential()
  blobs = BlobServiceClient("https://myacct.blob.core.localhost:4577", credential=cred, connection_verify="/path/ca.pem")
  secrets = SecretClient("https://myvault.vault.localhost:4577", credential=cred, connection_verify="/path/ca.pem")
  ```

  Key Vault needs no extra switch for this host. A vault reached by a name with a single label (`https://azemu:10010`) still needs the client's challenge switch
  (`verify_challenge_resource=False` in Python, `verify_challenge_resource: Some(false)` in the options of the Rust clients). Rust has no `DefaultAzureCredential`:
  `azure_identity::ManagedIdentityCredential::new(None)` reads the same two variables (tried: [Rust](#rust) above). The other credential types: `docs/services/identity.md`.
- **Terraform `azurerm`** and the management SDKs, through Resource Manager at `https://management.localhost:4577`. Resources made there exist in the services behind
  them. Provider block (from `tests/terraform/provider.tf`; Terraform 1.17.1 with azurerm 4.81.0; run `terraform apply` with `SSL_CERT_FILE` set to a bundle):

  ```hcl
  provider "azurerm" {
    features {}
    metadata_host                   = "management.localhost:4577"
    subscription_id                 = "11111111-2222-3333-4444-555555555555"   # any id works
    tenant_id                       = "22222222-2222-2222-2222-222222222222"
    client_id                       = "33333333-3333-3333-3333-333333333333"
    client_secret                   = "anything"
    resource_provider_registrations = "none"
    storage_use_azuread             = false
  }
  ```

  Eight configurations (storage, Service Bus, Event Hubs, Key Vault, App Configuration, Cosmos DB, Event Grid, a template deployment) pass apply, a clean second plan and
  destroy (`tests/terraform`). It is slow (the provider polls: about 2 minutes for a Key Vault), not the emulator. Which types work: [`docs/services/arm.md`](docs/services/arm.md).
- **Azure Functions** (Core Tools 4.16, .NET isolated and Node workers): a test app with a timer, queue, blob, Table, Service Bus, Event Hubs, Cosmos change feed,
  Event Grid and Durable Functions, and identity-based connections, ran against it and every function worked. The classic (polling) blob trigger works with either
  storage style, path style is faster; Cosmos needs gateway mode. Settings per extension: [`tests/functions/README.md`](tests/functions/README.md).
- **Kafka clients**, on the gateway or on port 9093 (tried with librdkafka: a message written and read back, TLS with SASL PLAIN on the gateway, plain on 9093):
  `bootstrap.servers=myns.eventhub.localhost:4577`, `security.protocol=SASL_SSL`, `ssl.ca.location=/path/ca.pem`, `sasl.mechanism=PLAIN`, `sasl.username=$ConnectionString`,
  and the connection string as the password. The event hub (the topic) must exist first. No transactions or compaction. Full settings: `docs/services/kafka.md`.
- **As a stand-in for Azurite and for Microsoft's Cosmos DB emulator**, on their usual ports: `azemu --pin blob=10000 --pin queue=10001 --pin table=10002 --pin cosmos=8081`
  (or `--standard-ports`). Tried: `UseDevelopmentStorage=true` for Blob, Queue and Table, and `CosmosClient("https://localhost:8081", key, connection_verify=False)`.

## How it is checked

- **SDK test suites in six languages** (Python, .NET, JavaScript, Java, Go, Rust), written from the SDKs and Microsoft's documentation without reading the emulator's
  code. `tests/run_all.sh` runs them all; [`tests/`](tests/README.md) says how to run each.
- **A replay of Microsoft's own test recordings** (real requests and real Azure answers, from the SDKs' recorded tests): `tools/replay` sends each recorded request
  and compares the answer. It counts a request as matching when the **status and the error code** agree, and separately counts the **values** in headers and bodies that
  differ (leaving out what a server makes up: dates, ids, ETags). Scores are in [`fidelity/SCORES.md`](fidelity/SCORES.md). Read them as "how often the emulator
  answers as Azure did in the cases Microsoft recorded". They say nothing about services without recordings (Cosmos, Event Hubs, Kafka, the AMQP side of Service Bus),
  and the recordings are of correct use more than of failure.
- **Real clients of other kinds:** the Azure Functions host ([`tests/functions`](tests/functions/README.md)), Terraform ([`tests/terraform`](tests/terraform/README.md)) and a
  container test ([`tests/container`](tests/container/README.md)).
- **The emulator's own tests:** about 2,400 unit and integration tests in the crates (`cargo test --workspace`).

## How it was made

Most of the code and the tests were written by AI coding agents, directed and reviewed by a person, over a few days.

It has been tested extensively, on the developer's machine and without an Azure account: several thousand tests through the official Azure SDKs in six languages
(Python, .NET, JavaScript, Java, Go and Rust), the Azure Functions host, Terraform's `azurerm` provider, real Kafka clients, and a replay of Microsoft's own
recordings of real Azure traffic, in which its answers are compared with the ones Azure gave. "How it is checked" above says what each of these covers, and all of them
are in this repository to run again.

It has not been checked against a live Azure subscription. Where Azure's behaviour could not be confirmed from a recording or from Microsoft's documentation, the
service's page says so. Treat it as a new tool: use it for development and tests, and keep a small set of tests that run against real Azure.

## What it cannot tell you

An emulator that is always fast, always consistent and accepts every credential cannot show the failures that come from the outside. Keep a small set of tests
that run against real Azure.

- **Throttling and slowness:** there are no 429 or 503 answers, no request units, no retry-after, and timing is not Azure's (locks, TTLs, soft delete and leases use
  real time and are fast). Your retry and back-off settings are never exercised.
- **Access control:** no role-based access checks. A missing role assignment, a wrong key or a managed identity with the wrong rights is a common Azure failure and never
  fails here.
- **Cosmos DB:** one physical partition, no splits, no consistency levels (reads see the latest write), no regions, request charges are made up.
- **Eventual consistency, regions, partition moves** and anything that depends on several machines.
- **Where the documentation was silent.** Service Bus messaging, Event Hubs, the Kafka endpoint and most of Cosmos have no recordings to check against; the tests and the
  code come from the same documentation. They were never compared with Microsoft's own Service Bus or Event Hubs emulators. Each service page lists its guesses.
- **Resource types that are stored only** (above), and operations that answer 501.
- **Services that are not emulated.** Azure Functions' runtime (run the real host: see above), Azure SQL (run SQL Server or its container), Redis (run Redis), Azure Files
  shares, Monitor and Application Insights (an app that exports telemetry will log errors), App Service, AKS, Virtual Machines, Event Hubs Schema Registry. Run the real
  engine in a container for the first three.
- **Platforms:** Linux x86-64 and arm64 (release binaries and the container image) and macOS on Apple silicon (release binary). There is no native Windows build: use the container image or WSL there ([`docs/install.md`](docs/install.md)).
- **Data directories** written by this version are tested to stay readable by later ones only for the state-file layouts that have a checked-in sample.

## Security

It has no sign-in: every program that can reach the port can read, change and wipe everything, including through `/_azemu/reset`. By default it listens on
loopback only, and it refuses requests that web pages in your browser make (cross-site requests and DNS rebinding). Publish container ports to `127.0.0.1` only
(`-p 127.0.0.1:4577:4577`, never `-p 4577:4577`). With `--data-dir` the Key Vault secrets and keys, and the identity signing key, are on disk unencrypted. A certificate
authority made with `--tls-name core.windows.net` must never be installed system-wide. Event Grid webhooks may go only to local addresses unless you say otherwise. Cosmos
stored procedures run client-written JavaScript in a separate process with limits. All of this, how each part was checked, and how to report a problem:
[`SECURITY.md`](SECURITY.md).

## Options

`azemu --help` lists them all; every setting is also an `AZEMU_*` variable ([`docs/SETTINGS.md`](docs/SETTINGS.md), generated from the code).

```
--port <n>            gateway port (default 4577); 0 picks a free port, the same on every address, and the ready line names it
--host <ip>           address to listen on, repeatable (default 127.0.0.1 and ::1)
--standard-ports      one fixed port per service, any host name works (AZEMU_STANDARD_PORTS=1)
--pin <service>=<n>   one more port that serves one service, repeatable
--cert-dir <path>     where the certificate authority is kept (default ~/.azemu)
--exit-on-stdin-close stop when standard input closes: for a test program that starts azemu with a pipe on its input
--tls-name <name>     more names the certificate authority may sign for (AZEMU_TLS_NAME)
--lenient             queues, topics, event hubs ... come into being when named (AZEMU_LENIENT=1)
--data-dir <path>     keep state between runs (AZEMU_DATA_DIR); --save-interval <seconds>, default 15
--allow-host <name>   serve this host name and everything under it (a hosts file for real Azure names)
--allow-origin <o>    let web pages from this origin use the emulator; `*` turns the cross-site check off
--allow-webhook-host <name>   let Event Grid webhooks go to this host (`*`: any)
--log-level <level>   error | warn | info | debug | trace
```

## Build, test, contribute, licence

```bash
cargo build -p azemu                        # a debug build: target/debug/azemu
cargo build --release -p azemu              # the release build (profile, features and targets: docs/releasing.md)
```

Tests: `cargo test -p azemu-<crate>` for a crate, `cargo test --workspace` for all of them, `.venv/bin/python tests/run_sdk_suites.py --bin target/debug/azemu` for the Python
suites, `tests/run_all.sh <binary>` for everything (about an hour). [`CONTRIBUTING.md`](CONTRIBUTING.md) says how to change the code and what a change must come with.
Installing: [`docs/install.md`](docs/install.md). Releasing: [`docs/releasing.md`](docs/releasing.md). Changes: [`CHANGELOG.md`](CHANGELOG.md).

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option. Third-party licences: [`THIRD-PARTY-LICENSES.md`](THIRD-PARTY-LICENSES.md).
