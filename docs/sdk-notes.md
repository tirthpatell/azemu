# Notes for each SDK

What each Azure SDK needs in order to talk to azemu, and what the SDKs themselves do that surprises people. Most of it is a client option, a host-name form or a
port that the SDK ignores, not a problem of the emulator: it is the same against Azure. Each entry was found while writing the SDK test suites in
[`tests/`](../tests/README.md), and the suites contain a working example of every one.

How to reach the emulator (host names, standard ports, https and the certificate authority) is in [Reaching the emulator](reaching-the-emulator.md). The
short form: names under `.localhost` on the gateway port (`http://myacct.blob.core.localhost:4577`) or the standard ports with `--standard-ports`. Entries below say
"https" where the SDK refuses to send a token over plain http. Services whose clients cannot create what a test needs (Event Hubs, Event Grid) are created through
the emulator's admin API or Resource Manager; see the service pages.

Contents: [Python](#python) · [JavaScript](#javascript) · [Go](#go) · [Java](#java) · [.NET](#net) · [Rust](#rust) · [Azure Functions](#azure-functions) · [Kafka](#kafka) ·
[Terraform](#terraform)

## Python

| Service | What to set, and what the SDK does |
|---|---|
| All | Trust the emulator's certificate authority per client: `connection_verify="/path/ca.pem"`. Do not set `SSL_CERT_FILE` or `REQUESTS_CA_BUNDLE` for the whole shell. |
| Blob, Queue, Table | Shared Key over plain http works with the host-name form (`AccountName=myacct;BlobEndpoint=http://myacct.blob.core.localhost:4577`). A token credential needs https. |
| Data Lake | `DataLakeServiceClient` takes the blob address from the dfs address by replacing `.dfs.` with `.blob.`, so `http://myacct.dfs.core.localhost:4577` works: both hosts reach the emulator. |
| Key Vault | No switch is needed for `<vault>.vault.localhost:<port>`: the challenge names the host of the request. A vault reached by a name with one label (`https://azemu:10010`, `https://localhost:4577`) needs `verify_challenge_resource=False` on the client. |
| Cosmos DB | `connection_verify` with a CA path is ignored by the Cosmos client: use `connection_verify=False` or trust the CA for the process. `retry_total=0` is ignored (zero counts as "not set" and the default of 9 applies): use `retry_total=1`. The async client has no `get_database_account`; use `client_connection.GetDatabaseAccount()`. The client asks for the account document (`GET /`) at construction and repeats it in the background. |
| Service Bus | `UseDevelopmentEmulator=true` in the connection string turns TLS off and keeps the port of `Endpoint=sb://myns.servicebus.localhost:<port>` (the key is case-sensitive and the value must be exactly `true`). `ServiceBusAdministrationClient` ignores that flag and always speaks https to the host and port of `Endpoint=`, so give it `connection_verify`. For AMQP inside TLS: `ServiceBusClient("myns.servicebus.localhost", credential, custom_endpoint_address="sb://myns.servicebus.localhost:<port>", transport_type=TransportType.Amqp, connection_verify=ca)`: `custom_endpoint_address` alone selects AMQP over WebSocket. Settling a peek-locked message waits up to 60 seconds for the broker's answer, so a broken broker makes tests hang rather than fail. |
| Event Hubs | Same connection-string rule as Service Bus (`Endpoint=sb://myns.eventhub.localhost:<port>;...;UseDevelopmentEmulator=true`, plus `eventhub_name=` or `;EntityPath=`). Every operation opens its own connection (each `get_partition_properties`, each producer partition, each consumer partition), so expect 0.3 to 0.5 s for each with the pure-Python client. A consumer with a checkpoint store uses owner level 0: a second client takes a partition from the first with the same level. AMQP inside TLS needs `custom_endpoint_address` and `transport_type=TransportType.Amqp`, as for Service Bus. |

## JavaScript

| Service | What to set, and what the SDK does |
|---|---|
| All | `NODE_EXTRA_CA_CERTS=/path/ca.pem` adds the CA to Node's list. |
| Key Vault | No client switch is needed. A `ClientSecretCredential` for another tenant than the one in the vault's challenge needs `additionallyAllowedTenants: ['*']`. `CryptographyClient.verifyData` checks the signature itself from the public key and uses PKCS#1 v1.5 padding for PS256, PS384 and PS512 (an SDK bug): for a check at the vault use `verify(algorithm, digest, signature)`. `CryptographyClient` checks `key_ops` itself before it sends anything. `@azure/keyvault-certificates` 4.11.0 does not send `options.policy` on `importCertificate`. `CertificateClient.getIssuer().name` is `"issuers"` (the SDK reads the second path segment). |
| Data Lake | The SDK maps a dfs address to the blob address it needs only for the known Azure suffixes. With `acct.dfs.core.localhost`, creating a file system works but listing file systems goes to the dfs host and gets 501. Use `http://acct.dfs.core.windows.net.localhost:<port>`: the SDK maps it to `acct.blob.core.windows.net.localhost`, and both reach the emulator. |
| Blob | A blob name with a `/../` segment cannot be addressed: the `URL` class normalises the path before it is sent. `getPageRanges` reports `count = end - start` (511 for a 512-byte page). |
| Cosmos DB | `PatchOperationType` has no `move` (use the string). `items.bulk()` refuses more than 100 operations per partition range: use `executeBulkOperations`. A cross-partition aggregate needs `VALUE` (a client-side rule). The SDK refuses an id with `/` itself. `item.read()` for a missing item returns `statusCode` 404 with `resource` undefined instead of throwing. |
| Service Bus | `UseDevelopmentEmulator=true` works for TCP and for the administration client, and it ignores `webSocketOptions`. For WebSocket leave the flag out and give `customEndpointAddress: 'https://myns.servicebus.localhost:<port>'` and `webSocketOptions: { webSocket: WebSocket }` (the `ws` package); the CA comes from `NODE_EXTRA_CA_CERTS`. `getQueue` for a missing queue throws `MessageEntityNotFoundError` with `statusCode` 200 (the SDK reads an empty feed). |
| Event Hubs | `UseDevelopmentEmulator=true` works for TCP (host `myns.eventhub.localhost`). For WebSocket: the connection string without the flag, `customEndpointAddress: 'https://myns.eventhub.localhost:<port>'` and `webSocketOptions: { webSocket: WebSocket }`. `loadBalancingOptions` is an option of the `EventHubConsumerClient` constructor, not of `subscribe()`. The `balanced` strategy claims one partition per cycle (10 s by default). |
| Resource Manager | `@azure/arm-resources` 8.0.0 has no `deployments`: they are in `@azure/arm-resourcesdeployments`. Give each client `endpoint` and `credentialScopes: ['https://management.localhost:<port>/.default']`. `@azure/eventgrid-namespaces` 1.0.0 is CommonJS. |
| App Configuration | `AppConfigurationClient` refuses `http://` unless `allowInsecureConnection: true`; use https. |
| Table | The SDK checks "same partition key" inside a transaction itself. `queryOptions.top` is not sent: use `byPage({ maxPageSize })`. A Guid comes back as `{ value, type: 'Guid' }`. |

## Go

| Service | What to set, and what the SDK does |
|---|---|
| All | `*.localhost` names resolve without a custom dialer. For https set `SSL_CERT_FILE` to a bundle of the system list plus `ca.pem` for that process, or give the client a transport that trusts it. Bearer or key credentials need `https://` in `azblob`, `azcosmos`, `azeventgrid`, `aznamespaces`, `azsecrets` and the Resource Manager clients; Shared Key credentials (Blob, Queue, Table, Data Lake) work on plain http. |
| Identity | `msal-go` keeps only the first path segment of the authority, so `https://localhost:<port>/_azemu/identity/login` does not work: use `https://login.localhost:<port>` and `DisableInstanceDiscovery: true` (with discovery on, the credential asks `login.microsoftonline.com`). |
| Key Vault | No client switch is needed. With `ClientSecretCredential` the challenge names the tenant `00000000-0000-0000-0000-000000000000` and `azidentity` refuses other tenants than its own: set `AdditionallyAllowedTenants: []string{"*"}` (a managed identity is not affected). |
| Blob | `UploadBuffer` sends one Put Blob up to 256 MiB whatever `BlockSize` says: use `StageBlock` or `UploadStream` to get blocks. A 304 on `DownloadStream` is not an error and gives a nil `Body`. Metadata names come back in Go's canonical header case (`Owner`): compare without case. |
| Queue | `azqueue/sas` writes the wall clock of the `time.Time` it is given as UTC: pass `.UTC()`. `GetSASURL` signs for `https` only, so a plain-http request with it is answered `AuthorizationProtocolMismatch`. |
| Table | `GetTableSASURL` joins the service URL and the table name without a slash: end the service URL with `/`. `SASSignatureValues.Sign` signs the key range but leaves `spk`, `srk`, `epk` and `erk` out of the query. `EDMEntity.MarshalJSON` doubles single quotes in `PartitionKey` and `RowKey`, so keys with `'` cannot round-trip on any server. `SubmitTransaction` has no error type (a failed batch is a `ResponseError`). Keys with `# ? / \` are refused by the service. |
| Data Lake | `directory.Client.Delete` always sends `recursive=true`, `file.Client.Delete` sends `false`. `UploadBuffer` and `UploadStream` need the file to exist (call `Create` first). Rename needs the destination's parent directory. |
| Cosmos DB | The SDK does not merge cross-partition `ORDER BY`, `TOP` or aggregates, and the gateway refuses them with 400 ("can not be directly served by the gateway"), as the real service does for a client that does not declare the query features; single-partition ones and cross-partition filters work. `ReadChangeFeed` needs a feed range or a continuation. `ItemOptions` has no `If-None-Match`. The SDK does not check an item id before it sends it. |
| Service Bus | The SDK has no AMQP rule management: use the `admin` client (https). `UseDevelopmentEmulator=true` turns TLS off; for TLS use `NewClient` with `TLSConfig` and `CustomEndpoint`. For WebSocket, `NewWebSocketConn` gets `wss://myns.servicebus.localhost:<port>/$servicebus/websocket`; use `coder/websocket` with an HTTP/1.1-only client. |
| Event Hubs | `PartitionClient.ReceiveEvents(ctx, n)` waits for all `n` events unless its context ends: give each call a short context in a loop. The processor's `UpdateCheckpoint` needs a context that outlives the processor's. `GetPartitionProperties` on a missing partition retries ("rpc: failed, status 400") until the retry options are off. |
| Event Grid | The SDKs cannot create topics or subscriptions (Resource Manager does on Azure): use the admin API (`/_azemu/eventgrid`) or Resource Manager. The older module `messaging/azeventgrid` is deprecated. |

## Java

| Service | What to set, and what the SDK does |
|---|---|
| All | Java needs a trust store with the emulator's CA: copy the JDK's `cacerts`, add `ca.pem` and set `-Djavax.net.ssl.trustStore` before the first client exists (`SSL_CERT_FILE` is not read). Do not create a `java.net.http.HttpClient` before that: it fixes the default `SSLContext`, which `qpid-proton-j` reads for AMQP over TLS. Token credentials need `https://`. |
| Blob | `BlobClientBuilder.endpoint("http://acct.blob.core.localhost:port/container/blob")` fails with `'blobName' cannot be null`: `BlobUrlParts` treats a host that contains `localhost` as an Azurite-style address. Use `endpoint(accountUrl).containerName(..).blobName(..)`. The same parser makes `BlobBatchClient.deleteBlobs(List<String>)` send `DELETE //container/blob`: use `BlobBatch.deleteBlob(container, blob)`. `listBlobsByHierarchy(String)` takes a prefix, not a delimiter: use the overload with a delimiter. `getBlobOutputStream(true)` of 10 MB sends one Put Blob (the single-upload limit is 256 MB): pass `ParallelTransferOptions` to force blocks. |
| Service Bus | `UseDevelopmentEmulator=true` keeps TLS off and the port of `Endpoint=`, also for WebSocket (plain `ws://`). For `wss://` leave the flag out and use `customEndpointAddress("https://host:port")` (an `sb://` custom endpoint is refused). The administration client builds `https://{host}` and drops the port: add a pipeline policy that puts it back. `createRule(topic, rule, subscription, options)` has the rule before the subscription. The sync receiver renews locks by itself for 5 minutes (`maxAutoLockRenewDuration(Duration.ZERO)` to see a lock expire) and wraps entity errors in `RuntimeException("The receiver client is terminated")`. A second settlement of one message is refused by the SDK. `tryAddMessage` of a message larger than an empty batch throws; it returns false only when the batch is partly full. |
| Event Hubs | `UseDevelopmentEmulator=true` works for TCP and WebSocket. `EventDataBatch.tryAdd` of an event larger than an empty batch throws `AmqpException`. `EventProcessorClient` starts from `latest` without a checkpoint (`initialPartitionEventPosition` changes that). `EventData` cannot carry an AMQP value or sequence body. `$default` is lower-cased in the receiver address and in blob names. |
| Cosmos DB | The Java SDK always speaks TLS to the endpoint, even for an `http://` URL; use `gatewayMode()`. An id with `%` fails inside the SDK (`URLDecoder: Incomplete trailing escape`). `readItem` with `If-None-Match` returns status 304 and a null item instead of throwing. A failed transactional batch reports the status of the failing operation (409), not 207. The bulk executor leaves `getOperation().getId()` null for create and upsert operations. Batch and bulk work over the gateway. |
| Key Vault | No challenge switch is needed. `CryptographyClient` does the public operations (encrypt, verify, wrap) locally from the key's public part; the private ones (decrypt, sign, unwrap) go to the vault. `setContacts`, `listContacts` and `deleteContacts` return lazy iterables. |
| App Configuration | 404 and 412 arrive as `HttpResponseException` (not `ResourceNotFoundException`). `getConfigurationSettingWithResponse(setting, null, true, ...)` returns a 304 as a response. |
| Identity | `azure-identity` 1.19 reads `IDENTITY_ENDPOINT` and `IDENTITY_HEADER` with `System.getenv`, not from its own configuration: an application sets the real variables. An `authorityHost` with a path does not work (`msal4j` keeps only the first path segment as the tenant): use `https://login.localhost:<port>` and `disableInstanceDiscovery()`. |
| Event Grid | The SDKs cannot create topics or subscriptions: use the admin API or Resource Manager. |
| Resource Manager | Build the fluent client's `AzureEnvironment` from `AzureEnvironment.AZURE.getEndpoints()` with `resourceManagerEndpointUrl` and `managementEndpointUrl` set to `https://management.localhost:<port>/`, a lambda `TokenCredential` and any subscription id. |

## .NET

| Service | What to set, and what the SDK does |
|---|---|
| Cosmos DB | Direct mode (the SDK default) is not emulated: the first item call asks `GET //addresses/` and gets 501. Use `ConnectionMode.Gateway`. The SDK does not escape `%` in an item id in the URL path, so the server reads `pct ` for `pct%20`. |
| Blob | A blob name with `.` or `..` segments is collapsed by `System.Uri` before the request. `BlobServiceClient.SetProperties` throws `NullReferenceException` unless the object comes from `GetProperties` first: read, change, write back. `DataLakeFileClient.ReadAsync` goes through the blob API, so a missing path is `BlobNotFound`, not `PathNotFound`. |
| Table | `CreateIfNotExists` returns a `TableItem` both times (look at the raw response for 409). `DeleteEntity` of a missing entity returns the 404 answer without throwing. |
| Service Bus | `PeekMessagesAsync` continues after the last message it returned (the second peek is empty: pass a sequence number). `DeadLetterSource` is set only when a dead-lettered message was auto-forwarded. Sending without `SessionId` to a session queue throws `ArgumentException` in the SDK. A session processor keeps its session link until the client is closed. AMQP over WebSocket needs `CustomEndpointAddress` (the port in `Endpoint=` is ignored). |
| Event Hubs | `PartitionReceiver` and `EventHubConsumerClient` need `UseDevelopmentEmulator=true` for TCP without TLS, or a `CustomEndpointAddress` with the port for TLS. |
| Key Vault | No switch is needed for `DisableChallengeResourceVerification`. A secret value over 25 KiB is refused, as on Azure. |
| Resource Manager | `Azure.ResourceManager.Resources` 1.12.0: `CreateOrUpdateAsync(WaitUntil.Completed, ...)` for a deployment fails inside the SDK (`No ModelReaderWriterTypeBuilder found for ArmDeploymentData`) before it reads any answer: start the deployment and read it with `Get`. |
| Identity | MSAL needs an https authority (`https://login.localhost:<port>/`) and `DisableInstanceDiscovery = true`. `ChainedTokenCredential` moves on only from a credential that says "unavailable", not from one that failed. |
| Azurite address | `UseDevelopmentStorage=true` means ports 10000 to 10002: start the emulator with `--pin` or `--standard-ports`, or use explicit endpoints. |

## Rust

A Rust team should start with [`rust.md`](rust.md): crates and versions, a client for each service, tests and CI. The settings, in short:

| Service | What to set, and what the SDK does |
|---|---|
| Crates | The 1.x crates, all on `azure_core` 1.1: `azure_identity` 1.0.0, `azure_storage_blob` 1.1.0, `azure_storage_queue` 1.1.0, `azure_security_keyvault_{secrets,keys,certificates}` 1.0.1, `azure_data_cosmos` 1.0.0 (stable) or 1.1.0-beta.2, `azure_messaging_eventhubs` 0.15.0 with `azure_core_amqp` 1.1.0. Event Hubs 0.16 needs `azure_core` 1.2 beta and cannot be in the same build (`cargo add` fails with "failed to select a version for `azure_core`"). The 0.21 crates (`azure_storage_blobs`, `azure_storage_queues`, `azure_security_keyvault`, `azure_messaging_servicebus`) are the old generation. |
| All | Every Azure crate uses rustls with the platform verifier, which reads `SSL_CERT_FILE` on Linux: write a bundle (the system list plus `ca.pem`) and set it before the first client is built. Blob, Queue and Key Vault clients also take `ClientOptions { transport: Some(Transport::new(Arc::new(reqwest_client))), .. }` with a `reqwest::Client` built with `tls_certs_merge([ca])`. In tests set `retry: RetryOptions::none()`: the default policy retries a certificate or connection error for over a minute (81 seconds for a Blob request) before it reports it. Crates that still use `reqwest` 0.12 with bundled roots (the legacy `azure_data_tables` 0.21, `azure_mgmt_*`) cannot trust the emulator's CA over https. |
| Identity | No `DefaultAzureCredential`. `ManagedIdentityCredential` reads `IDENTITY_ENDPOINT` and `IDENTITY_HEADER` (plain http works for this one). `DeveloperToolsCredential`, used in every crate README, needs the `az` program: against the emulator it fails with `az not found on PATH`. In tests any `TokenCredential` that returns a fixed token works. `ClientSecretCredential` needs `ClientOptions.cloud = Custom { authority_host: "https://login.localhost:<port>" }` (https only; the authority path is replaced, so `/_azemu/identity/login` does not work). |
| Blob, Queue | A bearer-token credential or none. A token is refused over `http://` by the SDK itself (`... must use https`, whatever the credential), so with a credential use https. Over http, or https, a client with no credential sends no `Authorization` header, and the emulator refuses it as it refuses any anonymous request: `401` for Blob, `403` for Queue (tried), with a log line that says the SDK was probably built without a credential; it succeeds only for a URL that carries a SAS or for a public container. No Shared Key, connection string or SAS generation; Blob has no listing by delimiter and no batch. |
| Key Vault | No switch for `{vault}.vault.localhost`. A host with a single label (`https://localhost:10010`, `https://azemu:10010`) needs `SecretClientOptions { verify_challenge_resource: Some(false), .. }` (the same field on the keys and certificates clients); without it the client fails with `challenge resource ... doesn't match the requested domain` (printed with `{:?}`). |
| Cosmos DB | Set `AZURE_COSMOS_BINARY_ENCODING_ENABLED=false` (or `BinaryEncodingOptions::new().with_enabled(false)`): the SDK's binary-encoded item bodies are not built and are refused with 400. Set `COSMOS_DISABLE_IMDS=1`: otherwise each client waits 2 seconds for Azure's instance metadata service at 169.254.169.254 before its first request. `azure_data_cosmos` needs the Cargo features `key_auth` (account keys), `control_plane` (create database and container) and `unstable_patch`. **The key must be valid base64** (azemu accepts any key, but the SDK computes the signature itself): use the well-known emulator key. `http://` is refused unless the host is a known Cosmos emulator host: use https. The SDK refuses cross-partition aggregates itself (`400/20113`). A change feed iterator never ends: stop at the first empty page. |
| Event Hubs | The crate takes no connection string and no `UseDevelopmentEmulator`: the AMQP address is `amqps://{host}/{hub}` (always TLS), and `{ns}.eventhub.localhost:<port>` works as the host. The hub must exist first (`PUT /_azemu/eventhubs/{ns}/hubs/{hub}`). `owner_level` has no effect: `azure_core_amqp` 1.1.0 does not put it in the attach frame. `EventDataBatchOptions::max_size_in_bytes` is overwritten with the link's maximum message size, and `try_add_event_data` counts the first event twice. |
| Service Bus | No official crate. The unofficial `azservicebus` 0.25 works (send, receive, complete), but always dials port 5671 whatever the connection string says: start the emulator with `--pin servicebus=5671` (or `--standard-ports`). Create queues with the HTTP management call or a config file ([`rust.md`](rust.md#7-create-what-a-test-needs)). |
| Table | Only the 2024 `azure_data_tables` 0.21: Shared Key over plain http. |
| Not covered | No usable official crate for App Configuration, Event Grid or Resource Manager: call their REST APIs with `reqwest` and any bearer token ([`rust.md`](rust.md#6-services-with-no-official-crate)). The Rust suites ([`tests/sdk-rust`](../tests/sdk-rust/README.md)) cover Blob, Queue, Key Vault, Cosmos DB, Event Hubs and identity. |

## Azure Functions

The Azure Functions host (Core Tools 4.x, .NET isolated and Node workers) works against azemu with these settings; the test app in
[`tests/functions`](../tests/functions/README.md) has a working `local.settings.json` for each extension.

- The host process needs the emulator's CA for the Cosmos DB trigger, the Event Grid output binding and identity-based connections: `SSL_CERT_FILE` set to the system
  list plus `ca.pem`, or `http://` for Cosmos DB.
- `UseDevelopmentStorage=true` fits only the Azurite ports. Otherwise give explicit endpoints (path style) and start the emulator with `--pin`.
- A storage connection that uses an identity (`__blobServiceUri`) must be https: the SDKs refuse to send a bearer token over http.
- Event Hubs with an identity needs the custom endpoint set in `host.json`.
- A queue message written by hand for a queue trigger must be base64.
- Identity-based Service Bus has no custom endpoint setting and dials port 5671 (AMQP over TLS), so it cannot use the gateway port: start the emulator with `--standard-ports` (which
  opens 5671 as well as 5672) or `--pin servicebus=5671`. Event Hubs has `extensions.eventHubs.customEndpointAddress` in `host.json` instead.
- Cosmos DB needs gateway mode (direct mode is not emulated).
- Nothing is created by being named: the host reports missing Service Bus entities, event hubs and Cosmos containers as Azure would. With `AZEMU_LENIENT=1` Queue,
  Service Bus and Event Hubs entities come into being on first use (Cosmos DB is not covered by lenient mode). A queue that is auto-created has a maximum delivery
  count of 10.
- The classic (polling) blob trigger works with either storage style, path style is faster.

## Kafka

Kafka clients connect to the gateway or to port 9093; see [`services/kafka.md`](services/kafka.md) for the settings (`security.protocol`, `sasl.mechanism=PLAIN`,
`sasl.username=$ConnectionString` and the connection string as the password). The event hub (the topic) must exist first.

## Terraform

`azurerm` needs `metadata_host = "management.localhost:4577"`, `resource_provider_registrations = "none"` and `storage_use_azuread = false`, and `SSL_CERT_FILE`
pointing at a bundle that contains `ca.pem`; the provider block is in the [README](../README.md#works-with) and a complete one in
[`tests/terraform/provider.tf`](../tests/terraform/provider.tf). The provider polls for a long time on some resources (about two minutes for a Key Vault); that is
the provider, not the emulator.
