# Using azemu from Rust

This page is for a Rust project that uses the Azure SDK for Rust. It takes you from nothing to a passing `cargo test` against the emulator, and says what
to do where the SDK has no crate. Everything here was run on Linux x86-64 against azemu 0.1.0 with the crate versions in the table; what was not run says "not tried".

If you only want to see one request work, the front page has it: [`README.md`](../README.md#a-first-request). A complete small project (Blob, Queue, Key Vault, Cosmos DB
and its tests) is in [`examples/rust/`](../examples/rust/README.md).

**Contents:** [1. Get azemu](#1-get-azemu) · [2. Pick the crates](#2-pick-the-crates) · [3. Reach it](#3-reach-it) · [4. Settings the SDK needs](#4-settings-the-sdk-needs) ·
[5. Build each client](#5-build-each-client) · [6. Services with no crate](#6-services-with-no-official-crate) · [7. Create what a test needs](#7-create-what-a-test-needs) ·
[8. Tests](#8-tests-with-cargo-test) · [9. CI](#9-ci) · [10. When something does not work](#10-when-something-does-not-work)

## 1. Get azemu

| How | Command |
|---|---|
| Release binary (Linux x86-64 and arm64, macOS on Apple silicon; with a checksum) | download it from the [GitHub release page](https://github.com/tirthpatell/azemu/releases) and put it on `PATH` |
| Container image | `docker run --rm -p 127.0.0.1:4577:4577 -p 127.0.0.1:10000:10000 ghcr.io/tirthpatell/azemu:<version>` |
| From source, pinned to a tag | `cargo install --locked --git https://github.com/tirthpatell/azemu --tag v<version> azemu --root <dir>`, and cache `<dir>` in CI |
| From a clone | `cargo install --locked --path crates/azemu` (Rust 1.91 or newer and a C compiler; about 3 minutes) |

Details: [`install.md`](install.md). Check it with `azemu --version`.

azemu is not on crates.io. Pin a version in CI: below 1.0 a minor version may change a flag or a setting ([`CHANGELOG.md`](../CHANGELOG.md)).

Start it with `azemu`. It prints one line with the gateway address and the path of its certificate authority:
`azemu ready gateway=http://127.0.0.1:4577 ca=/home/me/.azemu/ca.pem ...`.

## 2. Pick the crates

The Azure SDK for Rust has two generations with similar names. **Use the 1.x crates.** The older ones (`azure_storage_blobs` and `azure_storage_queues`
with an *s*, `azure_security_keyvault`, `azure_messaging_servicebus`, `azure_storage`, `azure_core` 0.21 and the `azure_data_tables` that goes with
them, all 0.21 from 2024) are no longer updated, and a crates.io search can list them before the current ones. How to tell the current ones apart: the crate name is
singular (`azure_storage_blob`), the Key Vault crates have a suffix (`azure_security_keyvault_secrets`), the versions are 1.x, and they depend on `azure_core` 1.x.

The versions below were run against azemu 0.1.0. All of them except Event Hubs sit on `azure_core` 1.1.

| Service | Official crate | Version run | Notes |
|---|---|---|---|
| Core, credentials | `azure_core`, `azure_identity` | 1.1.0, 1.0.0 | no `DefaultAzureCredential` (section 3) |
| Blob | `azure_storage_blob` | 1.1.0 | token credential only, so https only |
| Queue | `azure_storage_queue` | 1.1.0 | the same |
| Key Vault | `azure_security_keyvault_secrets`, `_keys`, `_certificates` | 1.0.1 each | a single-label host needs one option (section 5) |
| Cosmos DB | `azure_data_cosmos` | 1.0.0 (stable) or 1.1.0-beta.2 | features `key_auth` and `control_plane`, and `unstable_patch` for patch; gateway mode |
| Event Hubs | `azure_messaging_eventhubs` and `azure_core_amqp` | 0.15.0 and 1.1.0 | pin 0.15; see "The `azure_core` conflict" below |
| Table | none current; `azure_data_tables` 0.21 (2024) | 0.21.0 | Shared Key over plain http only |
| Service Bus | **none** | unofficial `azservicebus` 0.25 | needs port 5671 (section 6) |
| App Configuration | **none** | `reqwest` | REST (section 6) |
| Event Grid | **none** | `reqwest` | REST (section 6) |
| Resource Manager | **none usable** | `reqwest` | REST (section 6) |

Cosmos DB 1.0.0 and 1.1.0-beta.2 both work; the suites in [`tests/sdk-rust`](../tests/sdk-rust/README.md) use the beta and the example in `examples/rust` the stable one.

**The `azure_core` conflict.** The newest Event Hubs crate (0.16) needs `azure_core` 1.2.0-beta, and the other current crates need 1.1, so Cargo cannot put them in one build:

```
$ cargo add azure_core azure_identity azure_storage_blob azure_messaging_eventhubs tokio
error: failed to select a version for `azure_core`.
    ... required by package `azure_messaging_eventhubs v0.16.0`
```

The way out: stay on `azure_messaging_eventhubs = "0.15"` with `azure_core_amqp = "1.1"` (all the crates above then resolve together; tried). If you need 0.16, put the code that uses it in a
separate Cargo project with its own `Cargo.lock`, as [`tests/sdk-rust/beta`](../tests/sdk-rust/README.md) does. The checkpoint store for Event Hubs, `azure_messaging_eventhubs_checkpointstore_blob` 0.9.0,
also needs Event Hubs 0.15; it is not tried here.

Building the Event Hubs 0.15 crate links OpenSSL: install the development headers (`libssl-dev` on Debian and Ubuntu).

## 3. Reach it

**Endpoints.** The account is the first label of the host name; any name is a new, empty account.

| Service | Endpoint |
|---|---|
| Blob | `https://{account}.blob.core.localhost:4577/` |
| Queue | `https://{account}.queue.core.localhost:4577/` |
| Key Vault | `https://{vault}.vault.localhost:4577` |
| Cosmos DB | `https://{account}.documents.localhost:4577/` |
| Event Hubs | `{namespace}.eventhub.localhost:4577` (a host and port, no scheme: the crate always uses TLS) |
| Service Bus | `{namespace}.servicebus.localhost`, port 5671 (section 6) |

`*.localhost` names resolve on a Linux desktop. In a container, or on a machine where they do not, start azemu with `--standard-ports` and use `localhost` and the
standard ports ([`reaching-the-emulator.md`](reaching-the-emulator.md)); a path-style Blob address then is `https://localhost:10000/devstoreaccount1/` (tried with the Rust Blob crate).

**https, and the certificate.** The Rust crates send a token, and they refuse to send one over `http://` (`"http://.../ must use https"`, tried). azemu answers https on the same
port, with a certificate from its own authority, so your program must trust that authority (`ca.pem`, whose path is in the ready line, and which `GET /_azemu/ca.pem` also serves).
The Azure crates use rustls with the platform verifier, which reads `SSL_CERT_FILE` on Linux. That variable *replaces* the system list, so give it a bundle: the system list plus `ca.pem`.
Set it before the first client is built:

```bash
cat /etc/ssl/certs/ca-certificates.crt ~/.azemu/ca.pem > ~/.azemu/bundle.pem     # Linux; on macOS the system bundle is /etc/ssl/cert.pem
export SSL_CERT_FILE=~/.azemu/bundle.pem
```

**Without it, a request does not fail at once.** The Blob client's default retry policy retried the certificate error for 81 seconds and then reported
`InvalidCertificate(UnknownIssuer)`, and the emulator logs nothing because the handshake fails in the client. Turn the retries off in test code (section 5) and the same mistake fails in 40 milliseconds. The
Cosmos DB and Event Hubs clients failed at once in the same situation (tried: 0.4 s and 0.1 s). The Blob client can also take a `reqwest::Client` that trusts `ca.pem` through `ClientOptions.transport`
(section 8), which needs no variable; Cosmos DB and Event Hubs read only `SSL_CERT_FILE`. [`azemu-testkit`](#8-tests-with-cargo-test) sets it for you.

**Credentials.** azemu accepts any bearer token. The crates' own READMEs use `DeveloperToolsCredential`, which runs the `az` program and needs a sign-in; against azemu it fails with
`AzureCliCredential authentication failed. az not found on PATH` (tried). `azure_identity` has no `DefaultAzureCredential`. Use one of:

- `ManagedIdentityCredential::new(None)?`, the credential your service uses on Azure, with two variables that point it at azemu (tried; plain http is fine for this one):

  ```bash
  export IDENTITY_ENDPOINT=http://localhost:4577/_azemu/identity/msi/token IDENTITY_HEADER=azemu
  ```

- in tests, a credential of your own that returns a fixed token for code that takes `Arc<dyn TokenCredential>`. (`azemu-testkit` sets the two variables for you, so `ManagedIdentityCredential` works in tests.)

`ClientSecretCredential` also works if you set `ClientOptions.cloud` to a custom cloud whose `authority_host` is `https://login.localhost:<port>` (it is covered by `tests/sdk-rust`; not tried on this page).

**Cosmos DB keys.** azemu accepts any key, but the Rust SDK computes the signature itself and needs valid base64: `k` fails inside the client (`401/20402 ... failed to compute HMAC-SHA256
signature`, tried) before anything is sent. Use the well-known emulator key from [`services/cosmos.md`](services/cosmos.md#using-it) (`azemu_testkit::COSMOS_EMULATOR_KEY`).

## 4. Settings the SDK needs

| Setting | Why |
|---|---|
| `SSL_CERT_FILE` (section 3) | trust for azemu's certificate authority |
| `IDENTITY_ENDPOINT` and `IDENTITY_HEADER` | only if you use `ManagedIdentityCredential` |
| `AZURE_COSMOS_BINARY_ENCODING_ENABLED=false` (or `BinaryEncodingOptions::new().with_enabled(false)` on the client builder) | the Cosmos crate sends binary-encoded bodies by default; azemu reads JSON. Forgetting it gives a `400` whose text says exactly this (tried) |
| `COSMOS_DISABLE_IMDS=1` | otherwise each Cosmos client waits 2 seconds for Azure's instance metadata service at `169.254.169.254` before its first request (tried: 2.1 s, and 0.14 s with the variable). On a GitHub-hosted runner, which is an Azure machine, the SDK reads the runner's real metadata instead |
| `RetryOptions::none()` in the client options of Blob, Queue and Key Vault clients, in tests | the default policy retries a connection or certificate error for over a minute, so a wrong address or a missing certificate looks like a hung test |

## 5. Build each client

The lines below are taken from [`examples/rust/src/lib.rs`](../examples/rust/src/lib.rs), which compiles and runs. For tests the options say no retries; production code keeps the defaults.

```rust
use azure_core::http::{ClientOptions, RetryOptions, Url};

let client_options = ClientOptions { retry: RetryOptions::none(), ..Default::default() };
```

**Blob and Queue.**

```rust
let blobs = BlobServiceClient::new(
    Url::parse("https://myacct.blob.core.localhost:4577/")?,
    Some(credential.clone()),
    Some(BlobServiceClientOptions { client_options: client_options.clone(), ..Default::default() }),
)?;
let queue = QueueServiceClient::new(
    Url::parse("https://myacct.queue.core.localhost:4577/")?,
    Some(credential.clone()),
    Some(QueueServiceClientOptions { client_options: client_options.clone(), ..Default::default() }),
)?
.queue_client("jobs")?;
```

A container or a queue exists only after `create`; a second `create` of the same name answers `409 ContainerAlreadyExists`, as Azure does. With no credential at all
the clients work only for a URL that carries a SAS or for a public container; the other requests are refused (see "Credentials are checked loosely" in the front page). The crates have no Shared Key, connection string or SAS generation.

**Key Vault.** A vault reached as `{vault}.vault.localhost` needs nothing. A host with a single label (`https://localhost:10010` or `https://azemu:10010` under `--standard-ports`) makes
the client's challenge check fail; switch it off (the same field is on the keys and certificates clients):

```rust
let options = SecretClientOptions {
    verify_challenge_resource: Some(false),
    client_options,
    ..Default::default()
};
let secrets = SecretClient::new("https://localhost:10010", credential, Some(options))?;
```

Without the option the error reads `challenge resource 'https://vault.azure.net/.default' doesn't match the requested domain 'localhost'. Set verify_challenge_resource in client options ...`
when printed with `{:?}` (tried against `https://localhost:10010`).

**Cosmos DB.** Set the two variables of section 4 first. Create the database and the container (nothing is made for you), then use the container client:

```rust
let endpoint: AccountEndpoint = "https://myacct.documents.localhost:4577/".parse()?;
let account = AccountReference::with_authentication_key(endpoint, Secret::from(COSMOS_KEY));
let client = CosmosClient::builder().build(account, RoutingStrategy::ProximityTo("East US".into())).await?;
client.create_database("shop", None).await?;
let db = client.database_client("shop");
db.create_container(ContainerProperties::new("orders", "/customer".into()), None).await?;
let orders = db.container_client("orders", None).await?;
```

azemu serves gateway mode only; the Rust SDK needs no setting for that. Partition keys and ids passed to the item calls must be owned (`String`), not borrowed. The SDK refuses cross-partition aggregates itself
(`400/20113`). A change-feed iterator never ends: stop at the first empty page. The example shows an insert, a parameterised query, a cross-partition query and a `412` for a stale ETag.

**Event Hubs.** No connection string and no development-emulator switch: give the host and port, the hub and a credential. The hub must exist first (section 7).

```rust
let host = "myns.eventhub.localhost:4577";
let producer = ProducerClient::builder().open(host, "orders", credential.clone()).await?;
let event = EventData::builder().with_body(b"hello".to_vec()).build();
producer.send_event(event, Some(SendEventOptions { partition_id: Some("0".into()) })).await?;

let consumer = ConsumerClient::builder().open(host, "orders".to_string(), credential).await?;
let options = OpenReceiverOptions {
    start_position: Some(StartPosition { location: StartLocation::Earliest, inclusive: false }),
    ..Default::default()
};
let receiver = consumer.open_receiver_on_partition("0".to_string(), Some(options)).await?;
let mut events = receiver.stream_events();
if let Some(event) = events.next().await {
    println!("{}", String::from_utf8(event?.event_data().body().unwrap_or_default().to_vec())?); // hello
}
```

(`use futures::StreamExt;` for `next`.) `owner_level` has no effect and `max_size_in_bytes` of a batch is overwritten with the link's maximum, in this version of the crate
([`sdk-notes.md`](sdk-notes.md#rust)). The Event Hubs client reads only `SSL_CERT_FILE` and fails at once without it.

## 6. Services with no official crate

**Service Bus.** There is no current official crate. The 0.21 `azure_messaging_servicebus` sends the REST message operations, which azemu answers with `501` ([`services/servicebus.md`](services/servicebus.md); not tried here).
The unofficial `azservicebus` 0.25 speaks AMQP and works, with one limit: it ignores the port in the connection string and always dials **5671**. So azemu must listen there:
`azemu --pin servicebus=5671` (or `--standard-ports`, which opens 5671 too). Only one emulator per machine can hold that port, so parallel CI jobs on one runner must not each start their own Service Bus.
Without the pin the program fails with `Connection refused (os error 111)` (tried). Create the queue first (section 7). Tried with that crate: connect, send, receive and complete on one queue:

```rust
let connection = "Endpoint=sb://myns.servicebus.localhost/;SharedAccessKeyName=any;SharedAccessKey=c2VjcmV0";
let mut client = ServiceBusClient::new_from_connection_string(connection, ServiceBusClientOptions::default()).await?;
let mut sender = client.create_sender("commands", ServiceBusSenderOptions::default()).await?;
sender.send_message(ServiceBusMessage::new("ship order 1")).await?;
let mut receiver = client.create_receiver_for_queue("commands", ServiceBusReceiverOptions::default()).await?;
let message = receiver.receive_message().await?;
receiver.complete_message(&message).await?;
```

It needs `SSL_CERT_FILE` like the others (the crate uses OpenSSL). **Not tried** with it: topics and subscriptions, sessions, dead-letter queues, scheduled messages, batches, transactions, and AMQP over WebSocket.
If your code depends on those, test that part in another language (the emulator supports them; see the service page), or against real Azure.

**App Configuration, Event Grid and Resource Manager.** There is no usable crate for any of them. Call the REST API with `reqwest` and a token from `azure_identity`; azemu accepts any token.
This ran against azemu with `reqwest` 0.13 (its default TLS reads `SSL_CERT_FILE` like the Azure crates) and the variables of section 3:

```rust
use azure_core::credentials::TokenCredential;
use azure_identity::ManagedIdentityCredential;
use serde_json::json;

let token = ManagedIdentityCredential::new(None)?
    .get_token(&["https://management.azure.com/.default"], None)
    .await?;
let bearer = token.token.secret().to_string();
let http = reqwest::Client::new();

// App Configuration: set and read a key-value.
let store = "https://mystore.azconfig.localhost:4577";
http.put(format!("{store}/kv/greeting?api-version=1.0")).bearer_auth(&bearer).json(&json!({"value": "hello"})).send().await?;
let kv: serde_json::Value = http.get(format!("{store}/kv/greeting?api-version=1.0")).bearer_auth(&bearer).send().await?.json().await?;
println!("{}", kv["value"]);                                      // "hello"

// Event Grid: create the topic with the admin API, then publish.
http.put("http://127.0.0.1:4577/_azemu/eventgrid/topics/orders").json(&json!({})).send().await?;
let events = json!([{"id": "1", "subject": "order/1", "eventType": "Created", "eventTime": "2026-01-01T00:00:00Z", "data": {"n": 1}, "dataVersion": "1"}]);
let r = http.post("https://orders.eventgrid.localhost:4577/api/events?api-version=2018-01-01").bearer_auth(&bearer).json(&events).send().await?;
println!("{}", r.status());                                        // 200 OK

// Resource Manager: a resource group.
let group = "https://management.localhost:4577/subscriptions/11111111-2222-3333-4444-555555555555/resourcegroups/demo?api-version=2021-04-01";
http.put(group).bearer_auth(&bearer).json(&json!({"location": "eastus"})).send().await?;
let rg: serde_json::Value = http.get(group).bearer_auth(&bearer).send().await?.json().await?;
println!("{}", rg["properties"]["provisioningState"]);            // "Succeeded"
```

The operations each service answers are on its page: [`appconfig.md`](services/appconfig.md), [`eventgrid.md`](services/eventgrid.md), [`arm.md`](services/arm.md).

**Table Storage.** Only the 2024 `azure_data_tables` 0.21 exists. It reads its trust from bundled roots and cannot trust azemu's certificate authority, so use it over plain http with the account key.

## 7. Create what a test needs

Nothing below the account exists until something creates it, as on Azure. From Rust:

- **Blob containers, queues, Cosmos databases and containers, Key Vault secrets:** the SDK's own create calls (section 5).
- **Event hubs** (the Rust SDK has no administration client): the emulator's admin API, one call with any HTTP client. Tried with `curl`:

  ```bash
  curl -X PUT http://localhost:4577/_azemu/eventhubs/myns/hubs/orders -d '{"partitionCount":2}'      # 201, and the hub's description
  ```

- **Service Bus queues** (no administration client in Rust either): the HTTP management call the administration clients make, a `PUT` of an ATOM entry. Any `Authorization` header is accepted. Tried with `curl` (`201`):

  ```bash
  curl -X PUT -H 'Authorization: Bearer x' -H 'Content-Type: application/atom+xml' \
    "http://myns.servicebus.localhost:4577/commands?api-version=2021-05" \
    -d '<entry xmlns="http://www.w3.org/2005/Atom"><content type="application/xml"><QueueDescription xmlns="http://schemas.microsoft.com/netservices/2010/10/servicebus/connect"/></content></entry>'
  ```

- **Event Grid topics:** `curl -X PUT http://localhost:4577/_azemu/eventgrid/topics/orders -d '{}'` (section 6 ran it from Rust).
- **From a file**, loaded at start and again after every reset: `AZEMU_SERVICEBUS_CONFIG`, `AZEMU_EVENTHUBS_CONFIG`, `AZEMU_EVENTGRID_CONFIG` (formats on the service pages).
- **Resource Manager**, as Terraform and the management SDKs do (section 6 shows the REST form for a resource group).
- Or start with `--lenient`: queues, topics and event hubs then come into being when first used. Azure does not do this, so a test that passes only because of it would fail there. Cosmos DB databases and containers are never made for you.

`azemu-testkit` (below) has `create_event_hub` and `create_service_bus_queue`, which make these two calls.

## 8. Tests with `cargo test`

### With `azemu-testkit`

[`crates/testkit`](../crates/testkit/README.md) is a small crate, with no dependencies of its own, that does what a test suite needs. The first call of `shared()` finds or starts one emulator for the test binary on a free port
(`azemu` on `PATH`, or the file `AZEMU_BIN` names; or the one that `AZEMU_GATEWAY` names, if you set it), writes the certificate bundle, and sets the variables of section 4 for the process:
`SSL_CERT_FILE`, `IDENTITY_ENDPOINT` and `IDENTITY_HEADER`, `COSMOS_DISABLE_IMDS` and `AZURE_COSMOS_BINARY_ENCODING_ENABLED`. The emulator stops when the test program ends, however it ends (it is started with
`--exit-on-stdin-close`, and the system closes the pipe on its standard input). The crate also makes endpoints and unique names and resets an account. Add it as a dev-dependency:

```toml
[dev-dependencies]
azemu-testkit = { git = "https://github.com/tirthpatell/azemu", tag = "v0.1.0" }
```

(From a checkout of this repository, [`examples/rust`](../examples/rust/Cargo.toml) uses a `path` instead.) Install the matching `azemu` too (section 1). A test:

```rust
use azemu_testkit::{shared, unique_name, Service};
use azure_core::http::{ClientOptions, RetryOptions, Url};
use azure_identity::ManagedIdentityCredential;
use azure_storage_blob::{BlobServiceClient, BlobServiceClientOptions};

#[tokio::test]
async fn upload() {
    let emu = shared();                          // starts azemu the first time; the same one for every test in this binary
    let account = unique_name("t");              // a new account: no other test sees it
    let options = BlobServiceClientOptions {
        client_options: ClientOptions { retry: RetryOptions::none(), ..Default::default() },
        ..Default::default()
    };
    let blobs = BlobServiceClient::new(
        Url::parse(&emu.endpoint(Service::Blob, &account)).unwrap(),
        Some(ManagedIdentityCredential::new(None).unwrap()),   // the testkit set IDENTITY_ENDPOINT
        Some(options),
    )
    .unwrap();
    blobs.blob_container_client("box").create(None).await.unwrap();
    // ... upload, download, assert ...
    emu.reset_account(&account);                 // POST /_azemu/reset?account=...
}
```

- **Unique names.** Any account, vault or namespace name is a separate, empty account, so a test that makes its own with `unique_name` cannot disturb another test and the tests run in parallel.
- **Reset.** `emu.reset_account(&account)` empties one account in every service that has accounts; `emu.reset_all()` empties everything (do not call it from tests that run in parallel).
- **Addresses.** `emu.endpoint(Service::Blob, "acct")` is `https://acct.blob.core.localhost:<port>/`; `emu.host_name(Service::EventHubs, "ns")` is the `ns.eventhub.localhost:<port>` the Event Hubs crate takes;
  `emu.standard_endpoint(...)` is the standard-port form for an emulator started with `--standard-ports`, where `.localhost` names do not resolve.
- **What to create.** `emu.create_event_hub("ns", "hub", 2)` and `emu.create_service_bus_queue("ns", "queue")` make the two calls of section 7. `COSMOS_EMULATOR_KEY` is the Cosmos key.
- **A certificate in code.** `emu.ca_pem()` is the text of the emulator's `ca.pem`, for a client that takes a certificate as bytes: a `reqwest::Client` built with `tls_certs_merge` and given to a Blob client as
  `ClientOptions { transport: Some(Transport::new(Arc::new(client))), .. }` trusts the emulator without `SSL_CERT_FILE` (tried with the Blob client; the Cosmos DB and Event Hubs clients read only the variable).
- **A running emulator.** `AZEMU_GATEWAY=http://127.0.0.1:4577 cargo test` uses that emulator instead of starting one; `AZEMU_CA_FILE` names its `ca.pem` (the testkit fetches it if you give none).
- `AZEMU_TESTKIT_ARGS` adds arguments for the emulator (for example `--lenient`); `AZEMU_TESTKIT_DIR` says where its files go (default `target/tmp`, where `azemu.log` is).
- Call `shared()` before the first Azure client is built in the test process: it sets environment variables.

### Without it

Start the emulator outside `cargo test` (a shell step or a CI step), give the tests the port, and let them read the same variables a real program would:

```bash
azemu --cert-dir target/azemu &                  # prints the ready line; stop it with kill when the tests are done
cat /etc/ssl/certs/ca-certificates.crt target/azemu/ca.pem > target/azemu/bundle.pem
SSL_CERT_FILE=$PWD/target/azemu/bundle.pem COSMOS_DISABLE_IMDS=1 AZURE_COSMOS_BINARY_ENCODING_ENABLED=false \
  IDENTITY_ENDPOINT=http://localhost:4577/_azemu/identity/msi/token IDENTITY_HEADER=azemu cargo test
```

and in the test (`reqwest = "0.13"` as a dev-dependency; tried):

```rust
use std::sync::atomic::{AtomicU32, Ordering};

use azure_core::http::{ClientOptions, RequestContent, RetryOptions, Url};
use azure_identity::ManagedIdentityCredential;
use azure_storage_blob::{BlobServiceClient, BlobServiceClientOptions};

// A name nobody else uses: every test is its own storage account, so tests can run in parallel.
fn unique(prefix: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!("{prefix}{}x{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

async fn reset(account: &str) {
    let url = format!("http://127.0.0.1:4577/_azemu/reset?account={account}");
    reqwest::Client::new().post(url).send().await.unwrap().error_for_status().unwrap();
}

#[tokio::test]
async fn upload_and_download() {
    let account = unique("t");
    let options = BlobServiceClientOptions {
        client_options: ClientOptions { retry: RetryOptions::none(), ..Default::default() },
        ..Default::default()
    };
    let blobs = BlobServiceClient::new(
        Url::parse(&format!("https://{account}.blob.core.localhost:4577/")).unwrap(),
        Some(ManagedIdentityCredential::new(None).unwrap()),
        Some(options),
    )
    .unwrap();
    let container = blobs.blob_container_client("box");
    container.create(None).await.unwrap();
    let blob = container.blob_client("a.txt");
    blob.upload(RequestContent::from(b"hi".to_vec()), None).await.unwrap();
    let body = blob.download(None).await.unwrap().body.collect().await.unwrap();
    assert_eq!(&body[..], b"hi");
    reset(&account).await;
}
```

Starting the emulator from inside the test binary is possible too (the testkit does it): run `azemu --port 0 --exit-on-stdin-close --cert-dir <dir>` with a pipe on its standard input and read the port and the
path of `ca.pem` from the ready line (`gateway=` and `ca=`). `--port 0` picks a free port, the same on `127.0.0.1` and `::1`, and `--exit-on-stdin-close` makes azemu stop when your test program ends and the system
closes the pipe; a Rust test binary never drops its statics, so without the option you would have to stop the child yourself at exit. That is the reason to use the testkit rather than write it again.

### What the emulator cannot test for you

azemu never answers `429` or `503` and has no way to inject a failure, so retry and back-off code is not exercised by it. A Rust team can test its policy on the client side: `ClientOptions` takes `per_try_policies`
(a policy of yours can fail the first tries), and `azure_data_cosmos` has a `fault_injection` feature. Neither was tried here. Keep a small set of tests that run against real Azure.

## 9. CI

[`examples/ci/github-actions-rust.yml`](../examples/ci/github-actions-rust.yml) is a GitHub Actions workflow for a Rust project whose tests start azemu themselves through the testkit: it installs azemu once from a tag, caches it,
runs `cargo test`, and uploads the emulator's log (`target/tmp/azemu-testkit-*/azemu.log`) when a test fails. Nothing is published to the runner's ports, so parallel jobs do not collide. The file says which steps were run and which need the first release.

## 10. When something does not work

- **A request hangs for a minute or more, then reports `UnknownIssuer`:** the client does not trust azemu's certificate authority (section 3).
- **`... must use https`:** the Rust crates send a token only over https (section 3).
- **`az not found on PATH`:** you used `DeveloperToolsCredential`; use `ManagedIdentityCredential` or a fixed-token credential.
- **`challenge resource ... doesn't match the requested domain`:** a Key Vault host with one label (section 5).
- **A Cosmos `400` that mentions binary JSON:** `AZURE_COSMOS_BINARY_ENCODING_ENABLED=false` (section 4). **`401/20402` before anything is sent:** the key is not base64.
- **`409 ContainerAlreadyExists` on a second run:** the emulator kept the container; `POST /_azemu/reset` (or `?account=...`) empties it.
- **`Connection refused` from `azservicebus`:** start azemu with `--pin servicebus=5671` (section 6).
- `azemu --log-level debug` logs each request's query and headers, with credentials replaced by `<redacted>`.
- A request refused for lack of credentials is logged with the reason, at the default level, on standard error: for example `refused PUT /c1 (restype=container): no Authorization header and no SAS in the query: the Blob SDK was probably built without a credential` (tried).
- A `501` names the operation azemu does not have; `GET /_azemu/unsupported` lists them with counts (`DELETE` empties the list).
- `GET /_azemu/config` shows the settings in force and where each came from; `GET /_azemu/health` the version and the services.
- Some SDK errors print only a short text. Print them with `{:?}` (`Error: {e:?}`, or return the error from `main`) to see the cause.
- Other languages and the settings each SDK needs: [`sdk-notes.md`](sdk-notes.md). Everything about https: [`reaching-the-emulator.md`](reaching-the-emulator.md).
