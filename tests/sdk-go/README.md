# azemu tests with the Azure SDK for Go

Tests that drive the emulator with the official Go SDK modules, to catch what the Python and Rust suites cannot: Go's header
handling and URL encoding (`url.Values.Encode()` writes a space as `+`), its HTTP client that offers HTTP/2 over TLS by default,
and `go-amqp`, the AMQP 1.0 library under Service Bus and Event Hubs. What the Go SDKs need set to reach the emulator is in [`docs/sdk-notes.md`](../../docs/sdk-notes.md#go).

## Run

```bash
cd tests/sdk-go
go test -count=1 -p 2 ./...              # everything, about 75 seconds once built (-count=1: go test does not see a new emulator binary)
go test -p 2 ./servicebus                # one service
AZEMU_RUN_FINDINGS=1 go test -count=1 ./servicebus   # also run the tests that show a known emulator difference (they fail)
AZEMU_DUMP=1 go test ./cosmos -run X     # print every HTTP request and response of the SDK clients that use emu.Dump
AZEMU_KEEP_LOG=/tmp/azemu.log go test .. # copy the emulator's debug log here when the package ends
AZURE_SDK_GO_LOGGING=all go test ...     # the SDK's own request log
go test -tags debug ./eventhubs          # with DEBUG_LEVEL=3 go-amqp prints every AMQP frame
```

* The emulator is `.azemu-under-test`, a copy of `target/debug/azemu` (git ignores it). Copy it again with
  `cp ../../target/debug/azemu .azemu-under-test`, or point `AZEMU_BIN` at another binary.
* Every test package starts its own emulator in `TestMain` (`internal/emu`): a free port, a temp `--cert-dir`, `--log-level debug`,
  waits for `/_azemu/health`, stops it at the end (and the emulator dies with the test binary: `Pdeathsig`).
* TLS: the emulator's CA is loaded from the temp `ca.pem` into an `x509.CertPool` and handed to each client through
  `ClientOptions.Transport` (HTTP clients) or `TLSConfig` (AMQP clients). `SSL_CERT_FILE` is never touched.
* Tests do not share names: `emu.Name(prefix)` makes unique names; the Service Bus, Event Hubs and Cosmos tests use a fresh
  namespace or account per test.
* A test that shows a known difference from Azure calls `emu.Finding(t, "azemu: ...")`: it is skipped, unless `AZEMU_RUN_FINDINGS=1`. There are few at present.
* Needs about 100 MB of disk for the module cache and the Go toolchain `go.mod` names. Go resolves `*.localhost` names here without any setup.

## Entities are created by the tests

The emulator does not create queues, topics, event hubs, consumer groups or Event Grid topics because a client names them (as Azure does not; `AZEMU_SERVICEBUS_AUTOCREATE`, `AZEMU_EVENTHUBS_AUTOCREATE`, `AZEMU_EVENTGRID_AUTOCREATE`, or `AZEMU_LENIENT=1` for all, turn it on). The tests create what they use: Service Bus queues through the administration client (`nsWithQueue` in `servicebus/main_test.go`), event hubs and consumer groups through the admin API (`ensureHub` in `eventhubs/main_test.go`, called by `newProducer`, `newConsumer`), Kafka topics with `hub()` in `kafka/kafka_test.go`. The one test about auto-creation (`TestMissingTopicsAreCreatedWhenAutoCreationIsOn`) starts an emulator of its own with `AZEMU_EVENTHUBS_AUTOCREATE=1` (`emu.StartEnv`).

## Modules

| Module | Version |
|---|---|
| `azcore` | 1.24.0 |
| `azidentity` | 1.15.0 (with `msal-go` 1.10.1) |
| `storage/azblob` | 1.8.3 |
| `storage/azdatalake` | 1.7.1 |
| `storage/azqueue` | 1.0.1 |
| `data/aztables` | 1.4.1 |
| `security/keyvault/azsecrets`, `azkeys`, `azcertificates` | 1.5.1, 1.5.0, 1.5.0 |
| `data/azappconfig` | 1.2.0 |
| `data/azcosmos` | 1.5.0 |
| `messaging/azservicebus` (+ `admin`) | 1.11.0 (with `go-amqp` 1.7.0) |
| `messaging/azeventhubs/v2` (+ `checkpoints`) | 2.0.2 (the `v2` module path is the newest stable; v1 ends at 1.4.0) |
| `messaging/eventgrid/azeventgrid`, `aznamespaces` | 1.0.0, 1.0.0 (`messaging/azeventgrid` 0.5.0 is deprecated, it moved here) |
| `resourcemanager/resources/armresources`, `storage/armstorage`, `servicebus/armservicebus` | 1.2.0, 1.8.1, 1.2.0 |
| `github.com/coder/websocket` | 1.8.15 (the WebSocket transport of the AMQP clients) |
| `golang.org/x/net/http2` | 0.62.0 (forced HTTP/2) |

## Coverage

| Package | What it covers |
|---|---|
| `identity` | `ManagedIdentityCredential` and `DefaultAzureCredential` through `IDENTITY_ENDPOINT`/`IDENTITY_HEADER` (user assigned client id, caching), then used for a Blob and a Key Vault call over HTTPS; `ClientSecretCredential`, `ClientCertificateCredential`, `WorkloadIdentityCredential`, `EnvironmentCredential` against `https://login.localhost:{port}` with `DisableInstanceDiscovery`; token signature checked with the emulator's key set; instance discovery left on; device code and on-behalf-of (findings) |
| `blob` | containers (create, metadata, ACL, list with paging, public access), block blobs (`UploadBuffer`, `UploadStream` of unknown length, parallel staged blocks, `DownloadBuffer` with parallel ranges, `UploadFile`/`DownloadFile`, ranges, MD5, CRC64 and structured message validation), properties, metadata, tags and tag queries, tiers, soft delete and undelete, names that need encoding, flat and hierarchy pagers, leases (blob and container), snapshots, versions, copy, service/container/account/user delegation SAS and stored access policies, batch delete and set tier, conditional requests, append blobs, page blobs, wrong blob type, Shared Key with the right and a wrong key, HTTPS with the CA |
| `datalake` | file systems, directories, files, `AppendData` + `FlushData`, parallel `UploadBuffer`/`UploadStream`, ranges, rename of files and trees, ACLs (set, get, update, recursive), recursive delete, metadata and headers, shared data with the Blob endpoint, names, file system SAS |
| `queue` | create (idempotent and conflict), enqueue, dequeue with visibility timeout, peek, update, delete with stale pop receipt, TTL, delayed messages, properties, clear, FIFO order, XML characters, size limit, listing with paging, SAS (https only and manual), wrong key |
| `tables` | create, delete, list; every EDM type (int64, GUID, binary, DateTime...); add, get, update (merge/replace), upsert, delete with ETags; queries with filters, select and paging; transactions (commit and rollback); SAS; forbidden key characters; wrong key |
| `keyvault` | secrets (versions, properties, delete/recover/purge, paging over 25, backup/restore, disabled), keys (RSA and EC create; sign and verify checked with `crypto/rsa` and `crypto/ecdsa`; encrypt/decrypt, wrap/unwrap, rotate, rotation policy, import, key operations, random bytes, backup/restore), certificates (self-signed create with the operation polled by hand, PEM import, policy, versions, delete/recover, issuers, contacts, backup/restore) |
| `appconfig` | settings, labels, ETag conditions, read-only, 304, delete, keys that need encoding, listing with filters and paging past 100, field selection, revisions, `AcceptDateTime`, snapshots (create poller, content frozen, archive, recover, conflict), Entra token over HTTPS |
| `cosmos` | databases, containers (policies, replace, TTL, unique keys), throughput (manual, autoscale, database level), items (CRUD, ETags, ids that need encoding, partition key types, hierarchical keys, 2 MB limit, numbers and unicode), `ReadManyItems`, queries (parameters, paging with continuation tokens, cross partition filters, aggregates, GROUP BY, JOIN, functions), patch, transactional batch (commit, rollback with 424), change feed, Entra token and connection string |
| `servicebus` | queues (peek-lock, complete, abandon, defer, dead-letter, receive-and-delete, peek, batches of 1,000, 200 KiB messages, scheduling and cancelling, TTL, competing receivers, lock expiry and renewal, duplicate detection, dead-letter on expiry, forwarding), the `admin` client (queues, topics, subscriptions, rules of all filter kinds, runtime properties, paging), topics with SQL and correlation rules and actions, sessions (findings), AMQP over TCP with `UseDevelopmentEmulator`, over TLS with a token credential, over WebSocket (`NewWebSocketConn` with `coder/websocket`, `ws` and `wss`) |
| `eventhubs` | producer (batches, partition id, partition key), `PartitionClient` from earliest, latest, sequence number (inclusive and exclusive), offset and enqueued time, partition properties, consumer groups, owner level, 500 KB events, the blob checkpoint store (claim ownership, checkpoints), WebSocket and TLS transports, the `Processor` (finding) |
| `eventgrid` | publish in Event Grid and CloudEvents schema to a webhook subscription, validation errors, 401, bearer token; namespace pull delivery (send, receive, acknowledge, release, reject, renew, expiry, ordering) |
| `arm` | resource groups (with poller on delete), storage account created through ARM and used with the Blob SDK with the key from `ListKeys`, container through ARM, name availability, Service Bus namespace and queue through ARM then read through the `admin` client and used over AMQP, a deployment of an ARM template |
| `http2` | which protocol the TLS listener negotiates; Key Vault and Cosmos round trips with the Go default transport, forced HTTP/1.1 and forced HTTP/2 (`http2.Transport`); 40 parallel requests on one HTTP/2 connection |

Not covered, on purpose or because the SDK cannot: rule management over AMQP (the Go SDK has none), transactions (the Go SDK has none),
Kafka, Schema Registry, Capture, Data Lake SAS with user delegation, container soft delete, immutability policies, Key Vault `Release` and managed HSM.

## Blob versioning and the Key Vault challenge

Blob versioning is off until someone turns it on, as on Azure. `TestVersions_EachOverwriteMakesAVersionThatCanBeReadByVersionId` makes its own storage
account through the management API and switches versioning on there (`emu.AccountWithVersioning` in `internal/emu/versioning.go`: resource group,
storage account, `BlobServicesClient.SetServiceProperties` with `IsVersioningEnabled`); `TestVersions_AnAccountWithoutVersioningKeepsNoVersions` checks
the default. Key Vault clients are built with their defaults; `TestChallengeResource_NoSwitchNeededAndTheOldSwitchStillWorks` shows the old switch works.
