# azemu tests with the Azure SDK for JavaScript

Tests that drive the emulator with the official `@azure/*` packages on Node, to catch what the Python (`tests/sdk-python`) and Rust (`tests/sdk-rust`)
suites cannot: other request building (header order and casing, URL encoding), Node's HTTP/TLS stack, keep-alive pools, and `rhea`, the AMQP library
under the Service Bus and Event Hubs clients. What the JavaScript SDKs need set to reach the emulator is in [`docs/sdk-notes.md`](../../docs/sdk-notes.md#javascript).

## Run

```bash
cd tests/sdk-js
npm ci                                        # once; installs the versions in package-lock.json
npm test                                      # every file, 3 at a time, about 2 minutes
node run.mjs blob                             # only files whose name contains "blob" (several words allowed)
JOBS=2 npm test                               # fewer files in parallel 
```

Needs Node 22 or newer (tested with Node 26.13, npm 11) and the emulator binary. The runner uses, in this order: `AZEMU_BIN`, a copy at `tests/sdk-js/.azemu-under-test`
(`cp ../../target/debug/azemu .azemu-under-test` keeps a stable copy while you rebuild), `target/debug/azemu`.
Build it with `cargo build -p azemu` in the repository root.

Install command used (exact versions are recorded below):

```bash
npm install --save-exact @azure/identity @azure/storage-blob @azure/storage-file-datalake @azure/storage-queue @azure/data-tables \
  @azure/keyvault-secrets @azure/keyvault-keys @azure/keyvault-certificates @azure/app-configuration @azure/cosmos @azure/service-bus \
  @azure/event-hubs @azure/eventhubs-checkpointstore-blob @azure/eventgrid @azure/eventgrid-namespaces @azure/arm-resources \
  @azure/arm-resourcesdeployments @azure/arm-storage @azure/arm-servicebus ws
```

## Entities are created by the tests

The emulator does not create queues, topics, event hubs, consumer groups or Event Grid topics because a client names them (as Azure does not; `AZEMU_SERVICEBUS_AUTOCREATE`, `AZEMU_EVENTHUBS_AUTOCREATE`, `AZEMU_EVENTGRID_AUTOCREATE`, or `AZEMU_LENIENT=1` for all, turn it on). The tests create what they use, as a team does against Azure: Service Bus queues and topics through `ServiceBusAdministrationClient`, event hubs (and consumer groups) through the emulator's admin API (`newHub()` in `test/eventhubs.test.mjs` and `test/eventhubs_ws.test.mjs`), Event Grid topics through the admin API. No test sets an `AUTOCREATE` variable.

## How a file runs

`run.mjs` starts one emulator per test file (free port, temporary `--cert-dir`, waits for `/_azemu/health`), then runs `node --test --test-force-exit <file>` with
`NODE_EXTRA_CA_CERTS=<that emulator's ca.pem>`, `AZEMU_PORT`, `AZEMU_CERT_DIR`, `AZEMU_LOG_PATH`, and `IDENTITY_ENDPOINT` / `IDENTITY_HEADER` pointing at the
emulator's managed identity endpoint. Node reads `NODE_EXTRA_CA_CERTS` only at start, which is why the launcher exists; no client uses a per-client TLS option.
The emulator is stopped afterwards. A file that hangs is killed after 600 s (`FILE_TIMEOUT_S`); one test after 120 s (`TEST_TIMEOUT_MS`).
Tests share their file's emulator and use unique names (`uniq()` in `lib/emu.mjs`); nothing is reset between tests.
`azurite.test.mjs` starts a second emulator with `--pin` ports and the same cert directory. Other environment: `AZEMU_TEST_LOG_LEVEL` (default `debug`),
`AZEMU_TEST_KEEP_LOGS=1` (keep the temp directory with `azemu.log`, under `tests/sdk-js/.tmp`).

A skipped test whose reason starts with `azemu:` is a known difference of the emulator from Azure; `sdk:` is a limit of the SDK (see [`docs/sdk-notes.md`](../../docs/sdk-notes.md#javascript)). To see a skipped test run,
use a copy of its file with the `{ skip: '...' }` option removed.

## Packages

| Package | Version | Package | Version |
|---|---|---|---|
| `@azure/identity` | 4.14.1 | `@azure/service-bus` | 7.10.0 (rhea 3.0.5) |
| `@azure/storage-blob` | 12.34.0 | `@azure/event-hubs` | 6.0.4 |
| `@azure/storage-file-datalake` | 12.31.0 | `@azure/eventhubs-checkpointstore-blob` | 2.0.0 |
| `@azure/storage-queue` | 12.32.0 | `@azure/eventgrid` | 5.12.0 |
| `@azure/data-tables` | 13.3.2 | `@azure/eventgrid-namespaces` | 1.0.0 |
| `@azure/keyvault-secrets` | 4.12.0 | `@azure/arm-resources` | 8.0.0 |
| `@azure/keyvault-keys` | 4.10.2 | `@azure/arm-resourcesdeployments` | 2.0.0 (deployments moved out of arm-resources) |
| `@azure/keyvault-certificates` | 4.11.0 | `@azure/arm-storage` | 21.0.0 |
| `@azure/app-configuration` | 1.13.1 | `@azure/arm-servicebus` | 7.0.0 |
| `@azure/cosmos` | 4.10.2 | `ws` | 8.22.2 |

## Coverage

| File | Tests | What |
|---|---|---|
| `test/blob.test.mjs` | 81 | containers (create, metadata, list with paging, lease, access policy), block blobs (upload, `uploadData` / `uploadStream` / `uploadFile` with parallel blocks, ranges, MD5, stage and commit blocks, tags and tag search, soft delete and undelete, tiers), flat and hierarchy listing, blob leases, copy (async and sync with SAS), snapshots, versions, append (seal) and page blobs, conditional requests, 14 names that need encoding, SAS (blob, container, stored policy, response headers, account, user delegation), `BlobBatchClient`, service properties |
| `test/datalake.test.mjs` | 36 | file systems, directories, files, append and flush (also out of order), parallel upload, rename (file, tree, across file systems), ACLs (set, permissions, inheritance, recursive), leases, file / file system / directory SAS, Entra token |
| `test/queue.test.mjs` | 30 | lifecycle, metadata, messages (visibility, update, peek, TTL, 64 KiB limit, special characters), queue and account SAS, auth |
| `test/table.test.mjs` | 38 | tables, entities with every EDM type (BigInt, Date, Guid, Binary), raw OData types, keys with special characters, merge / replace / upsert, etags, queries (filters with `odata` templates, select, paging with continuation), transactions, SAS |
| `test/keyvault.test.mjs` | 53 | secrets (versions, paging, delete / recover / purge, backup / restore), keys (RSA, EC, import, rotate, backup, random bytes), `CryptographyClient` (sign, verify, encrypt, wrap, unwrap, remote and local), certificates (create poller, import, delete, issuers, contacts) |
| `test/identity.test.mjs` | 12 | `ManagedIdentityCredential`, `DefaultAzureCredential` through `IDENTITY_ENDPOINT`, `ClientSecretCredential` against `login.localhost`, `EnvironmentCredential`, used for Blob and Key Vault |
| `test/appconfig.test.mjs` | 27 | settings, labels, tags, etags, read-only, listing with filters and paging, revisions, `acceptDateTime`, feature flags, Key Vault references, snapshots |
| `test/cosmos.test.mjs` | 53 | databases, containers (unique keys, hierarchical keys, TTL), items CRUD with ETags, JSON types, special ids, partition key types, patch, bulk, transactional batch, queries (parameters, cross partition, ORDER BY, aggregates, GROUP BY, DISTINCT, JOIN, paging with continuation), change feed, TTL |
| `test/cosmos_depth.test.mjs` | 14 | session consistency, diagnostics, composite ORDER BY, GROUP BY with aggregates, hierarchical key queries, full-fidelity change feed, errors |
| `test/servicebus.test.mjs` | 47 | queues (peek-lock, receive-and-delete, abandon, defer, dead-letter, max delivery count, lock expiry and renewal, TTL, duplicate detection, forwarding, size limit), batches, scheduling, `subscribe()`, sessions, topics with SQL / correlation rules and actions, administration client |
| `test/servicebus_rules.test.mjs` | 8 | `ServiceBusRuleManager` over AMQP `$management` |
| `test/servicebus_ws.test.mjs` | 9 | AMQP over WebSocket (`ws`, wss) |
| `test/eventhubs.test.mjs` | 30 | producer (batches, partition id, partition key, size limit), properties, event content, consumer start positions (earliest, latest, sequence number, offset, enqueued time), consumer groups, owner level, blob checkpoint store (resume after checkpoint, 2-consumer load balancing, ownership list) |
| `test/eventhubs_ws.test.mjs` | 4 | the same over WebSocket |
| `test/eventgrid.test.mjs` | 20 | custom topic publish (Event Grid, CloudEvents, custom schema, SAS key), push delivery to a local webhook (headers, filters, retry, handshake), namespace topics with `@azure/eventgrid-namespaces` (send, receive, acknowledge, release, reject, renew) |
| `test/arm.test.mjs` | 15 | resource groups, storage accounts / containers / queues, Service Bus namespaces / queues / topics, deployments, and the created resources used from the data plane |
| `test/azurite.test.mjs` | 8 | `--pin` ports, path-style connection strings, Cosmos drop-in port |
| `test/http_depth.test.mjs` | 7 | 200 parallel uploads, 48 MiB parallel blocks, aborted download, short body, chunked request |

## Blob versioning and the Key Vault challenge

Blob versioning is off until someone turns it on, as on Azure. The one test that needs versions (`blob.test.mjs`) makes its own storage account
through the management API and switches it on there (`accountWithVersioning` in `lib/emu.mjs`: resource group, storage account,
`blobServices.setServiceProperties({ isVersioningEnabled: true })`), the way a team does. Key Vault clients are built with their defaults; one test in
`keyvault.test.mjs` shows that `disableChallengeResourceVerification: true` still works. The default pass runs with `AZEMU_LENIENT` and `AZEMU_BLOB_VERSIONING` unset.
