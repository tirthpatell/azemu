# azemu tests with the Azure SDK for Rust

Tests that drive the emulator with the official Rust SDK crates, to catch what the Python suites (`tests/sdk-python`) cannot:
other header order and casing, URL encoding, HTTP/2, TLS stacks, and Rust's own AMQP implementation.
What the Rust SDKs need set to reach the emulator is in [`docs/rust.md`](../../docs/rust.md) and [`docs/sdk-notes.md`](../../docs/sdk-notes.md#rust).

## Run

```bash
cd tests/sdk-rust
cargo test                      # all test binaries, runs against a copy of the emulator
cargo test --test eventhubs     # one binary
cargo test -- --ignored         # run only the tests that show a known emulator difference (they fail)
(cd beta && cargo test)         # the second project, with the beta crates (own target directory)
```

Needs `target/debug/azemu` in the repository root (`cargo build -p azemu` there). To get a stable target while
others rebuild it, copy it once: `cp ../../target/debug/azemu target/azemu-under-test` (this copy is used when it exists), or point `AZEMU_BIN` at a binary.
The first build needs the network (crates.io) and takes several minutes; the target directory is `tests/sdk-rust/target` (ignored by git). Building a test binary also needs a C compiler
and the OpenSSL development headers (`libssl-dev` on Debian and Ubuntu), because the Event Hubs crate links OpenSSL.

Every test binary starts its own emulator on a free port with a temporary certificate directory, waits for `/_azemu/health`, builds a CA
bundle (system list plus the emulator's CA, written to the temporary directory) and points rustls at it with `SSL_CERT_FILE`, and stops the
emulator when the binary exits. Tests share that emulator and use unique names (`uniq()`), so they run in parallel.

Environment: `AZEMU_GATEWAY=http://127.0.0.1:4577` (use a running emulator; `AZEMU_CA_FILE` its `ca.pem`), `AZEMU_BIN` (binary to start),
`AZEMU_TEST_LOG_LEVEL` (default `debug`; `trace` shows every AMQP frame), `AZEMU_TEST_KEEP_LOGS=1` (keep the temporary directory with
`azemu.log`).

## Entities are created by the tests

The emulator does not create queues, topics, event hubs, consumer groups or Event Grid topics because a client names them (as Azure does not; `AZEMU_SERVICEBUS_AUTOCREATE`, `AZEMU_EVENTHUBS_AUTOCREATE`, `AZEMU_EVENTGRID_AUTOCREATE`, or `AZEMU_LENIENT=1` for all, turn it on). `tests/eventhubs.rs` creates every hub it uses through the admin API (`create_hub`, called by `hub()`), with the consumer group `analytics` next to `$Default`; `beta/tests/eventhubs_beta.rs` does the same through `create_eventhub` in `src/lib.rs`. No test sets an `AUTOCREATE` variable.

## Crates (versions in `Cargo.toml`)

Which crates and versions to use per service, which services have no official crate, and what a Rust team does then, is on one page for users:
[`docs/rust.md`](../../docs/rust.md#2-pick-the-crates). Here is what these suites run.

The newest releases are betas that need `azure_core` 1.2.0-beta, and the Cosmos crate used here needs `azure_core` 1.1, so Cargo cannot hold both
in one graph. `tests/sdk-rust` uses the stable `azure_core` 1.1.0 set (Cosmos is a beta, 1.1.0-beta.2); `beta/` is a second project with the
`azure_core` 1.2.0-beta set (`azure_core` 1.2.0-beta.2, `azure_identity` 1.1.0-beta.2, `azure_storage_blob` 1.2.0-beta.1, Key Vault secrets 1.1.0-beta.1,
Event Hubs 0.16.0 with `azure_core_amqp` 1.2.0-beta.1). The legacy `azure_data_tables` 0.21 (2024) covers Table, over plain http with Shared Key.
Service Bus, Event Grid, App Configuration and Resource Manager have no usable official crate, so these suites do not cover them.

TLS: reqwest 0.13 with rustls and `rustls-platform-verifier` for the current crates; `SSL_CERT_FILE` is honoured. The Event Hubs 0.15
crate also links OpenSSL (`libssl.so.3`; fe2o3-amqp prefers native-tls when both stacks are enabled, so AMQP-in-TLS goes through
OpenSSL, which also honours `SSL_CERT_FILE`; this is read from the fe2o3-amqp source and `ldd`, not otherwise verified); building it needs OpenSSL development headers. The
legacy table crate uses reqwest 0.12 with rustls and bundled roots, over HTTP only. Event Hubs 0.16.0 moves to rustls (aws-lc-rs).

## What is covered

| File | Tests | What |
|---|---|---|
| `tests/blob.rs` | 52 | containers (create, list with prefix and paging, delete, metadata, lease), block blobs (upload, download, ranges, properties, metadata, tags, delete), block upload and commit, listing (prefix, paging, metadata), copy, conditional requests (ETag), 9 MiB upload, special-character names (space, unicode, `%`, `+`, `#`, `?`), snapshots, versions, append and page blobs, tiers, tag search, access policy, anonymous access over HTTP |
| `tests/identity.rs` | 8 | `ManagedIdentityCredential` through `IDENTITY_ENDPOINT` / `IDENTITY_HEADER`, user-assigned client id, `ClientSecretCredential` against the login endpoint, tokens used for Blob and Key Vault |
| `tests/keyvault.rs` | 41 | secrets (set, get, versions, list, update, delete, recover, purge, backup, restore, limits), keys (RSA, EC, sign and verify, encrypt and decrypt, wrap and unwrap, rotate, backup), certificates (self-signed create through the poller, get, list, versions, delete, recover, purge) |
| `tests/queue.rs` | 25 | queue lifecycle, metadata, messages (send, receive, peek, delete, update, visibility timeout, TTL, 64 KiB), listing, access policy |
| `tests/tables.rs` | 26 | tables, entities (insert, get, update, merge, upsert, delete, ETag), queries (top, paging, select), transactions, typed values, Shared Key check |
| `tests/cosmos.rs` | 50 | databases, containers (also hierarchical keys), items (create, read, replace, upsert, delete, ETag, special ids, partition key types), patch, queries (parameters, cross-partition, order by, paging), transactional batch, throughput, change feed (latest and all versions and deletes), limits |
| `tests/cosmos_h2.rs`, `tests/cosmos_binary.rs` | 1 + 1 (the binary one is ignored) | the SDK's default settings (HTTP/2 probe, binary encoding) |
| `tests/eventhubs.rs` | 42 | hub and partition properties, send (single, batch, partition id, partition key), receive from earliest, latest, sequence number, offset, enqueued time, consumer groups. Each receiving test runs twice: through `azure_core_amqp` and through the Event Hubs SDK's own receiver |
| `beta/tests/blob_beta.rs` | 7 | blob 1.2.0-beta.1: Arrow-or-XML listing, hierarchical listing with a delimiter, async copy |
| `beta/tests/eventhubs_beta.rs` | 6 | Event Hubs 0.16.0: connection string, WebSocket transport, buffered producer; send checked through the admin API. Each test creates its hub first (`create_eventhub` in `src/lib.rs`, shared with the main project) |

Ignored tests carry `#[ignore = "azemu: ..."]` with the reason (at present one: Cosmos binary-encoded item bodies, which are not built). `cargo test -- --ignored` runs them.

## Blob versioning and the Key Vault challenge

Blob versioning is off until someone turns it on, as on Azure. `overwrite_creates_a_new_version_and_old_version_stays_readable` makes its own storage account
through the management API and switches versioning on there (`account_with_versioning` in `src/lib.rs`: plain HTTP to the gateway with the host
`management.localhost`, because the Rust SDK has no management crate for Storage); `an_account_without_versioning_keeps_no_versions` checks the default.
Key Vault clients are built with their defaults (`verify_challenge_resource` is left alone); `the_old_switch_still_works` and one Identity test use
`Some(false)`.
