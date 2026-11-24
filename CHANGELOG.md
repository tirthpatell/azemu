# Changelog

All notable changes to azemu are written here, newest first. The version is the one in the root `Cargo.toml`;
`azemu --version` prints it with the commit the binary was built from. The project follows [Semantic Versioning](https://semver.org/),
with the usual allowance for versions below 1.0: a minor version may change a flag, a setting or the answer of an endpoint.

## 0.3.0

Six things an emulator of Azure usually cannot do, all off or out of the way until asked for: **faults on request** (throttling, errors, latency, dropped connections), a **strict authorisation mode**
(role assignments, Key Vault access policies, Service Bus and Event Hubs key rights and expiry, with Azure's own refusals), an **inspector** (a read-only API and a web page over the state of every service),
and three services that were missing: **Azure Files** (the REST API), a **sink for Application Insights and Azure Monitor telemetry**, and the **Event Hubs Schema Registry**. Event Grid and Resource Manager
templates gained what was listed as not built. As in 0.2.0, what could be compared with a real Azure subscription was compared (November 2026), and each page says what was and what was not.

**Three answers changed to the ones Azure gave, and a test that relied on the old ones can fail:**

- An Event Grid subscription on a storage account that names no event types receives only `BlobCreated` and `BlobDeleted` (it received every event before). Name the others in `includedEventTypes`.
- In Resource Manager templates `equals`, `contains`, `distinct`, `union` and `intersection` compare strings with case, as Azure does (they ignored case before), and a number with a fraction in an expression is refused.
- An event that Event Grid delivers to a Service Bus queue, a topic or an event hub has the content type `application/json; charset=utf-8` (was `application/json`).

Three new names are in the ready line and in `GET /_azemu/health`: `monitor` and `file` (standard ports 10015 and 10004) and `auth` (the role registry behind strict authorisation; it has no port of its own). A program that reads the list of services, or that uses those ports for something else next to `--standard-ports`, will notice.

### Added

- **Faults**: rules that make the emulator misbehave on purpose, so that retry and back-off code runs. Off until a rule is added: `POST /_azemu/faults` adds one, `PUT` replaces the
  list, `DELETE` removes (`/_azemu/faults/{id}` one), `GET` lists them with how often each fired, and a full `POST /_azemu/reset` removes the ones added through the API. `--faults
  <file>` (`AZEMU_FAULTS`) holds rules that exist from the start and come back after a reset. A rule matches on service, account, method, path pattern, query, and (Service Bus,
  Event Hubs, Kafka) operation, with `probability`, `skip` and `times`, and does one of: `throttle` (each service's own answer: Blob, Queue, Table and Data Lake `503 ServerBusy`; Cosmos DB
  `429` with `x-ms-retry-after-ms` and sub status 3200; Key Vault, App Configuration, Event Grid, Resource Manager `429`; AMQP `com.microsoft:server-busy`), `error` (a status, code and
  message of your own), `latency`, `drop` (close the connection), `timeout` (never answer), and for Cosmos DB `ruBudget` (a 429 once the request charges of the second are used up). A rule
  with a mistake in it is a 400 that says which; a rule is small (a path, query or account of at most 1024 bytes, a message of 2048, 8 headers) and a pattern is matched in one pass. A wait on an AMQP or Kafka connection ends when the client leaves. The request log says `fault=throttle rule=3` and an answer made by a rule carries `x-azemu-fault`. Tried with the Python, Go and Rust SDKs
  for Blob, Cosmos DB and others, and the Python SDKs for Queue, Table, Key Vault, App Configuration and Service Bus: they retried, and most waited for the retry header; what each one
  did is in [`docs/faults.md`](docs/faults.md). The emulator still never throttles by itself. `Service::fault_response` lets a service word its own answer (a default is provided);
  `azemu-amqp` has a handler wrapper (`faults::wrap`) that Service Bus and Event Hubs use.

- **Strict authorisation** (`--strict-auth`, `AZEMU_STRICT_AUTH=1`; off by default, and with it off nothing changes): a bearer token must be one the identity service issued
  (signature, `exp`, `nbf`, audience), and the caller's role assignments decide what it may do, with the answers Azure gave in November 2026: Blob, Data Lake, Queue and Table (`AuthorizationPermissionMismatch`, and
  `AuthorizationFailure` for create container and create queue; XML, OData JSON and Data Lake JSON; the 401 and 403 forms of a bad token by `x-ms-version`; a blob batch judged part by part), Key Vault (role-based `ForbiddenByRbac`
  with the action and resource named, and access-policy vaults `ForbiddenByPolicy` / `AccessDenied`), App Configuration (empty 403 and 401), Cosmos DB (its own roles; the three 403 sub-statuses 5300, 5301 and 5302) and Event Grid (401).
  A copy (Copy Blob, Put Blob From URL, the block, append and page forms) needs `read` on its source and a Data Lake rename needs `move` and `read` on the file system it comes from; the audience of a token must be a whole host the service is known by; the calls between the emulator's own services are marked in the process, not by a token text. Account keys, SAS and the other keys are checked as before; Resource Manager is not checked; Monitor and File do not check roles, and the start-up log says which services do not.
  Resource Manager stores and serves `Microsoft.Authorization/roleAssignments` and `roleDefinitions` (custom roles too) at every scope, and Cosmos DB's `sqlRoleDefinitions` and `sqlRoleAssignments`, whether the mode is on
  or not, so that Terraform's `azurerm_role_assignment` and `azurerm_cosmosdb_sql_role_assignment` work. `/_azemu/auth` gives roles to accounts Resource Manager did not make (short scopes such as
  `storage/myaccount/containers/data`), lists and removes them, and answers `GET /_azemu/auth/check` (what the registry decides and which assignments it looked at); `--auth-config <file>`
  (`AZEMU_AUTH_CONFIG`) holds assignments that exist from the start and after every reset; a refusal is logged in one line with the principal, the action and the scope.
  `GET /_azemu/identity/principal?client_id=` gives the object id a client id has (and the default managed identity's). The built-in roles with their real ids and actions are in; `azemu_core::auth`
  has the registry and the token check for a service that is not HTTP. [`docs/strict-auth.md`](docs/strict-auth.md).
  **Service Bus, Event Hubs, Kafka and the Schema Registry** check their credentials in this mode, with Azure's answers (checked in November 2026 with four principals and keys of three rights): a SAS
  token needs a key name the namespace or the entity has, a signature made with either key, an audience that names the namespace and covers the entity and an expiry not more than five minutes past; a bad one is refused at
  `$cbs` `put-token` (`401`, `com.microsoft:auth-failed`, `InvalidSignature` / `InvalidAudience` / `ExpiredToken`), and a good one without the right claim at the link attach
  (`amqp:unauthorized-access`, "'Send' claim(s) are required ..."). Entra tokens need `Azure Service Bus` / `Event Hubs Data Sender`, `Receiver` or `Owner`; Event Hubs answers a principal with no role at
  `$management` with a 401 inside the answer; the Service Bus administration API answers 401 (a rule needs `Manage`; a token needs the `actions` of a role); Kafka checks the SASL login (PLAIN with a
  connection string, OAUTHBEARER) and answers `TOPIC_AUTHORIZATION_FAILED`; the Schema Registry answers 401 `ErrorInRequest` naming the actions, and takes no SAS. Authorization rules come from Resource Manager
  (the keys `listKeys` gives, regenerated keys at once), from `AuthorizationRules` in the config files, or from `/_azemu/servicebus|eventhubs/{ns}/authorization-rules`; a namespace Resource Manager did not make
  has `RootManageSharedAccessKey` with the key `SAS_KEY_VALUE` (the one Microsoft's emulators show). A link whose credential runs out is detached unless it was renewed.

- **The inspector**: a read-only API over the state of every service, and a page that shows it. `GET /_azemu/inspect` is one tree for all services (`/{service}/{account}/...`, every node
  `{path, kind, title, properties, children, items, next}`, paging with `?limit=` and `?marker=`): Blob (containers, blobs with `?prefix=` and a delimiter view, blocks, snapshots,
  versions, the first 64 KiB of the content), Queue (every message, the hidden ones too), Table, Azure Files, Key Vault (secret values only with `?reveal=1`, never private key material),
  App Configuration (key-values, revisions, snapshots), Cosmos DB (containers, documents, scripts, change feed length), Service Bus (messages by state, sessions, rules, authorization rules),
  Event Hubs and Kafka (partitions, events from a sequence number, committed offsets, schema groups), Event Grid (subscriptions, delivery log, dead letters, pending deliveries),
  Resource Manager (resources with the stored-only flag, deployments and their operations), the role assignments of strict mode, the identity signer, the telemetry sink, and the
  emulator itself (`_emulator`: health, configuration, the operations asked for, the fault rules). Looking changes nothing: no message is locked or counted, no clock or sequence number
  moves, no account is made. A row carries at most 128 KiB (a value that does not fit is shown cut, with its length) and an answer 4 MiB (a longer page ends early and `next` goes on from there). `GET /_azemu/ui` is the page (one HTML, one script and one style sheet in the binary, no network; light and dark; deep links; filter, paging, auto refresh);
  it is served with a strict Content-Security-Policy and builds its DOM without `innerHTML`. Both refuse requests from other web sites like every admin path. `Service::inspect` is the
  new method a service implements. Checked in Chrome with `tests/inspector-ui/check.mjs`. `docs/inspector.md`.

- **Monitor**: a sink for Application Insights and Azure Monitor telemetry (`azemu-monitor`, standard port 10015). An application with `APPLICATIONINSIGHTS_CONNECTION_STRING`
  set runs against the emulator without logging errors, and a test reads back what it reported (`GET /_azemu/monitor/telemetry`, with filters by key, base type, name, operation id
  and `since`; `GET /_azemu/monitor/logs`; `DELETE` on each; `POST /_azemu/reset`). It takes the ingestion endpoint of the SDKs and the OpenTelemetry exporters (`/v2/track` and
  `/v2.1/track`, plain, gzip or newline-delimited, with Azure's `206` and error messages, recorded from a real resource), the application id lookup, Live Metrics (answered "nobody
  is watching"), and the Logs Ingestion API (`/dataCollectionRules/{rule}/streams/{stream}`). Telemetry is kept in memory only, bounded by `AZEMU_MONITOR_MAX_ITEMS`,
  `AZEMU_MONITOR_MAX_LOG_ROWS` and `AZEMU_MONITOR_MAX_MB` (items are kept as the text they came as, and the limit counts that text; a request may carry 65,536 items, the documented maximum, and a larger one is a 400); the oldest is dropped and the answers say how many. Queries, metrics queries and alerts answer 501. Tried with the Python
  and JavaScript OpenTelemetry exporters and the Python `azure-monitor-ingestion` client. [`docs/services/monitor.md`](docs/services/monitor.md).

- **Azure Files** (`file`, standard port 10004, `{account}.file.core.localhost`): the REST API of file shares, not SMB or NFS (a share cannot be mounted). Shares (properties,
  quota, access tier, metadata, stored access policies, statistics, snapshots, leases, soft delete and restore, permissions), directories (properties, metadata, listing with
  `include`, rename, delete), files (create, ranges, downloads by range, range lists and the difference to a snapshot, SMB properties, leases, copy, rename), service properties
  (metrics, CORS, SMB multichannel), Shared Key and SAS (service SAS `sr=s` and `sr=f`, account SAS, stored access policies), persistence and `reset`. Symbolic and hard links
  (NFS) and a copy from a blob answer `501`. A file may be declared up to 4 TiB without holding that much: a read of 1 MiB or more is sent in parts as the client takes it, and a clear of a range looks only at the stored pages. `AZEMU_FILE_SOFT_DELETE_DAYS` (default 7) sets how long a deleted share can be restored. Checked against a real account in November
  2026; Microsoft's 679 recordings of the Python SDK are in the replay (`tools/replay`, score in `fidelity/SCORES.md`). Docs: `docs/services/file.md`.

- **Event Hubs Schema Registry**: the REST API under `/$schemaGroups` on the namespace host (`https://{ns}.eventhub.localhost:{port}/$schemaGroups/...`), the one that `azure-schemaregistry`,
  the Avro encoders and the Kafka serializers of Microsoft call. Register (the same content gives the same id and version, changed content a new version), read by id and by version (`0` is the
  latest), `get-id` by content, list versions, group and schema names, and `DELETE`, with Azure's headers, error codes and texts (Avro that does not parse, a kind the group does not take,
  a wrong content type or `api-version`, a schema over 1 MB, a missing token) and `api-version` 2021-10, 2022-10, 2023-07-01 and 2024-05. A group in Forward or Backward mode refuses an Avro schema that breaks the mode
  (against the latest version, as Azure does; the check follows Avro's rule with the differences found on a real namespace, [`docs/services/schema-registry.md`](docs/services/schema-registry.md)
  says which). Groups are made as on Azure, outside the API: the admin API (`PUT /_azemu/eventhubs/{ns}/schemagroups/{group}`), the Event Hubs config file (`SchemaGroups`), Resource
  Manager (`Microsoft.EventHub/namespaces/schemagroups`), or a named group comes into being with `--lenient`. The registry is part of the namespace: kept by `--data-dir` (`schemas.state`, a
  file of its own), dropped by a reset and by deleting the namespace. Roles and the limits per namespace (25 versions on Standard) are not enforced; the emulator's own limits are 1,000 versions of one schema, and a compatibility check that goes more than 64 levels deep or needs more than 200,000 steps is a 400 that says so. Compared with a Standard namespace in
  November 2026; tried with the Python clients (sync, async, Avro encoder through Event Hubs). Go and Rust have no Azure registry client.

- **Event Grid: delivery properties, blob dead letters, Azure Function destinations, managed identity.** `deliveryAttributeMappings` (static, secret, dynamic from the event) become HTTP headers on a webhook or
  function and message properties on Service Bus (`MessageId`, `SessionId`, `PartitionKey`, `CorrelationId`, `Label`, `ReplyTo`, `ReplyToSessionId`, `To`, others as application properties) and Event Hubs (`PartitionKey`); a
  session-enabled queue now receives its events when `SessionId` is mapped. A mapping named like a header of the connection (`Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `Upgrade`, `TE`, `Trailer`, `Expect`, `Keep-Alive`, `Proxy-*`) or with the prefix `aeg-` is a 400. `deadLetterDestination` (`StorageBlob`) writes dead letters as blobs `{topic}/{SUBSCRIPTION}/{yyyy}/{MM}/{dd}/{HH}/{guid}.json` (a
  JSON array of the event with `deadLetterReason`, `deliveryAttempts`, `lastDeliveryOutcome`, `publishTime`, `lastDeliveryAttemptTime`); the write raises no blob event. `AzureFunction` destinations deliver to
  `{host}/runtime/webhooks/EventGrid?functionName={name}` of a Functions host named by `AZEMU_EVENTGRID_FUNCTION_HOSTS` or `PUT /_azemu/eventgrid/functions/{app}`. `deliveryWithResourceIdentity` and
  `deadLetterWithResourceIdentity` are accepted, shown and delivered like the plain forms. All of it arrives from ARM `PUT` of an event subscription and is read back in ARM's shape (a secret value is
  hidden). What Azure did with dynamic values (missing, empty, number, object, over 4096 bytes) and with dead letters was checked against a real topic in November 2026 (`docs/services/eventgrid.md`).

- **Resource Manager templates: more functions.** `list*` functions (`listKeys`, `listConnectionStrings`, `listAccountSas`, `listServiceSas`, `listSharedAccessKeys`, ...) run the
  provider action of the same name, so an output or a property can hold a key or a connection string, and they wait for a resource of the same template; `listAccountSas` and
  `listServiceSas` are new direct actions of storage accounts too. `utcNow` and `newGuid` (only in a parameter default, as in Azure), `dateTimeAdd`, `dateTimeToEpoch` and
  `dateTimeFromEpoch` with .NET date formats, `environment()` (the storage and Key Vault suffixes and the Resource Manager address point at the emulator), the lambda functions
  (`map`, `filter`, `reduce`, `sort`, `toObject`, `mapValues`, `groupBy`), the array, object and string functions that were missing (`items`, `intersection`, `flatten`,
  `objectKeys`, `shallowMerge`, `tryGet`, `indexFromEnd`, `lastIndexOf`, `join`, `like`, `uri`, `dataUri`, `base64ToJson`, `float`, `extensionResourceId`, `pickZones`, ...),
  user-defined functions, and templates with `"languageVersion": "2.0"` (symbolic names, `existing`, `definitions` and `$ref`, `nullable`, outputs with `copy`). All of it counts
  against the limits of a request. Tried with templates that the Bicep compiler wrote (`crates/services/arm/tests/bicep`); nine resource-free templates (`crates/services/arm/tests/azure`) were also deployed to a real resource group in
  November 2026 and the emulator answers the same for all of about 230 outputs; that made `equals`, `contains`, `distinct`, `union` and `intersection` compare strings with case,
  `objectKeys` sort, `K` and `o` write `+00:00`, `string(true())` write `True`, and refuse `P1W`, the format `U`, `string` of a float and fractions in expressions, as Azure does.
  A failed `utcNow`, `newGuid` or `list*` call now fails the deployment with Azure's error instead of refusing the template. [`docs/services/arm.md`](docs/services/arm.md).

- **Application Insights and Log Analytics through Resource Manager.** `Microsoft.Insights/components` is built (PUT, GET, PATCH, DELETE, lists, `currentbillingfeatures`): a stable
  `InstrumentationKey` and `AppId`, and a `ConnectionString` whose endpoints are the emulator's monitor service, so telemetry sent with `azurerm_application_insights.x.connection_string` is
  read back with `GET /_azemu/monitor/telemetry?ikey=...`; the application id lookup of the monitor service answers the component's `AppId`. `Microsoft.OperationalInsights/workspaces`
  is stored with a stable `customerId` and `sharedKeys`. The other types of the two namespaces stay stored only. Terraform case `insights` (apply, clean plan, destroy).

- **File shares through Resource Manager.** `Microsoft.Storage/storageAccounts/fileServices/shares` (PUT, GET, PATCH, DELETE, list) is joined to the Azure Files service: a share made through
  ARM exists in the file service with its quota, access tier, metadata and access policies; a share made through the file API is listed and read by ARM; the delete of a share, an account or a group
  removes it. `fileServices/default` keeps `shareDeleteRetentionPolicy` (it does not reach the file service, which has one global setting). A storage account shows `primaryEndpoints.file`.
  Terraform case `shares` (`azurerm_storage_share` in both forms, `azurerm_storage_share_directory`, `azurerm_storage_share_file`).

- **A certificate for a client that connects by IP address.** An address given with `--tls-name` (`AZEMU_TLS_NAME`) is now an IP alternative name of the certificate served when the client sends
  no server name; before, only `localhost`, `127.0.0.1` and `::1` were. Single addresses only (a range cannot be named). A public address is still accepted and logs a warning at start.

- **The image's health check turns healthy sooner.** `--start-period=10s --start-interval=250ms` (Docker Engine 25.0 or newer; older builders refuse the flag): the first checks come 250 ms
  apart instead of 5 s. A container was healthy 0.84 s after `docker run` (about 3.2 s with the 0.2.0 image), which is what a Testcontainers wait on the health check waits for.

### Changed to match Azure

- **A subscription on a storage account that names no event types gets only `Microsoft.Storage.BlobCreated` and `Microsoft.Storage.BlobDeleted`**, as on Azure (the default set of that topic type). It used to get every event the
  emulator raises. If your subscription relied on getting the others (`BlobTierChanged`, `AsyncOperationInitiated`, `DirectoryCreated`, `DirectoryDeleted`, `BlobRenamed`, `DirectoryRenamed`), add them to
  `includedEventTypes` (ARM: `filter.includedEventTypes`; Terraform: `included_event_types`; admin API: `filter.includedEventTypes`). Key Vault and App Configuration subscriptions and custom topics still get every event
  (their default sets are not in Microsoft's documentation).
- **Storage events follow what Azure raised** (checked against a real storage account in November 2026, `docs/services/storage-events.md`): the `BlobDeleted` event of a blob has `eTag` and `contentLength`, and
  `BlobTierChanged` / `AsyncOperationInitiated` have `eTag`. An event delivered to a Service Bus queue or an event hub has the content type `application/json; charset=utf-8` (was `application/json`), as the real one had.

### Security

- **A page on a public IP address is another site.** An `Origin` or `Referer` with an IP address used to count as the emulator's own whatever the address was, so with a browser too old to send
  `Sec-Fetch-Site` a page served from a public address could reach the admin API. It now passes only for an address on a private network (loopback, `10/8`, `172.16/12`, `192.168/16`, link-local,
  unique-local, `100.64/10`) or the very address and port the request was sent to; anything else is `403 CrossSiteRequestRefused`. A page on a public address that must use the emulator needs `--allow-origin`.
- **A browser must send an admin request's body as JSON.** `POST`, `PUT` and `PATCH` on `/_azemu/...` with a body and a browser's `User-Agent` need `Content-Type: application/json` (`415` otherwise): a second
  line behind the origin check, for the request types a page can send without asking first. Programs that are not browsers are not affected (`curl -d` works as before). [`SECURITY.md`](SECURITY.md) has both.

### Documentation

- [`docs/testcontainers.md`](docs/testcontainers.md): starting the published image from Testcontainers in Rust, Go and Python: the three things a test needs (the image with a pinned version, the mapped ports, a wait on `/_azemu/health`), plain http with
  the Azurite connection string, https with a token credential (the container's own certificate authority trusted by one client, `IDENTITY_ENDPOINT` and `IDENTITY_HEADER` at the mapped port, the Key Vault challenge switch, `AZEMU_TLS_NAME` when the Docker
  host is not `localhost`), Service Bus on the mapped port, reset, a seed file, and one container per test or per run (start-up measured at 0.25 to 0.35 s). Three tested projects in [`examples/testcontainers`](examples/testcontainers). It also records
  what the libraries do that surprises: they publish on every network address (the examples publish on `127.0.0.1`), the image's health check as the wait took about 3 seconds with the 0.2.0 image, the Rust `azservicebus` crate always dials port 5672, and the Rust
  Testcontainers crate has no reaper, so a container kept in a `static` outlives the tests. [`docs/azurite.md`](docs/azurite.md) points to it.

## 0.2.0

The emulator was compared with a real Azure subscription for the first time (November 2026), and the rough edges a project meets when it moves from Azurite were worked on (seeding, the Rust SDK's credentials, macOS, a migration page).
Many answers changed to the ones Azure gave. **A test that passed on 0.1.0 because the emulator was more lenient than Azure can fail now**; the ones most likely to be noticed:

- A storage signature that is made up (`sig=bad`, a Shared Key that is not base64) is refused for every account, and a Shared Key request with a date more than 15 minutes old is refused.
  Real signatures made by an SDK are not affected (the signature itself is still compared only for `devstoreaccount1`). `AZEMU_LOOSE_AUTH=1` brings the old leniency back.
- Service Bus: a client that names a queue or topic that does not exist is refused at the token step (`amqp:not-found`), which some SDKs show as an authentication error rather than "entity not found";
  a receiver cannot be attached to an entity that forwards; `ForwardTo` must name something that exists.
- Event Hubs: a partition key goes to the partition Azure sends it to, which is another one than in 0.1.0 for most keys; a consumer group nobody created is accepted.
- Kafka: a producer that names a topic that does not exist creates it (1 partition), as Azure does.
- Cosmos DB: `x-ms-version` is required on raw requests, division by zero and similar fail the query instead of answering `null`, and a query plan request must list the features the query needs.
- Event Grid: a topic made through Resource Manager checks its access keys.

What was compared, and what was not, is in the README ("How it is checked") and in each service page.

### Added

- `--seed <file>` (`AZEMU_SEED`): a JSON file of blob containers, queues, tables, Cosmos databases and containers, Key Vault secrets and App Configuration key-values that exist from
  the start and are made again after every reset (a partial reset makes again only what it removed). Each item is made by the service's own handler with the request a client would send;
  something that already exists, from `--data-dir` or named twice, is left as it is. A file that cannot be read, has a property that is not known, or lists something a service refuses stops the
  start with a message that names the file, the place in it and the reason. `GET /_azemu/config` shows the file as `seedFile`. Format and example: [`docs/seed.md`](docs/seed.md);
  a Compose example in `examples/seed`. Service Bus, Event Hubs and Event Grid keep their own files. `azemu-testkit` gains `create_blob_container`, `create_cosmos_database` and
  `create_cosmos_container`, and the Cosmos page and `docs/rust.md` show the two `curl` calls that make a database and a container.
- The Entra token paths (`/{tenant}/oauth2/v2.0/token`, the discovery documents and the other fixed shapes) also answer at the root of the gateway when the host names no service
  (`localhost`, an IP address, a bare name such as `azemu`), so `AZURE_AUTHORITY_HOST=https://localhost:4577` works for the SDKs that drop a path in the authority (the Rust
  `azure_identity` credentials, `msal-go`). A service says so with the new `Service::claims_root_path`; it is tried only where the gateway answered "No service matched".
  Tried with `azure_identity` 1.0.0 and Go `azidentity` 1.15.0.
- `GET /_azemu/` (and `/_azemu`) lists the admin endpoints with their methods, including the prefix of each service that has admin endpoints. An unknown `/_azemu/...` path points at it.
- `tests/sdk-rust/older`: a Rust SDK project pinned to `azure_core`, `azure_identity` and `azure_storage_blob` 1.0.0 and `azure_data_cosmos` 0.36.0 (12 tests: Blob over https with a credential and over http with an account SAS, Cosmos with a key), in `run_all.sh` and in the `sdk-rust` CI job.
- `tests/sdk-python`: the Cosmos DB, Service Bus, Event Hubs and Kafka suites can run against real Azure resources with `AZEMU_AZURE=1` (`tests/sdk-python/README.md`, "Running a suite against Azure");
  tests of something only the emulator has are marked `emulator_only`, tests of a difference that was left are marked `azure_differs` with the reason.
- New settings, both in [`docs/SETTINGS.md`](docs/SETTINGS.md): `AZEMU_LOOSE_AUTH` (storage signatures and dates are not checked) and `AZEMU_SERVICEBUS_TIER=standard` (refuse what only a Standard namespace refuses).

### Fixed

- The container image makes `/home/azemu/.azemu` and `/data` private (mode 0700), so the first start no longer warns about the mode of the certificate directory. The container test checks that the
  start log has no WARN line. The warning for a TLS name the certificate authority may not sign for now names `--tls-name` / `AZEMU_TLS_NAME`.
- A SIGTERM or Ctrl-C that arrives the moment the ready line is printed, or while azemu is still starting, stops it cleanly with exit code 0. The handlers used to be installed just after
  the line, so a supervisor or a test that signalled at once could kill the process with no save and no exit code (seen on a busy machine: 46 of 60 tries with 0.1.0, none now).
- A start waits up to 300 ms for the lock of the data directory before it says that the directory is in use (an emulator that had just stopped could be refused its own directory for an instant).
- Three tests of the emulator's own crates depended on the day they ran and would have failed by themselves (the Cosmos DB one from 14 November 2026, the Queue one from 20 November 2026, the
  Key Vault one from February 2027): the checked-in state files of version 1 hold a time to live, a hidden message and a deleted secret, which run out. The files are unchanged; the tests now say what holds on any day.

### Documentation

- Documentation: [`docs/azurite.md`](docs/azurite.md), for a project that moves from Azurite (what stays the same, the options and what replaces them, what differs, the Rust SDK with an account SAS over plain http or with a credential
  over https, Compose and testcontainers). The Python Cosmos SDK's `InsecureRequestWarning` for a `localhost` host is explained (it is the SDK, not the emulator), and `docs/sdk-notes.md` says that other SDK versions are expected to work and only the listed ones were run.
- macOS and the Rust crates: the documentation no longer says that `SSL_CERT_FILE` works there. The Azure crates' TLS (`rustls-platform-verifier`) asks the Keychain on macOS, so the certificate authority has to be
  trusted there, or given to a Blob, Queue or Key Vault client in code; [`docs/rust.md`](docs/rust.md#macos) has the commands. A manual
  `macos-rust` job in `.github/workflows/ci.yml` does it on a macOS runner (not part of `ci-passed`): on macOS 26, Apple silicon, with the authority trusted in the System keychain, the testkit example and the Blob, Cosmos DB and identity tests of the Rust suite passed over https. The login keychain command, the removal commands and the certificate given in code were not run on macOS.
- The README says precisely what is and is not checked (bearer tokens and most keys are not; storage signatures for the development account are), and that the certificate authority is new at every container start without a volume.

### Changed to match Azure

Cosmos DB (NoSQL API). Each line is something a client can see. They come from a run of the Python suite and of raw probes against a real account in November 2026 (details: [`docs/services/cosmos.md`](docs/services/cosmos.md)).

- A replace whose body has another `id` renames the item (it keeps its resource id, the old id is gone, a taken id is a 409); it used to be a 400.
- Item ids: every character is accepted (`/`, `\`, `#`, a trailing space) up to 1,023 characters; 1,024 or more is a 400 with Azure's text, an id with fewer characters but more than 1,023 bytes is a 500, an id that is empty or not a string is a 400 that shows it. Names of databases and containers are at most 254 characters and may hold `#`, `?` and spaces (only `/` and `\` are refused); users, permissions and scripts refuse nothing but length.
- A request by id cannot reach a name with `#`, `/`, `\`, a trailing space or a percent escape that was encoded once more: 401 with the text Azure says it signed. Such an item is created, and found by a query.
- Every request is checked as Azure does: `x-ms-version` is required and must be a date from 2014-08-21; `x-ms-consistency-level` `Strong` and `BoundedStaleness` (stronger than the account's `Session`) and unknown values are a 400; a malformed `x-ms-session-token` is a 400 on documents and scripts, and a read whose token is ahead of the container is a 404 with sub-status 1002 (what the Python SDK meets after a container is deleted and made again under the same name); `x-ms-max-item-count` that is not an integer from -1 up is a 400. The session token the emulator sends is `0:-1#<lsn>`.
- Continuation tokens name their container (`-RID:~<rid>#...`); text that is not a token, `{}` and the token of another or deleted container are a 400 `The continuation token specified in the request is malformed...`; tokens of version 0.1.0 are still read. A page of a read feed or a query ends after the document that takes it to 4 MB (five 1.5 MB documents: 3 and 2, were 2, 2 and 1).
- Change feed: an `If-Modified-Since` that is not a date is ignored, an `If-None-Match` position ahead of the container is a 304 that gives the position back, and one that is not a number is a 400 `Change feed token format is invalid`.
- Partition key ranges: a plain read has no `etag` and no request charge and ignores `If-None-Match`; read with `A-IM: Incremental feed` (the Python SDK) it has an `etag` and ends in a 304; each range has `lsn` and `ownedArchivalPKRangeIds`.
- Headers and bodies of the account and of resources: a database answers with neither `x-ms-content-path` nor `x-ms-alt-content-path`, making or deleting a container sends only `x-ms-alt-content-path`; `GET /` has `disableCrossRegionalHedging` and `enablePerPartitionFailoverBehavior` and no top-level `consistencyPolicy`; a container made with the default policy has no `uniqueKeyPolicy`.
- Errors: the 404, 409 and 412 of items have Azure's shape (the text, then `RequestStartTime`, `RequestEndTime` and `Number of regions attempted`), a database, container, user or script that exists is `Resource with specified id, name, or unique index already exists.`, a partition key header that does not match the document is `PartitionKey extracted from document doesn't match the one specified in the header...` (sub-status 1001; fewer or more components and an unreadable header have their own texts), a body that is not JSON, an empty body, a name that is not valid, a missing property and a missing trigger have Azure's words, and a partition key value over 2,048 bytes says `Supplied partition key length 2049 is bigger than maximum allowed 2048.`.
- Patch: Azure's messages (`For Operation(1): Path(from 'x') on which patch operation has to be applied is absent...`, `Cannot patch system property 'id'`, the missing `op`, `path` and `value`, array index and depth errors), a patch with no operations is a 304 with no body, a false condition is a 412 with sub-status 1110, a bad condition is the compiler's text inside `Errors`, a patch that makes the document too deep is a 500, `/` is a valid path (the empty name), 11 operations say `The number of patch operations cannot exceed '10'.`.
- Users, permissions and triggers keep the case of `permissionMode`, `triggerType` and `triggerOperation` as sent (they used to be rewritten to `Read`, `Pre`, `All`), a second permission of a user on the same resource is a 409, and a bad mode, trigger type or operation is a 400 with Azure's text.
- Stored procedures and functions: `console.log` logs its first argument as text and the calls run together (`hellon =`; `console.info`, `warn`, `error` and `debug` are a `TypeError`, the 1 KB cut is gone); a script that sets no body answers `""`; a chain or query call with no callback appends its result as JSON text to the body; `flatten()` leaves nested arrays alone and `flatten(true)` flattens one level; a user-defined function is not called when an argument is undefined (no row, no error).
- A query that builds a value Azure cannot hold (`REPLACE` over a 1.5 MB string) is a 503, not a 400.

- Cosmos DB queries, ORDER BY: the documents that lack the sort field are part of the answer whatever the indexing policy (first ascending, last descending); only a path of the document may be sorted by (`2206` for an expression, a function, a constant or an aggregate; `Order-by over correlated collections is not supported.` for a JOIN value); the path must be in the index (`The index path corresponding to the specified order-by item is excluded.`, indexing mode `none`, `c._rid`); rows with equal keys come in creation order (reversed when every key is DESC).
- Cosmos DB queries, aggregates and GROUP BY: `SUM` of no values is 0 (also in a subquery); `AVG` adds integers exactly; `MIN` and `MAX` over arrays or objects are undefined; a select list with an aggregate may only hold aggregates, constants and GROUP BY expressions (`SC2102`, `SC2103`, `SC2041`, `SC2110`, `SC2111`, `SC2101`, `SC2104`); with one GROUP BY expression and only `COUNT` the groups come in ascending order of the key, and DISTINCT over one value ascending.
- Cosmos DB queries, scalar subqueries that can return several rows are refused when the query is compiled (`SC2201`); `TOP`, `OFFSET LIMIT` and `ORDER BY` are refused in any subquery (`SC2203`, `SC2204`, `SC2202`).
- Cosmos DB queries, NaN and infinity: division by zero, `LOG(0)`, `SQRT(-1)` and the like fail the query with a 400, sub-status 4001, instead of answering `null`. Values of different types are unequal (`=` false, `!=` true), the ternary takes its second branch for anything but `true`, a parameter that was not supplied is undefined, a fraction as an array index is cut off.
- Cosmos DB queries, syntax: `--` comments; reserved words cannot be property names, aliases or unquoted object keys; `TOP`, `OFFSET` and `LIMIT` take a whole number or a parameter only (`SC1001`, `SC2060`, `SC2061`, `SC2062`); duplicate names in a select list (`SC2022`) or aliases (`SC2021`); at most 10 joins (`SC3002`); `LIKE ... ESCAPE` of more than one character (`SC2207`); a member indexer that is not a number, a string or a parameter (`SC2990`); `udf` only in lower case; deeper nesting accepted (about 990 levels, then `SC1030`); parameter names must start with `@` and may not repeat; Azure's error codes and texts for the end of the text (`SC1002`), a bad token (`SC1010`), string (`SC1012`) and number (`SC1011`), `SELECT *` without FROM (`SC1021`), argument counts (`SC2050`), a missing function.
- Cosmos DB queries, functions: `SETINTERSECT` takes Azure's order, `LASTINDEXOF(s, '')` is `length - 1`, `SUBSTRING` with a negative start is `""`, `REPLACE` with an empty search string and a count with a fraction are undefined, `TOSTRING(1.5e-7)` is `1.5e-07`, `DATETIMEBIN` takes days and smaller units only, `DATETIMEFORMAT` writes a run of `d` or `M` once, date times with a UTC offset are accepted and converted to UTC, `DOCUMENTID` and `VectorDistance` are built.
- Cosmos DB query plan: a plan request is refused (400, one `Query contained <feature>, which the calling client does not support.` line each) when the client did not list a feature the query needs, and a request with no list supports none, so the Python SDK gets a 400 for GROUP BY across partitions instead of wrong rows; a select list of aggregates across partitions is `Cross partition query only supports 'VALUE <AggregateFunc>' for aggregates.` and `VALUE COUNT(1) + 1` is `Compositions of aggregates and other expressions are not allowed.`; MIN and MAX are planned as `{"item", "item2"}` for every client; `SELECT VALUE <aggregate> ... GROUP BY` has no aliases; ordered DISTINCT keeps every row and filters with `IS_DEFINED`; OFFSET LIMIT is planned as `OFFSET 0 LIMIT n`; the answer to a cross partition query that needs a plan is the plain text of Azure.

Blob quick query, customer-provided keys, structured messages, `$logs` and Data Lake, as real Azure answered them in November 2026 (a general-purpose v2 account and a hierarchical one; the answers are in the service pages).

- Quick query: a bad expression is `400 ErrorProcessingQueryExpression` before any data (`Syntax error in query: <expression>`), not a fatal `ParseError` in the stream. So are a table name or path that does not fit the input
  format, a column name when there is no header row or that is not in it, `_N` beyond the header row, `ESCAPE` of two characters and `IS MISSING` over CSV. `QueryType` may be any value; a missing or empty `QueryType` or `Expression`
  is `InvalidXmlDocument`. Parquet input of a blob that is not Parquet is `400` "Invalid parquet file. Corrupt footer.". A page or append blob is `400 QueryBlobContentsNotSupportedForPageBlobsOrAppendBlobs`, not `409 InvalidBlobType`.
- Quick query dialect: `IS NULL`, `||`, `SUBSTRING(x FROM n)`, a table alias, `_1.a`, `AS "quoted"`, several aggregates in one select list and `DISTINCT` are syntax errors, as on Azure; the language has no exponent (`1e3` is `1` with the alias `e3`).
- Quick query data errors: `InvalidTypeConversion` (non-fatal, "Invalid type conversion.") instead of `CastFailed`; division by zero, integer overflow, a bad timestamp or `TO_STRING` pattern and `SUM`, `AVG`, `MIN` or `MAX` over text end the query with a fatal
  `ClientError` and Azure's text; an empty field is NULL when read as a number; `CAST(x AS BOOLEAN)` of text that is not `true` is `false`; `TO_TIMESTAMP` needs the `T` (`2007-02-23` is refused). A `ParseError` of the data is at position 0 and counts its bytes from the start of the blob.
- Quick query output: numbers as typed when a select item is a number (`2.0`), computed numbers with 15 digits (`0.1 + 0.2` is `0.3`); timestamps as `2007-02-23T12:14:33+00:00`; no header row when a select item is an expression without an alias; a failed row leaves the items before it on the line;
  an aggregate is an empty data record, `progress`, the result record, `end`; an empty blob and `LIMIT 0` are only `end`; a result is cut at 4 MiB, not 256 KiB (Azure sent 1,020,000 bytes as one record).
- Customer-provided keys: another key is `409 BlobCustomerSpecifiedEncryptionMismatch`; a key for a blob written without one is `409 BlobDoesNotUseCustomerSpecifiedEncryption` (Get Blob, Get Blob Properties, Set Blob Metadata, query; writes are not refused); the hash alone does not name the key;
  a write with a hash and no key is `201`; a wrong hash is `InvalidHeaderValue` for `x-ms-encryption-key-sha256`.
- Structured messages: a wrong layout (also a segment one byte short, or a wrong `x-ms-structured-content-length`) is `400 InvalidStructuredMessage`, checked before the CRCs; the credentials are checked before the message.
- `$logs`: once logging was switched on, Delete Container, Set Container Metadata, Set Container ACL, Lease Container and every write of a blob into it are `403 InsufficientAccountPermissions`, not `400 InvalidOperation` for the delete alone.
- Data Lake: the Blob endpoint's operations sent to the `dfs` host are `400` (`InvalidQueryParameterValue` for `restype=container`, `InvalidUri` for the account root, `MissingRequiredHeader` for a `PUT` or `POST` on a path with no `resource` or `action`), not `501`;
  Create Container on the `dfs` host makes nothing and quick query is no longer served there (it was an extra). More than 32 ACL entries is `400 TooManyAclEntries`, an entry that cannot be read is `400 InvaldAccessControlList` (Azure's spelling).
  On the Blob endpoint a write below a file is `404 BlobNotFound` (Put Blob, Copy) or `409 OperationNotAllowedInCurrentState` (blocks), onto a directory `409 OperationNotAllowedInCurrentState` (Put Block List: `400 OperationNotSupportedOnDirectory`), never `PathConflict`;
  a name of 62 segments is `409 PathIsTooDeep`.

Storage credentials, anonymous access and CORS of Blob, Queue, Table and Data Lake, as real Azure answered them in November 2026 (a general-purpose v2 account; the answers are in the service pages).

- A signature that is made up is refused for every account: a Shared Key header whose signature is not base64 (`replay`, `Sanitized`, empty, no colon) is `400 InvalidAuthenticationInfo`; a SAS `sig` that is empty, not base64 or not 32 bytes is `403 AuthenticationFailed` with `sig is mandatory. Cannot be empty`, `Signature fields not well formed.` or `Signature size is invalid`. The MAC is still compared only for `devstoreaccount1`. A signature under 20 characters used to pass; recorded traffic needs `AZEMU_LOOSE_AUTH=1` now (the replay tool sets it).
- `AZEMU_LOOSE_AUTH=1` checks no signature of any form and no date, and logs a warning at start. The warning for each placeholder signature that was accepted is gone (a placeholder is refused unless the setting is on).
- A Shared Key request more than 15 minutes old is `403 AuthenticationFailed` (`Request date header too old: '<date>'`); no date or an unreadable date is refused too. A date in the future is not. A request with no `x-ms-version` is signed the old way (Shared Key Lite's string to sign).
- `sv` of a SAS that is not a date from 2012-02-12 on is `403 AuthenticationFailed` (Blob: `SAS field 'sv' is not well formed.`; Queue: `sv is either not in the correct format or is not equal or later than version 2012-02-12`; Table: no detail). `x-ms-version` that is not a date from 2008-10-27 on, or lies too far ahead, is `400 InvalidHeaderValue` on Queue and Table too (Table in its XML form).
- `sip` is checked on Queue and Table as on Blob, and a `sip` that is not the caller is `403 AuthorizationFailure` on all three (Blob answered `AuthorizationSourceIPMismatch`); one that is no address is `403 AuthenticationFailed`.
- A SAS whose expiry is not after its start (a missing start is the time of the request, so an expired SAS without `st` is this) is `403 AuthenticationFailed`, `Signed expiry time [..] has to be after signed start time [..]` (`must` for a Blob service SAS); other windows that are not now keep `Signature not valid in the specified time frame`. On Table every such SAS is `403 AuthenticationFailed` (it was `AuthorizationFailure`).
- No credentials: Blob answers `401 NoAuthenticationInformation` for a read of a container that does not exist too (it was `404 ResourceNotFound`); Queue and Table answer `401 NoAuthenticationInformation` with the bearer challenge (Table in JSON with `Content-Type: application/json`; Queue was `403 AuthenticationFailed`, Table `401 InvalidAuthenticationInfo`). An `x-ms-version` older than 2019-12-12 (Table: 2020-12-06) gets `404 ResourceNotFound` instead, as on Azure.
- `Authorization` header: a scheme Azure does not know (`Foo bar`) is `403 AuthenticationFailed` (Queue adds `Authentication scheme Foo is not supported`); one word only, or `Bearer` with no token, is `400 InvalidAuthenticationInfo` on all three (Queue and Table used to accept them). An anonymous Put Blob without `x-ms-blob-type` is `400 MissingRequiredHeader`, not `401`.
- CORS (Blob, Queue, Table): a preflight without `Origin` or `Access-Control-Request-Method` is `400 InvalidHeaderValue` ("A required CORS header is not present."), and Blob and Queue errors carry `MessageDetails`; methods are compared as written (`get` does not match `GET`); a matching preflight answers the origin (also for a rule with `*`) with `Access-Control-Allow-Credentials: true`, the request headers as sent, and no `Vary`; `Access-Control-Expose-Headers` lists the names of the rule, a name ending in `*` standing for the headers of that answer that start with it; a request with no `Origin` is matched by a rule with `*`, and a GET without `Origin` on an account with CORS rules gets `Vary: Origin`; Table CORS errors are in the Table service's XML.

Key Vault, App Configuration and Event Grid, as real Azure answered them in November 2026 (a Key Vault with role-based access, a free-tier store, a custom topic; the answers are in the service pages).

- Event Grid, custom topics: a topic has access keys when it is made through Resource Manager (`key1`, `key2` of `listKeys`; `regenerateKey` takes effect at once) or when the admin API or the config file gives `keys`. A wrong `aeg-sas-key` is `401`, an empty one `400`; a signed token (`aeg-sas-token`, `Authorization: SharedAccessSignature`) is checked for format, expiry, host and signature (`r` and `e` come from the signed text only; `s` must be last and no name may repeat); `Authorization: SharedAccessKey` is `401` (that is the namespace scheme). A bearer token is still not looked at, and a topic with no keys still takes any key. Messages end with Azure's `Report '…' to our forums…` sentence.
- Event Grid, classic publish: `api-version` other than `2018-01-01` (that looks like a version) is `400 UnsupportedApiVersion`; one event over 1,049,430 bytes is `413`; a topic fixed to the Event Grid schema reads events as Azure does (unknown properties are `400`, `dataVersion` is optional, numbers are taken as text, names are not case sensitive, the order of the checks and the texts are Azure's, a CloudEvent is refused whatever the content type); `OPTIONS /api/events` is `200`, other methods `405` with `Allow`, `POST /` and `POST /api` are `404`.
- Key Vault: `api-version` must be one Azure knows (`7.0` to `7.6` and their previews, `2015-06-01`, `2016-10-01`, `2025-07-01`, `2026-01-01-preview`, `2026-03-01-preview`, `2026-04-01`); a missing one is "api-version must be specified". Lists take `maxresults` 1 to 25 only and refuse a `$skiptoken` that is not base64url JSON; the tokens the emulator writes are now of that form. Key operations answer with Azure's texts for a bad algorithm, size or padding, an algorithm that does not fit the key or its curve, and a body with missing properties; `nbf` and `exp` have Azure's words. A secret over 25600 characters says so (`SecretTooLarge`); a secret without a value is "Property  has invalid value"; a restore of something that is not a backup is `400` `Malformed backup blob`; purge of a live object and recover of one that is not deleted are `400 MethodNotAllowed`; software `oct` keys, RSA sizes other than 2048, 3072 and 4096, and unknown curves are refused with Azure's texts.
- Key Vault, a certificate whose request is pending (issuer `Unknown`): the bundle is `enabled: false` with `nbf` and `exp`, the versions list is empty, a second create is `409`, a cancel leaves the request `inProgress` with `cancellation_requested`, and deleting the request leaves the certificate.
- App Configuration: `api-version` must be one of `1.0`, `2022-11-01-preview`, `2023-10-01`, `2023-11-01`, `2024-09-01`, `2026-04-01` (`400 Unsupported API version` otherwise); a request with none is taken and gets the older shape (no `etag` in a list). A `401` has no body. `400`s for a body that is not JSON, wrong `tags` and snapshot filters name the parameter and have an empty `detail`; six filter values say `key(0): Too many values. Maximum supported is 5`; a `PUT` that is not JSON is `415`; `$select` with an unknown field leaves it out; a number as `value` is stored as text; a locked key is `409` before a failing `If-Match`; on a list, a bare etag is no condition and a quoted `If-Match` that matches nothing on the page is `412`; a second snapshot of a name has Azure's body; a `Range` read of revisions links to the rest.

Service Bus. They come from a run of the Python suite and of raw probes against a real Standard namespace in November 2026 (details: [`docs/services/servicebus.md`](docs/services/servicebus.md)).

- The `$cbs` `put-token` for a queue, topic or subscription address whose entity does not exist is answered 404 `amqp:not-found` (the topic of a subscription must exist), so a client never attaches; the Python SDK shows an authentication error on send, peek and receive, where it showed "entity not found" (with auto-creation on every token is accepted).
- A receiver cannot be attached to a queue or subscription that forwards (`amqp:not-allowed`, "Cannot create a message receiver on an entity with auto-forwarding enabled"), nor to the dead-letter queue of one that forwards its dead letters.
- Forwarding: `ForwardTo` / `ForwardDeadLetteredMessagesTo` must be a full address of a queue or topic that exists (400 with Azure's texts); a chain longer than four hops is accepted and dead-lettered in the queue that would forward it a fifth time (reason `Maximum transfer hop count is exceeded.`), where the send was refused; when a target is deleted the messages stay in the source; a forward can go to a topic. `x-opt-enqueue-sequence-number` is set only on a message that came through a topic (the topic's number), and `x-opt-deadletter-source` of a subscription is `topic/Subscriptions/sub`.
- A filter or action that cannot be evaluated (a string compared with a number: `Operator '>' cannot be applied to operands of type 'string' and 'long'`) dead-letters the message in the subscription (`FilterException`, `RuleActionException`), or drops it when `DeadLetteringOnFilterEvaluationExceptions` is off. In the SQL language `<`, `>`, `<=`, `>=` take numbers and times only, `=` and `<>` two values of one type, `==` and a comma between action statements are syntax errors, `TRUE`, `FALSE` and a bare property are no filter, `REMOVE sys.x` is taken.
- Rules are refused with Azure's texts: empty, too long or unparsable expressions (`There was an error parsing the SQL expression. [Token line=1, column=3, ...]`), unknown functions and system properties, parameters used and not given or given and not used, `LIKE` with a number, a correlation filter with nothing set, names with other characters or over 50 characters.
- Scheduled messages: a message that was scheduled keeps the state `Scheduled` after it arrived; a peek that starts at an active message stops before the first scheduled one; cancelling an unknown sequence number is `amqp:not-allowed` 400 for the whole request, cancelling twice is no error; a scheduled duplicate is dropped and answers a real sequence number.
- Dead letters and deferral: no reason means no `DeadLetterReason` (it was `com.microsoft:dead-letter`); abandoning in the dead-letter queue does not count a delivery; a deferred message counts as delivered once; `receive-by-sequence-number` errors are 500 `com.microsoft:message-not-found` with Azure's three texts; a message with a time to live of zero is refused (`com.microsoft:argument-out-of-range`).
- Application property names are not case sensitive (`Key`, `key` and `KEY` are one property: the first spelling, the last value).
- Error texts: sending without a session id is `amqp:not-allowed`; the session receiver on a plain entity, the disabled entity (`Messaging entity 'sb://...' is currently disabled...`, `<namespace>:Topic:<topic>|<sub>` for a subscription) and the lost message lock use Azure's words; a missing subscription is named `<namespace>:Topic:<topic>|<sub>`.
- Administration API: zero or negative lock durations and time to live, `MaxDeliveryCount` outside 1 to 2,147,483,647 (500 above), `UserMetadata` over 1,024 characters, `MaxSizeInMegabytes` that is not a whole number of gigabytes (up to 80), `MaxMessageSizeInKilobytes` other than 256 or 1,024 to 102,400, names with other characters, a topic with `ReceiveDisabled` and a subscription with `SendDisabled` are 400 with Azure's texts; a duplicate window under 20 seconds is accepted (Azure takes it); an update with an ETag that is not the entity's is 412 (500 for a subscription); reads below a topic that is not there are 200 with the list of services; a body that is no description is 404; `$top`/`$skip` and `api-version` are checked.
- New setting `AZEMU_SERVICEBUS_TIER=standard`: refuses what only a Standard namespace refuses (`MaxMessageSizeInKilobytes` other than 256, `MaxSizeInMegabytes` over 5120). The default takes what Premium takes too. The replay of the Service Bus recordings (made on Standard) uses it.

Event Hubs and its Kafka endpoint. They come from a run of the Python suites and of raw probes against a real Standard namespace in November 2026 (details: [`docs/services/eventhubs.md`](docs/services/eventhubs.md), [`docs/services/kafka.md`](docs/services/kafka.md)). What only the Standard tier does is not copied where Microsoft documents more for Premium or Dedicated (compression, compaction as a limit, 32 partitions, 7 days, 10 hubs, 20 consumer groups); credentials are still not checked.

- A consumer group that was never created is accepted: a receiver on it gets the events, the `$cbs` token is accepted, and the group is not added to the hub's list (it was `amqp:not-found`; with auto-creation on it was created).
- Partition keys go where the Event Hubs service sends them. The rule was worked out from about 600 samples on hubs with 2 to 32 partitions: the key is cut to ASCII and put in upper case, hashed with lookup3, the two words XOR-ed, taken modulo 32767, and the positions are divided in order between the partitions. It is not the function of Microsoft's SDKs, which was used before (`a` and `A` now go together). Tested against 475 samples (`crates/services/eventhubs/tests/partition_key.rs`).
- Start positions: a sequence number or an offset past the last one is refused, inclusive or not, with `com.microsoft:argument-error` ("The supplied sequence number '3' is invalid. The last sequence number in the system is '2' ..."); an offset past the end used to wait. A filter that cannot be read is `amqp:not-allowed` ("The filter '...' is not a valid amqp filter for EventHub entities").
- Events carry the annotation `x-opt-sequence-number-epoch` (-1), runtime metrics `last_enqueued_sequence_number_epoch`, and an event written through Kafka also `x-opt-kafka-key`. The partition reply of `$management` has `begin_sequence_number_epoch` and `last_enqueued_sequence_number_epoch`, both -1 (the second was 0).
- Offsets are running byte counts with Azure's step (64 for a small event, then the message size plus 56, 59 from 256 bytes on, rounded up to a multiple of 8), not size plus 32.
- `$management`: an answer carries `operation` and `status-code` only; a type it does not know is 200 with no body (it was 501); an operation other than `READ` is refused when the request arrives with `amqp:not-allowed`, "Invalid command 'DELETE'." (it was 501).
- Error texts as Azure's: a partition that does not exist ("It should be between 0 and 3.", and a second line `Parameter name: PartitionId` on a link), the owner level conflicts and the sixth receiver (receivers are named `nil`), and `SystemTracker:<namespace>.servicebus.windows.net:<hub>` in the not-found text.
- The AMQP `open` of Event Hubs says `max-frame-size` 65536 and `idle-time-out` 120 s (Service Bus connections on the same port keep 1 MiB and 240 s). `azemu-amqp` gains `ConnectionHandler::open_settings` for this.
- Kafka: a producer or an admin client (`list_topics`) that names a topic that does not exist makes the event hub, with 1 partition and 7 days, whatever `AZEMU_EVENTHUBS_AUTOCREATE` says (a consumer's request does not). `CreateTopics` without a count or a retention makes 1 partition and 7 days (it was 4 and 1 day); a bad topic name is `INVALID_CONFIG` (it was `INVALID_TOPIC_EXCEPTION`); the text of `TOPIC_ALREADY_EXISTS` is Azure's.
- Kafka: `DescribeConfigs` of a topic lists `cleanup.policy`, `message.timestamp.difference.max.ms`, `message.timestamp.type` (`LogAppendTime`) and `retention.ms`, and nothing for a topic that does not exist (no error); of a broker it is `INVALID_REQUEST`. `CreatePartitions` is `INVALID_REQUEST`. `Metadata` lists no replicas and no in-sync replicas.
- Kafka: a fetch past the end of the log answers with no error and no records (it was `OFFSET_OUT_OF_RANGE`, which made `auto.offset.reset` apply); `ListOffsets` for a time after the last event gives the end offset (it was -1). A record that came in through Kafka is `LogAppendTime`, an event that came in over AMQP `CreateTime`.
- Kafka: a group without members is not listed, describes as `Dead`, and the request for the offsets of every partition returns nothing (a request that names the partitions still gets the commits). The session timeout is at most 5 minutes (it was 30). `ApiVersions` advertises Produce up to 7, ApiVersions up to 3, DescribeConfigs up to 2 and CreatePartitions up to 1, as Azure does.

## 0.1.0

The first version. It is an emulator of Azure services in one binary, made for developing and testing against: it is not for production, and it has no sign-in. What it
contains, what it does not, and how well it matches Azure are in [`README.md`](README.md) (per service: [`docs/services/`](docs/services/)); this entry does not repeat them.

- The services: Blob (with Data Lake Gen2 and quick query), Queue, Table, Key Vault, App Configuration, Cosmos DB (NoSQL API, gateway mode), Service Bus, Event Hubs (with a Kafka
  endpoint), Event Grid, Identity (token endpoints) and Resource Manager, behind one gateway port or one fixed port per service (`--standard-ports`).
- A container image (`Dockerfile`): the one binary on a small base, running as a user that is not root, with a health check (`azemu __healthcheck`), and stopping at once on `docker stop`.
- `azemu` builds with two Cargo features that are on by default: `cosmos-scripts` (the JavaScript engine for Cosmos stored procedures, triggers and user-defined functions) and
  `kafka` (the Kafka endpoint of Event Hubs). `--no-default-features` makes a smaller binary; the operations of a missing feature answer an error that says the build does not include them.
- Nothing comes into being by being named. A queue, topic, subscription, event hub, consumer group or Event Grid topic must be created first, as on Azure (an administration client,
  the Resource Manager endpoint, a config file or the admin API). The other behaviour is `--lenient` / `AZEMU_LENIENT=1`, or one service's own `..._AUTOCREATE` variable.
- Blob versioning is off for a storage account until it is turned on (through Resource Manager, or `AZEMU_BLOB_VERSIONING=1`).
- A request is served only for an expected host name, and a web page in a browser cannot call the emulator cross-site (`--allow-host`, `--allow-origin`). Event Grid webhooks
  go to local targets only (`--allow-webhook-host`).
- The admin API: `POST /_azemu/reset` (with `?service=` and `?account=`), `GET /_azemu/health`, `GET /_azemu/config`, `GET /_azemu/unsupported`.
- For Rust: [`docs/rust.md`](docs/rust.md) (crates and versions per service, a client for each, services with no official crate, tests, CI), a small project with tests
  ([`examples/rust`](examples/rust/README.md)), a GitHub Actions workflow ([`examples/ci/github-actions-rust.yml`](examples/ci/github-actions-rust.yml)), and `azemu-testkit`
  (`crates/testkit`), a crate with no dependencies that starts the emulator from `cargo test` and reaches it. `--port 0` picks a free port, the same on every address, and
  `--exit-on-stdin-close` stops the emulator when a test program that holds a pipe on its input ends.
- The certificate directory is created with mode 0700, and the emulator warns when an existing one is open to other users. A request that carries no credentials is refused with
  a log line that says why.
- Dual licensed under Apache 2.0 or MIT (`LICENSE-APACHE`, `LICENSE-MIT`), with `NOTICE`, `THIRD-PARTY-LICENSES.md` and `deny.toml`.

Linux x86-64 is the tested platform; other platforms are untested or unsupported ([`docs/install.md`](docs/install.md)).
