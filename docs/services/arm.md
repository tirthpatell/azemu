# Resource Manager (ARM)

**What it emulates.** The Azure Resource Manager API (`management.azure.com`): subscriptions, resource groups, deployments (ARM templates, and the
JSON that Bicep compiles to), tags, and the resource types of the services in this emulator. Management SDKs and the `azurerm` Terraform provider
use it to create things. The point of the crate: **a resource created through ARM exists in the emulated data plane.** A Service Bus queue made
through ARM works over AMQP, a storage container made through ARM is visible to the Blob SDK, and so on.

**How to reach it.** `https://management.localhost:4577` (also `http://`); Terraform's `metadata_host` is `management.localhost:4577`. With
`--standard-ports`: `https://azemu:10013`. Any subscription id exists. A request without an `Authorization` header is refused; the token is not
checked. Over https the client must trust the emulator's certificate authority (`docs/reaching-the-emulator.md`).

**Not built.** Resource types outside the emulated services are *stored only*: a `PUT` of `Microsoft.Web/sites` is kept as JSON and answers
`Succeeded`, and nothing behind it exists (a virtual machine PUT makes no virtual machine). Every answer for such a type carries
`x-azemu-stored-only: true`, one warning is logged per type, `GET /_azemu/unsupported` lists the types, and `AZEMU_ARM_STRICT=1` makes them answer 501
instead. Types of a namespace that has a provider but that the provider does not build (storage private endpoints, Event Grid domains, `deploymentScripts`) answer
501. Locks, policy, quotas, soft delete and long-running operations (everything finishes inside the request) do not exist. Role assignments and role definitions are kept (below). The
table below says which types reach which service.

Crate `azemu-arm`. Tests and a Terraform run are listed under "Tests" and "Terraform"; the numbers and dates are in the README.

## Running it

`azemu-arm` alone stores resources and does nothing else (`Backends::none()`). The unified `azemu` binary
(`cargo build -p azemu`, `target/debug/azemu`) runs ARM and every service it drives on one port, with the services
connected to each other (Blob raises events into Event Grid, Event Grid delivers to queues, Service Bus and event hubs):
`crates/azemu/src/lib.rs` is the one place that says how. The SDK tests start it. Clients use
`https://management.localhost:{port}` (bearer tokens need https; trust `ca.pem` from the cert dir). `AZEMU_ARM_STRICT=1` (or `true`, `yes`,
`on`; one spelling of true and false for every setting, `azemu_core::settings`): a PUT, PATCH or POST for a resource type that has no provider
answers 501 naming the type (without it the type is stored as plain JSON). `AZEMU_ARM_DEPLOYMENT_HISTORY_MB` bounds the deployment records
(see `docs/SETTINGS.md`).

## How a request is handled

1. `/metadata/endpoints` answers the cloud metadata document (no token needed, see "Terraform").
2. No `Authorization` header: 401 `AuthenticationFailed` with a `WWW-Authenticate` challenge. Any token is accepted.
3. No `api-version` query: 400 `MissingApiVersionParameter`. The value is not checked.
4. The path is split into subscription, resource group and the rest. Any subscription id is accepted and exists.
   Path segments are matched ignoring case; `//` (an empty parent path, as the Python SDK sends) is ignored.
5. `.../providers/{ns}/{type}/{name}/{childType}/{childName}[/{action or collection}]` goes to the resource engine.
   The engine finds the provider for `{ns}`, canonicalises the type spelling, checks the group and the parent
   resource, calls the provider (which may create things in a service) and stores the result.

Every answer carries `x-ms-request-id`, `x-ms-correlation-request-id` (the client's value if it sent one),
`x-ms-routing-request-id`, `Cache-Control: no-cache`, `Pragma: no-cache`, `Expires: -1` and a rate limit header.

### Long-running operations

Everything finishes inside the request. A PUT answers with the final body and `provisioningState: "Succeeded"`;
the status codes per provider come from the REST API specs (for example Storage accounts 200, containers 201 then 200,
Event Grid topics 201, Cosmos DB deletes 204) because the Python SDK refuses codes the spec does not list. Where Azure
answers 202 with a status URL, the emulator does too and the URL answers "done" at once:

| Operation | Answer |
|---|---|
| Delete resource group | 202, `Location: .../subscriptions/{s}/operationresults/{id}?api-version=...` (GET answers 200) |
| Put deployment | 200/201 with the final body and `Azure-AsyncOperation: .../deployments/{n}/operationStatuses/{id}` (GET answers `{"status": "Succeeded"}` or `Failed` with the error). The header is what makes the SDK pollers raise for a failed deployment: a body that is already final is not polled |
| `.../operationresults/{id}`, `.../operationstatuses/{id}` anywhere | 200 `Succeeded` |

## Operations

Built unless the "Not built" column says otherwise.

### ARM basics

| Operation | Notes |
|---|---|
| `GET /subscriptions`, `GET /subscriptions/{id}` | the default subscription `00000000-0000-0000-0000-000000000000` plus every id used so far; `tenantId` is the identity service's default tenant |
| `GET /tenants` | one tenant |
| `GET /subscriptions/{id}/locations` | 27 regions with Azure's metadata fields |
| `GET /providers`, `GET /subscriptions/{id}/providers`, `GET .../providers/{ns}`, `POST .../providers/{ns}/register`, `/unregister` | 19 namespaces with resource types; every provider is `Registered` always; an unknown namespace is 400 `InvalidResourceNamespace` |
| `PUT/GET/HEAD/PATCH/DELETE /subscriptions/{id}/resourcegroups/{rg}` | 201 on create, 200 on update; `LocationRequired`, `InvalidResourceGroupLocation` (409), `InvalidResourceGroup`; location is normalised (`East US` becomes `eastus`); DELETE of a missing group is 404; HEAD 204/404 |
| `GET /subscriptions/{id}/resourcegroups[?$filter=tagName eq 'x']` | tag filters (`tagName`, `tagValue`) |
| `GET /subscriptions/{id}/resources`, `GET .../resourceGroups/{rg}/resources` | top-level resources of the scope; `$filter` on `resourceType`, `name`, `location`, `tagName`, `tagValue`; no paging (`nextLink` is never sent) |
| `GET/PUT/PATCH/DELETE {scope}/providers/Microsoft.Resources/tags/default` | scope is a subscription, a group (these are the group's tags) or a resource |
| `GET/PUT/DELETE /subscriptions/{id}/tagNames[/{name}[/tagValues/{value}]]` | |
| `GET .../providers/{ns}/operations` | empty list |
| Any `POST .../providers/{ns}/{type}/{name}/{action}` of a type with no provider | 501 naming it |

### Generic resources

A type from a namespace ARM has no provider for (Compute, Network, Web, Authorization, ...) is stored as JSON: PUT
stores the body, adds `id`, `name`, `type`, `properties.provisioningState: "Succeeded"` and (when missing) `location`
from the group; GET, HEAD, PATCH (tags replaced, properties merged), DELETE and list work; the parent must exist for a
child type (`ParentResourceNotFound`). With `AZEMU_ARM_STRICT=1` PUT, PATCH and POST answer 501. Nothing happens
behind these resources: a virtual machine PUT through ARM makes no virtual machine.

**Stored-only resources are shown**. Every answer for such a type (PUT, PATCH, GET, HEAD, DELETE, a list, and a deployment that
holds one) carries `x-azemu-stored-only: true`; the first write of each type logs one warning; `GET /_azemu/unsupported` lists the types under
`storedOnly` with the number of writes (`DELETE` on it clears the lists between two test runs, and the summary at shutdown prints them). Every 501
that ARM answers is counted there too, named by method and type without names (`PUT Microsoft.Web/sites`, `POST Microsoft.Web/sites/restart`).
Types a provider lists as settings only (`passthrough`, such as blob service settings) are not marked: they are the provider's.

For a type under a namespace that **has** a provider but that the provider does not build (for example
`Microsoft.Storage/storageAccounts/privateEndpointConnections`), PUT, PATCH and POST answer 501 naming the type, because
storing it would pretend a service exists. GET answers 404, a list answers an empty list, DELETE answers 204.
"Settings only" types such as `storageAccounts/managementPolicies` and `namespaces/networkRuleSets/default` are
stored as JSON. `Microsoft.Insights` and `Microsoft.OperationalInsights` are the exception to the 501 rule: they build `components` and `workspaces`
(below), and every other type of the two namespaces (action groups, alert rules, diagnostic settings, saved searches, ...) is stored only, as it was before.

### Providers wired to services

| Provider and type | What reaches the data plane | Other operations built | Not built |
|---|---|---|---|
| `Microsoft.Storage/storageAccounts` | nothing on create (Blob, Queue and Table make an account on first use); delete removes every container, queue and table of the account, and its file shares (deleted ones too) | `listKeys`, `listAccountSas`, `listServiceSas` (blob and queue), `regenerateKey`, `POST /providers/Microsoft.Storage/checkNameAvailability`, list by group and subscription, PATCH; `primaryEndpoints.{blob,queue,table,dfs,file}` = `http://{acct}.{svc}.core.localhost:{port}/`; the four default services (`blobServices`, `queueServices`, `tableServices`, `fileServices` named `default`) exist from creation; `blobServices/default` PUT, PATCH and GET drive versioning, CORS and delete retention (see "Blob service properties") | `failover`, blob `immutabilityPolicies`, `managementPolicies` content (stored only), private endpoints, encryption scopes (stored only) |
| `.../blobServices/default/containers/{c}` | `PUT /{c}?restype=container` with `x-ms-meta-*` and `x-ms-blob-public-access`; an existing container gets its metadata and ACL set; delete removes the container | 201 on create, 200 on update, list | `lease`, `immutabilityPolicy`, `legalHold` |
| `.../queueServices/default/queues/{q}` | create queue with metadata; delete | list | |
| `.../tableServices/default/tables/{t}` | create table; delete | list | |
| `.../fileServices/default` | settings only: PUT keeps what the request leaves out; the retention of deleted shares (`shareDeleteRetentionPolicy`) is stored and returned, and the file service has one global setting for it (`AZEMU_FILE_SOFT_DELETE_DAYS`), which it does not change | GET, list | |
| `.../fileServices/default/shares/{s}` | `PUT /{s}?restype=share` in the file service with `x-ms-share-quota`, `x-ms-access-tier` and `x-ms-meta-*`; an existing share gets its properties and metadata set; `signedIdentifiers` become the share's access policies; delete removes the share (soft delete of the file service applies). A share made through the file API is read by ARM (see "File shares") | 201 on create, 200 on update, list, GET | `enabledProtocols` NFS and the premium settings (`rootSquash`, provisioned sizes), `$expand=snapshots`, listing deleted shares (`$filter=deleted`), lease through ARM |
| `Microsoft.ServiceBus/namespaces` | the namespace is declared in the Service Bus state; delete removes the namespace with all queues, topics and messages | `authorizationRules` (+ `RootManageSharedAccessKey` at creation) with `listKeys` and `regenerateKeys`, `POST /providers/Microsoft.ServiceBus/checkNameAvailability`, PATCH; `serviceBusEndpoint` = `https://{ns}.servicebus.localhost:{port}/` | disaster recovery, migration, network rule sets (default only), private endpoints |
| `.../queues`, `.../topics`, `.../topics/subscriptions`, `.../subscriptions/rules` | create in the Service Bus state with the properties applied (lock duration, max delivery count, TTL, sessions, duplicate detection, dead lettering, forwarding, auto delete on idle, size limits, status); an existing entity is updated in place and keeps its messages; a subscription gets a `$Default` rule resource; rules compile SQL and correlation filters and SQL actions (a bad expression is 400); delete removes the entity | live `messageCount`, `sizeInBytes` and `countDetails` on queues, topics and subscriptions; `authorizationRules` and their keys on queues and topics | |
| `Microsoft.EventHub/namespaces` | the namespace is declared in the Event Hubs state; delete removes every event hub (and the Kafka groups of the namespace) | `authorizationRules` + `listKeys` + `regenerateKeys`, `checkNameAvailability`; `serviceBusEndpoint` = `https://{ns}.eventhub.localhost:{port}/` | clusters, capture, application groups |
| `.../eventhubs`, `.../eventhubs/consumergroups` | create hub (`partitionCount`, `messageRetentionInDays`); the partition count of an existing hub cannot change (400); a consumer group is added to the hub; delete | `$Default` consumer group resource, `authorizationRules` on a hub | |
| `.../schemagroups` | the group is made in the Schema Registry of the namespace (`schemaType`, `schemaCompatibility`, `groupProperties`; a bad value is 400); delete removes the group with its schemas ([`schema-registry.md`](schema-registry.md)) | | |
| `Microsoft.KeyVault/vaults` | nothing on create; delete removes all secrets, keys and certificates; `accessPolicies/{add,replace,remove}` change the vault's policy list | `checkNameAvailability`; `vaultUri` = `https://{name}.vault.localhost:{port}/`; purge of a "deleted" vault is accepted (200) and does nothing | `vaults/secrets`, `vaults/keys` (ARM-side secrets), managed HSMs, deleted vault listing, soft delete (a delete is final) |
| `Microsoft.DocumentDB/databaseAccounts` | nothing on create; delete removes every database | `listKeys`, `readonlykeys` (POST and GET), `listConnectionStrings`, `regenerateKey`; `documentEndpoint` = `https://{acct}.documents.localhost:{port}/` | failover, regions, metrics, backup, Mongo, Cassandra, Gremlin and Table APIs |
| `.../sqlDatabases`, `.../sqlDatabases/containers` | `POST /dbs`, `POST /dbs/{db}/colls` with partition key, indexing policy, TTL, unique keys, conflict resolution; throughput from `options.throughput` / `autoscaleSettings` is sent as the offer header; an existing container is replaced; deletes | `.../throughputSettings/default` is created when a throughput was given and can be read; `properties.resource` carries `_rid`, `_ts`, `_etag` from the Cosmos service | stored procedures, triggers, UDFs, client encryption keys, `migrateToAutoscale`; changing the throughput after creation does not change the offer |
| `Microsoft.AppConfiguration/configurationStores` | nothing on create; delete removes all settings | `listKeys` (4 keys with connection strings), `regenerateKey`, `checkNameAvailability`; `endpoint` = `https://{name}.azconfig.localhost:{port}`; purge of a "deleted" store is accepted | key values, replicas, snapshots, deleted store listing, soft delete |
| `Microsoft.Insights/components` | nothing is made: the component gets an instrumentation key, an application id and a connection string whose endpoints are this emulator's monitor service (see "Application Insights and Log Analytics") | GET, PATCH, DELETE, list by group and subscription, `currentbillingfeatures` (read and write, kept, not applied) | every other type of `Microsoft.Insights` is stored only |
| `Microsoft.OperationalInsights/workspaces` | nothing is made: the workspace is stored with a stable `customerId` | `sharedKeys` (POST), GET, PATCH, DELETE, list | queries, tables, saved searches, data exports (stored only), `regenerateSharedKey` |
| `Microsoft.EventGrid/topics` | `PUT /_azemu/eventgrid/topics/{name}` on the Event Grid admin API with the input schema and the real resource id (so events carry it as `topic`); delete | `listKeys` (also `listSharedAccessKeys`), `regenerateKey` (the topic on the Event Grid service takes exactly these keys: a publish with another key is 401); `endpoint` = `https://{name}.eventgrid.localhost:{port}/api/events` | domains, namespaces, partner topics |
| `Microsoft.EventGrid/systemTopics` | nothing: it names a source (`properties.source`: a storage account, a key vault or an App Configuration store that ARM has; `topicType` has to fit it or is filled in) | list by group and subscription, PATCH | system topics of other sources (501 naming the type) |
| `Event Grid event subscriptions` | in the Event Grid service, through `put_source_subscription` / the admin API (see "Event Grid subscriptions") | | |

### File shares

`Microsoft.Storage/storageAccounts/fileServices/shares` is joined to the file service (`crates/services/file`) through `Backends::file`, in the way containers are joined to
Blob: ARM calls the service's own REST API. The answers follow recordings of a real account (November 2026):

- A PUT answers with the properties it was given (`accessTier`, `metadata`, `shareQuota`), without `etag`; a GET with all of them (`accessTier`, `accessTierChangeTime`, `enabledProtocols`,
  `lastModifiedTime`, `leaseState`, `leaseStatus`, `metadata`, `shareQuota`, `signedIdentifiers`) and the `etag`; a list without `metadata` and `signedIdentifiers`.
- **The file service is the record.** Before every GET, PUT, PATCH, DELETE and list of a share, ARM reads the account's shares from the service: a share made with the file API (an SDK, `azcopy`,
  Terraform's `storage_account_name` form) gets an ARM record and is listed; a record whose share was deleted through the file API is dropped; quota, tier, metadata and times are the service's.
  This is true only for an account that is an ARM resource with `fileServices/default`; a share of an account ARM does not know is not listed anywhere in ARM. The access policies
  (`signedIdentifiers`) are written to the service on a PUT; ARM keeps the list it was given and does not read the service's access policies back.
- A name that is not 3 to 63 lower-case letters, digits and hyphens (starting with a letter or a digit, no `--`) is `400 InvalidResourceName`, "The specifed resource name contains invalid characters." (Azure's own words, with its spelling).
  A missing share is `404 ShareNotFound`, "The specified share does not exist." A DELETE of a share that is not there is 204.
- The service's own refusals come back as they are (a tier it does not know, a quota outside 1 to 102,400: 400). A DELETE of a share that has snapshots is `409` unless the request has `x-ms-snapshot: include`.
  Deleting the account or the group drops snapshots and breaks a lease.
- Deleting the account also removes the shares the service holds for it, the soft-deleted ones too.
- Not tried on Azure: the answer of a PATCH, the delete of a missing share, and the error for a name of the wrong length (it is the same `InvalidResourceName` here; the file API answers `OutOfRangeInput` for that).

### Application Insights and Log Analytics

`Microsoft.Insights/components` (shape: a recorded answer; the property names start with capitals, `type` is `microsoft.insights/components`, and so is the namespace in `id`):

- `InstrumentationKey` and `AppId` are GUIDs made from the resource id, so they stay the same across a second PUT, a restart and a PATCH. `AppId` is the id the monitor service answers for that key on
  `GET /api/profiles/{ikey}/appId` (both come from one function, `azemu_monitor::app_id`), so an SDK that looks the id up gets the same one as the resource shows.
- `ConnectionString` is `InstrumentationKey=...;IngestionEndpoint=http://{location}.in.localhost:{port}/;LiveEndpoint=http://{location}.livediagnostics.localhost:{port}/;ApplicationId=...`: the
  monitor service, in the host form it answers on the gateway port ([`monitor.md`](monitor.md)). It is plain `http`, as Application Insights ingestion takes no token. With `--standard-ports` in a container the
  hosts still carry the gateway port of the request that asked, as `primaryEndpoints` of a storage account do; write the string by hand there (`IngestionEndpoint=http://azemu:10015/`).
  The telemetry sent with it is read back with `GET /_azemu/monitor/telemetry?ikey=...`. `azurerm_application_insights.x.connection_string` is this string.
- The other properties are Azure's defaults and what the request gave: `Application_Type` (`web`), `kind`, `Flow_Type`, `Request_Source`, `IngestionMode` (`LogAnalytics` with a `WorkspaceResourceId`, else `ApplicationInsights`),
  `RetentionInDays` (90; 30 to 730) with `Retention` (`P90D`), `SamplingPercentage` (100.0), `DisableIpMasking`, `DisableLocalAuth`, the two `publicNetworkAccessFor...` (`Enabled`), `Ver` (`v2`), `TenantId`
  (here the subscription id), `CreationDate`, and a new `etag` at every write. `WorkspaceResourceId` is not checked against an existing workspace.
- `components/{c}/currentbillingfeatures` (GET, PUT: the daily data cap `azurerm_application_insights` writes) keeps what was written and limits nothing. Its shape is that of the 2015-05-01 API, not a recorded answer.

`Microsoft.OperationalInsights/workspaces` (shape: a recorded answer): `provisioningState` `Succeeded`, a `customerId` made from the resource id, `sku.name` written with Azure's capitals (`PerGB2018`), `retentionInDays` (30; 7 to 730),
`features`, `workspaceCapping` (`dailyQuotaGb` -1 unless set; `quotaNextResetTime` the next 22:00 UTC), `createdDate`, `modifiedDate`, `etag`. `POST .../sharedKeys` gives two stable 88-character keys. Nothing is ingested into a workspace and no query runs.
A workspace name is 4 to 63 letters, digits and hyphens, not starting or ending with a hyphen. Written without a recording: the `workspaceCapping` default, the name rules, the retention bounds.

### Event Grid subscriptions

Built in `src/eventsubs.rs`. Paths (API version does not matter; the shapes are those of `2025-02-15`):

| Path | Meaning |
|---|---|
| `{resource id}/providers/Microsoft.EventGrid/eventSubscriptions/{name}` (PUT, PATCH, GET, DELETE) | the form `azurerm_eventgrid_event_subscription` and most templates use. The resource is a storage account, a key vault, an App Configuration store (events the emulator raises) or a custom topic |
| `Microsoft.EventGrid/topics/{t}/eventSubscriptions/{name}` | the same subscription as the extension form on the topic (an alias; the id is the extension form) |
| `Microsoft.EventGrid/systemTopics/{st}/eventSubscriptions/{name}` | a child of a system topic; its source is the system topic's `properties.source`; the id is `.../systemTopics/{st}/eventSubscriptions/{name}` |
| `POST .../eventSubscriptions/{name}/getFullUrl`, `/getDeliveryAttributes` | the webhook URL (400 for another destination); the stored `deliveryAttributeMappings` |
| `GET {scope}/providers/Microsoft.EventGrid/eventSubscriptions`, `.../systemTopics/{st}/eventSubscriptions`, `.../topics/{t}/eventSubscriptions` | list of that scope |
| `GET /subscriptions/{s}[/resourceGroups/{g}]/providers/Microsoft.EventGrid/eventSubscriptions` | the "global" subscriptions (on storage accounts, vaults and stores) of the subscription or group |
| `GET .../providers/Microsoft.EventGrid/locations/{l}/eventSubscriptions` | the subscriptions on custom topics of that location |

What happens:

* **Sources** (storage account, key vault, App Configuration store, system topic): `EventGridService::put_source_subscription`
  with `SourceRef::from_resource_id(<the stored resource id>)`. Events therefore carry Azure's `topic` (the account's ARM id), and
  a Blob SDK upload to that account raises `Microsoft.Storage.BlobCreated` into the subscription. **Custom topics**: the admin
  API `PUT /_azemu/eventgrid/topics/{t}/subscriptions/{name}`; the topic is created with its real resource id, so events published
  to it carry that id as `topic`.
* **Destinations** `WebHook`, `StorageQueue` (`resourceId` is the storage account, plus `queueName`), `ServiceBusQueue`,
  `ServiceBusTopic`, `EventHub` (`resourceId` of the queue, topic or hub), `AzureFunction` (`resourceId` ending `…/sites/{app}/functions/{name}`:
  the emulator delivers to a Functions host it was told about, `docs/services/eventgrid.md`, "Azure Function destinations"). The names in the resource ids are the account or
  namespace names the data plane knows. The queue, topic or hub has to exist when an event is delivered, not when the
  subscription is made. `HybridConnection`, `MonitorAlert` and `NamespaceTopic` answer 501 naming what is missing.
* **Delivery properties, dead letters, identities** (all go to the Event Grid service and are read back in the ARM shape): `deliveryAttributeMappings` inside
  `destination.properties` (static, secret and dynamic; a secret value is not shown by GET, `getDeliveryAttributes` gives it); `deadLetterDestination`
  (`StorageBlob`, `resourceId` of the storage account and `blobContainerName`: dead letters are written to that container); `deliveryWithResourceIdentity`
  (`identity` and `destination`) and `deadLetterWithResourceIdentity` (`identity` and `deadLetterDestination`): stored, shown and used like the plain forms, no credentials are checked (the emulator
  checks none inside its own connections). A subscription has either `destination` or `deliveryWithResourceIdentity`. Checked in the Event Grid service: an identity of another type than
  `SystemAssigned` / `UserAssigned`, a `UserAssigned` one without `userAssignedIdentity`, more than 10 delivery properties.
* **Webhook handshake** runs inside the PUT, as on Azure: the validation event (Event Grid schema) or the `OPTIONS` request
  (CloudEvents schema) goes to the URL and the PUT answers when it is done. A webhook that does not answer correctly makes the PUT
  fail with 400 `Url validation` ("Webhook validation handshake failed for ...") and nothing is created (the code and text are
  written from memory of Azure's answer). A webhook that answers 200 without the code leaves the subscription at
  `provisioningState: AwaitingManualAction` until its validation URL is called (it then reads `Succeeded`).
* **Filters** (`subjectBeginsWith`, `subjectEndsWith`, `includedEventTypes`, `isSubjectCaseSensitive`, `advancedFilters`),
  `eventDeliverySchema` (`EventGridSchema`, `CloudEventSchemaV1_0`), `retryPolicy` (`maxDeliveryAttempts`,
  `eventTimeToLiveInMinutes`) are handed to the Event Grid service as they are.
* **Stored and returned, not applied**: `labels`, `expirationTimeUtc`, `maxEventsPerBatch`,
  `preferredBatchSizeInKilobytes`, `enableAdvancedFilteringOnArrays`.
* The webhook's full URL is not shown by GET (`endpointBaseUrl` is), as on Azure; `getFullUrl` gives it.
* PUT and PATCH answer 201, also for an existing subscription (the spec lists only 201); DELETE answers 200 (204 when it did not
  exist). PUT replaces the subscription; PATCH takes the update parameters (`destination`, `filter`, ... at the top of the body,
  or under `properties`) and merges them into the stored properties.
* The scope resource has to exist in ARM (404 otherwise). A resource of another type (a virtual machine, a web site, ...) answers
  501: nothing raises events for it. **Subscription and resource group scope** (`/subscriptions/{s}/providers/Microsoft.EventGrid/eventSubscriptions/{name}`)
  receive Azure Resource Manager events, which the emulator does not raise: PUT answers 501. Event Grid domains and partner topics
  answer 501 too.
* Deleting a subscription, its system topic, its resource (a storage account), its group, or resetting removes it from the Event
  Grid service; delivery stops. A subscription made through the admin API directly is not an ARM resource and is not listed.
* A template can contain `Microsoft.EventGrid/systemTopics` and `Microsoft.EventGrid/systemTopics/eventSubscriptions`. A template
  resource of type `Microsoft.EventGrid/eventSubscriptions` with a `scope` is not built (the template engine ignores `scope`) and
  answers 501.
* Without an Event Grid service behind ARM (`azemu-arm` alone) subscriptions are stored and nothing is delivered.

### Blob service properties

`PUT/PATCH/GET .../storageAccounts/{a}/blobServices/default`:

* `isVersioningEnabled` calls `BlobService::set_versioning_enabled(account, bool)`; GET reads `versioning_enabled(account)` live.
  **A new account made through ARM starts with versioning off**, as on Azure (the account's `blobServices/default` is created with
  `isVersioningEnabled: false` and the Blob service is told so). An account that ARM never made (the Blob SDK against
  `acct.blob.core.localhost`, `devstoreaccount1`) keeps the Blob service's own default, which is off as on Azure (`AZEMU_BLOB_VERSIONING=1`
  or `AZEMU_LENIENT=1` turns it on for such accounts). Turning versioning off keeps the versions that exist. Deleting an ARM account does not put its flag back to the
  default; it stays until `POST /_azemu/reset`.
* `cors.corsRules` (at most 5, the Blob service checks) and `deleteRetentionPolicy` (`enabled`, `days`, `allowPermanentDelete`) and
  `defaultServiceVersion` are written to the Blob service as Set Blob Service Properties does, so the Blob SDK's
  `get_service_properties()` shows them. The other direction is not read: a change made with the Blob SDK is not shown by ARM.
* Stored and returned, not applied: `containerDeleteRetentionPolicy` (new accounts show `enabled: true, days: 7`, as Azure does),
  `changeFeed`, `restorePolicy`, `lastAccessTimeTrackingPolicy`, `automaticSnapshotPolicyEnabled`. A property a PUT leaves out keeps
  its value. `cors` of the queue and table services is stored and returned only.

### Cosmos DB account properties

Any account property is stored and returned as sent (`enableAllVersionsAndDeletesChangeFeed`, `capabilities`, `capacity`, `cors`,
`ipRules`, `networkAclBypass`, `disableLocalAuth`, `enableAnalyticalStorage`, ...). What an account does not set comes back with
Azure's defaults (`enableBurstCapacity`, `enablePartitionMerge`, `minimalTlsVersion: Tls12`, `defaultIdentity`, ...). Nothing is
enforced: the Cosmos service behaves the same whatever the account says.

### Deployments

`PUT/GET/HEAD/DELETE .../providers/Microsoft.Resources/deployments/{name}` at group and subscription scope,
`POST .../validate`, `POST .../cancel` (409 `DeploymentCannotBeCancelled`: everything is finished), `POST .../exportTemplate`,
`GET .../deployments` (list), `GET .../deployments/{name}/operations[/{id}]` (also without `providers/Microsoft.Resources`,
the way older API versions address operations). A deployment runs synchronously and every resource goes through the same
code as a direct PUT, so a template that names a storage container creates it in the Blob service. Details:

- `properties.template` (an object or a JSON string), `properties.parameters` (`{name: {value}}`), `mode: Incremental`.
  Parameters get type checks, `defaultValue`, `allowedValues`, `minValue`, `maxValue`, `minLength`, `maxLength`; an unknown
  or missing parameter is 400 `InvalidTemplate`.
- `variables` (including variable `copy`), `resources` with `condition`, `copy` (`count`, `copyIndex()`), `dependsOn`,
  nested `resources`, names with `/` for child types, `resourceGroup` per resource. Resources whose `dependsOn` or
  `reference()` points at something not yet deployed wait for it; a cycle is 400 `InvalidTemplate`. `outputs` are
  evaluated at the end.
- Subscription-scope deployments create `Microsoft.Resources/resourceGroups` resources; a `Microsoft.Resources/deployments`
  resource inside a template runs as a nested deployment (its `properties.parameters` are evaluated in the outer scope;
  `expressionEvaluationOptions` scope `inner` is not built) and is stored as a deployment of its own.
- A resource that fails (bad name, a data plane error) stops the deployment with `provisioningState: "Failed"`,
  `properties.error` (`DeploymentFailed` with the resource error in `details`) and a failed operation entry. Resources
  deployed before it stay.
- Functions (the lists are in "Template functions" below): the string, array, object, numeric, logical, comparison, date, lambda,
  resource and scope functions of Microsoft's reference, `list*` functions, `environment()` and user-defined functions.
- **Limits** (`src/limits.rs`; each is 400 `InvalidTemplate` with the number in the message, wherever the expression is: an output, a property, a name).
  From Azure's documentation: an expression is at most 24,576 characters; 256 parameters, 512 variables, 800 resources (a copy loop counts every copy),
  64 outputs; `range` makes 0 to 10,000 numbers with `start + count` at most 2,147,483,647; a resource `copy.count` is 0 to 800; a resource definition is at most
  1 MB and the expanded template 4 MB. The azemu's own choices (Azure documents none): functions and indexes nest at most 64 deep in one expression and 128
  evaluation steps deep in all (variables referring to variables count); one value is at most 4 MiB and 100 levels deep; `format` widths at most 1024; `copy` in
  properties at most 10,000; a request may do 64 MiB of evaluation work in all (nested deployments share it); nested deployments at most 10 deep, child resources at
  most 8 deep, 10,000 resources per request. `add`, `sub`, `mul`, `div`, `mod`, `copyIndex`, `range` use checked 64-bit arithmetic: an overflow or a division by zero is
  `InvalidTemplate`. Each variable is evaluated once per template (and copy index) and reused; a variable that refers to itself is `InvalidTemplate`. `padLeft` is built.
- Not built, with an error that names the function or option: `mode: Complete` (501), `templateLink` and `parametersLink` (501: the emulator makes no
  outbound calls), Key Vault references in parameters (501), what-if (501), deployment stacks, `managementGroup()` (a management group deployment),
  `import` and `imports` of a `languageVersion` 2.0 template, and `scope` on a resource (it is read and ignored). `securestring` values are not shown in the
  output (as in Azure).

### Template functions

Source of every function: Microsoft Learn, "ARM template functions" (`azure-resource-manager/templates/template-functions-*`); the unit tests in
`src/template/tests_functions.rs` and `tests_lambda.rs` use the examples printed on those pages, with the page named above each one.

| Group | Built |
|---|---|
| String | `base64`, `base64ToString`, `base64ToJson`, `concat`, `contains`, `dataUri`, `dataUriToString`, `empty`, `endsWith`, `first`, `format`, `guid`, `indexOf`, `join`, `json`, `last`, `lastIndexOf`, `length`, `like`, `newGuid`, `padLeft`, `replace`, `skip`, `split`, `startsWith`, `string`, `substring`, `take`, `toLower`, `toUpper`, `trim`, `uniqueString`, `uri`, `uriComponent`, `uriComponentToString` |
| Array | `array`, `concat`, `contains`, `createArray`, `distinct`, `empty`, `first`, `flatten`, `indexFromEnd`, `indexOf`, `intersection`, `last`, `lastIndexOf`, `length`, `max`, `min`, `range`, `skip`, `take`, `tryGet`, `tryIndexFromEnd`, `union` |
| Object | `contains`, `createObject`, `empty`, `intersection`, `items` (sorted by key, as the page says), `json`, `length`, `null`, `objectKeys`, `shallowMerge`, `tryGet`, `union` |
| Lambda | `filter`, `groupBy`, `map`, `mapValues`, `reduce`, `sort`, `toObject`, with `lambda(...)` and `lambdaVariables(...)` |
| Numeric, logical, comparison | `add`, `copyIndex`, `div`, `float`, `int`, `max`, `min`, `mod`, `mul`, `sub`; `and`, `bool`, `false`, `if`, `not`, `or`, `true`; `coalesce`, `equals`, `greater`, `greaterOrEquals`, `less`, `lessOrEquals` |
| Date | `dateTimeAdd`, `dateTimeFromEpoch`, `dateTimeToEpoch`, `newGuid`, `utcNow` |
| Resource and scope | `deployer`, `deployment`, `environment`, `extensionResourceId`, `list*`, `managementGroupResourceId`, `parameters`, `pickZones`, `reference`, `references` (2.0), `resourceGroup`, `resourceId`, `subscription`, `subscriptionResourceId`, `tenant`, `tenantResourceId`, `variables`; a `variables` entry named `copy` makes array variables |

**`list*` functions.** `list{Name}(resource, apiVersion[, body])` is the POST action `{resource}/list{Name}`, and it runs the code that the direct POST runs (the
provider's `action`), so the answer is the same: storage `listKeys`, `listAccountSas` and `listServiceSas`; Service Bus and Event Hubs authorization rules
`listKeys`; Cosmos DB `listKeys` and `listConnectionStrings`; App Configuration `listKeys`; Event Grid topics `listKeys` and `listSharedAccessKeys`. `resource` is a
resource id, a name or (`languageVersion` 2.0) a symbolic name. A call waits, like `reference`, for the resource it names when that resource is in the same template. A
call that fails fails the deployment after it started, as Azure does (it makes the POST and reports what came back): `DeploymentFailed` with the error of the service in `details`
(a resource that is not in the template and does not exist: `ResourceNotFound`, "The Resource 'Microsoft.Storage/storageAccounts/x' under resource group 'g' was not found. ...";
a resource group or a type with no such action: `NotFound`, "No HTTP resource was found that matches the request URI '.../listKeys?api-version=...'"). A `list*` call is
allowed in the properties of a resource and in outputs; in a variable the template is refused when it is validated (`InvalidTemplate`, "The template function 'listKeys' is not
expected at this location"), and in a condition, a resource name or a copy count it is an `InvalidTemplate` too. The answer of an action is kept for the rest of the deployment (`listKeys` is evaluated once). `listAccountSas` and `listServiceSas`
sign with the account key and the signing code of the Blob and Queue services, `sv=2022-11-02`; `listServiceSas` is built for the blob and queue services (a table or file
canonicalized resource is 501). The data plane does not compare the signature of an account other than the development account (its keys are the emulator's own) but
reads what the token says (permissions, resource type, times).

**Dates.** `utcNow([format])` and `newGuid()` are allowed only in the default value of a parameter. In an output the deployment fails with `DeploymentOutputEvaluationFailed`
("The template function 'utcNow' is not expected at this location. The function can only be used with parameter default value expresions.", Azure's spelling); in a variable the
template is refused when it is validated (`InvalidTemplate`).
`dateTimeAdd(base, duration[, format])` takes an ISO 8601 date (`2020-04-07 14:53:14Z`, `2023-01-01T00:00:00Z`, `20200407T145314Z`, a date alone, a zone offset, fractions of a
second) and an ISO 8601 duration where a year is 365 days and a month 30 days (as the page says; `P1W` is refused: "The second parameter 'P1W' is not a valid ISO8601 Duration string"); without a format the answer is `yyyy-MM-ddTHH:mm:ssZ`. A zone offset in the base is ignored by `dateTimeAdd` (`2023-06-01T12:00:00+02:00` stays 12:00, written with `Z`) and used by `dateTimeToEpoch`. Formats are .NET
formats of a `DateTimeOffset` with the invariant culture (`K` and the end of `o` are `+00:00`, not `Z`): every standard format of the .NET page (`d D f F g G m M o O r R s t T u y Y`; `U` is refused, as Azure refuses it) and the custom specifiers `d dd ddd dddd f..fffffff
F..FFFFFFF g gg h hh H HH K m mm M MMM MMMM s ss t tt y..yyyyy z zz zzz : /`, quoted text, `\` and `%x`; every other character is copied as .NET does. A format
.NET does not know (`a`, `U`, five `d` in a row) is "The third parameter 'x' is not a valid DateTime format string". Every date is UTC.

**`environment()`.** The public cloud's object (`name`, `gallery`, `graph`, `portal`, `graphAudience`, `activeDirectoryDataLake`, `batch`, `media`, `sqlManagement`,
`vmImageAliasDoc`, `resourceManager`, `authentication`, `suffixes`) with the emulator's own addresses where it has a service (the host the client used for ARM,
`management.localhost:4577`, gives `localhost:4577`):

| Property | Value |
|---|---|
| `resourceManager` | `https://management.localhost:4577/` |
| `authentication.loginEndpoint` | `https://login.localhost:4577/` |
| `suffixes.storage` | `core.localhost:4577`, so `{account}.blob.{suffix}` is the account's Blob host (plain `http` here: the Blob endpoint of an account is `http`) |
| `suffixes.keyvaultDns` | `.vault.localhost:4577` |
| all others | the public cloud's value (`.azurecr.io`, `.database.windows.net`, `azurefd.net`, `https://graph.windows.net/`, ...) |

**Lambdas.** The lambda is read from the syntax tree and evaluated once per element; each call is charged to the request's work budget, so a lambda over a large
array can end in `The template is too complex to evaluate` like any other expression. Lambda variables are not visible in the variables of the template. `sort`
is stable. `toObject` with a duplicate key keeps the last one (a guess, see below), `groupBy` keys are the text of the lambda's answer.

**Work that is charged.** The type check of a parameter, of a user-defined function's argument and of a `languageVersion` 2.0 output charges each node of the value to the same work
budget as the evaluator (and each entry of an `allowedValues` list it is compared with): a check that needs too much ends in `The template is too complex to evaluate`. Looking up the names
of an object against a schema's `properties` takes time in proportion to both, not to their product. `like` takes time in proportion to the text and the pattern. `indexOf` and
`lastIndexOf` on a string answer a position in the text that was given also when a letter changes its length in lower case (`indexOf('İİx','x')` is `2`); an ISO 8601 duration that adds
up to more than a 64-bit number of seconds is refused as not a duration.

**User-defined functions.** `functions` with namespaces, called `namespace.name(...)` (the name is not case sensitive). A function sees only its own parameters
(`parameters('x')`); it cannot read `variables`, call `reference`, a `list*` function or another user-defined function, and its parameters have no default values
(each is an `InvalidTemplate` error that says so). Arguments and the result are checked against the declared types.

**`languageVersion` 2.0** (what Bicep writes for user-defined types and functions, and for symbolic names): `resources` as an object keyed by symbolic name
(`resources` as an array is refused in 2.0 and the object form without 2.0); `dependsOn`, `reference`, `references` and `list*` take symbolic names; `existing: true`
reads a resource and deploys nothing (`reference` waits until it is there); a resource with `copy` is addressed with `references('name')`; `definitions` with `$ref`
(`#/definitions/name`) in parameters, outputs and function parameters, with `type`, `allowedValues`, `minValue`, `maxValue`, `minLength`, `maxLength`, `nullable`, `items`,
`prefixItems`, `properties` (a member must be present unless it is `nullable`), `additionalProperties`, `discriminator`; a `nullable` parameter without a default is null; outputs with
`copy` (`count`, `input`). Outputs are checked against their types in a 2.0 template only. Not built: `import`/`imports`, `scope`, `metadata` on functions is ignored.

### Behaviour that could not be confirmed against Azure

Choices made without a recorded answer from Azure for the same input (the comparison below settled the others):

- `toObject` with two elements that give the same key keeps the last one (the comparison had no such case).
- A `list*` function with fewer than two arguments is `InvalidTemplate` at validation; a `list*` call that cannot work answers what the service answers (see "list*
  functions"), but only the two errors in the comparison (an account that does not exist, a resource group) are Azure's own words.
- `listAccountSas` and `listServiceSas` answer `sv=2022-11-02`, with the parameters in the order `sv ss srt sp st se sip spr sig`; the order and the signed version are not documented
  and were not compared (no `list*` result was compared).
- `pickZones` answers an empty array for every type and region: the emulator has no zone data.
- `environment()`: only the storage and Key Vault suffixes, `resourceManager` and `loginEndpoint` point at the emulator (the values of the public cloud were not compared).
- The error of Azure for a template refused at validation carries `additionalInfo` (type `TemplateViolation`, with the line, the position and the path); the emulator's does not.
- `languageVersion` 2.0: what Azure does with `existing` resources that do not exist, `references` in a loop and the other details of the 2.0 form that the comparison template does
  not have.

## Decisions about state

- **Reset.** `POST /_azemu/reset` resets every service; ARM resets its own store. Both sides are empty afterwards
  (tested for storage in `tests/storage.rs`).
- **ARM lists what ARM created.** A container made with the Blob SDK is not in the ARM container list (file shares are the exception: ARM reads the file service, see "File shares"). A GET of an ARM
  resource whose data plane object was deleted through the data plane still answers (the ARM record is the record);
  a later PUT of the same resource creates the object again.
- **Names are global**, as host names are: two storage accounts, vaults, namespaces, Cosmos accounts or topics with the same
  name cannot exist in different groups or subscriptions (409). Two kinds of resource may share a name (a storage account
  and a vault called `x`).
- **Keys** are derived from the resource id, so they are stable across restarts and `listKeys` repeats. `regenerateKey`
  changes the key ARM shows; the data planes accept any key for these accounts, so an older key keeps working. Storage keys
  are the keys the Blob/Queue/Table services derive for the account (generation 0), which is what they would verify.
- **Delete of a group** deletes members deepest first, calling each provider's delete (data plane included), then the group.
- **Delete of a parent** deletes its children the same way.
- **A delete removes everything or says why not**. `Provider::delete` returns a result. A member whose data-plane delete failed
  stays in ARM (the others that went away leave it) and the first failure is the answer: 409 with the data plane's error code. Every page of
  every list is read (containers and queues 5,000 a page, tables 1,000, Cosmos databases by continuation). A container delete through ARM
  of a leased container is `409 LeaseIdMissing`; deleting the account or the group breaks the lease first ("force": what goes with its parent
  is dropped, as Azure drops the account). The Storage resource provider spec lists only 200 and 204 for a container delete, so the status and
  code of the refusal are a choice, not Azure's documented answer. The Blob service refuses a container delete for a lease and also for a
  blob (or snapshot or version) under a legal hold (`409 ContainerHasLegalHold`) or under an immutability policy that has not run out (a locked
  one: `409 ContainerImmutabilityPolicyLocked`; an unlocked one: `409 BlobImmutableDueToPolicy`, a guess). ARM gives the Blob service's code back in
  its `409`; the account wipe breaks a lease first and is stopped by a hold or a policy the same way.
- **Group delete and a create that overlap** (`src/writers.rs`). Every write into a group takes a guard. A group delete first marks the group
  (new writes, and a group PUT, get `409 ResourceGroupBeingDeleted`), waits for the writes that are running (at most 60 s), then deletes the
  members. When the delete answers, the group and everything in it are gone and no create that was accepted can appear afterwards; a create
  after that is `404 ResourceGroupNotFound`. If a member cannot be deleted the group stays, open for writes again.
- **Deployment history** (`src/history.rs`): at most 800 deployments per resource group and 800 at subscription scope (the oldest are dropped,
  as Azure does), and at most 64 MB of deployment records per subscription (`AZEMU_ARM_DEPLOYMENT_HISTORY_MB`); the template is kept once.
- **Names and tags**: Event Hubs hubs, consumer groups and authorization rules and App Configuration store names follow Microsoft Learn's
  naming rules page; tags are limited to 50, with names up to 512 and values up to 256 characters (the error code is a guess).

## Differences from Azure

- **Reset.** `POST /_azemu/reset?service=arm&account=<subscription id>` forgets one subscription: its resource groups, resources, deployments and
  history, tags and tag names, and its entry in the list of subscriptions that were used. Other subscriptions stay. It is ARM's record that goes: what
  ARM made in the other services (blobs of a storage account, a queue, a vault) stays there; reset those services by account. A write or a group
  delete that is running is not waited for. Resource group is not a unit of its own: delete the group.
- Service Bus namespaces answer `type` as `Microsoft.ServiceBus/Namespaces` and everything below in lower case (`.../namespaces/queues/authorizationrules`),
  the location as the display name (`East US`) and 200 for a create, as the recordings of azure-mgmt-servicebus do. Other types are not checked against recordings.

- A Service Bus entity property that is not a valid ISO 8601 duration (`lockDuration`, `defaultMessageTimeToLive`, `duplicateDetectionHistoryTimeWindow`,
  `autoDeleteOnIdle`) is a 400 `InvalidRequestContent` naming the property. The code is a guess: no spec or recording under `refs/` shows one.
- A request body over 4.25 MiB (4 MiB template plus the wrapper) is refused with 413 `RequestEntityTooLarge` before it is read.

- Every operation is synchronous. See "Long-running operations".
- `uniqueString` is a base-32 form of a SHA-256 of the arguments, 13 characters like Azure's, but not the same value.
- No soft delete for vaults, stores or accounts: a delete is final, "purge" is accepted and does nothing.
- `systemData`, locks, policy and quotas do not exist. Role assignments are stored and served, and decide in `--strict-auth` mode only (below). Subscriptions never run out of quota.
- The resource provider list is a fixed catalog (19 namespaces); registration is a no-op.
- Locations list 27 regions. Any location string is accepted on a resource.
- Resource lists never page. `$top` and `$expand` are ignored.
- Tokens are not validated. A request with no `Authorization` header is refused.
- Connection strings handed out by `listKeys` end in `UseDevelopmentEmulator=true` (Service Bus, Event Hubs) so the
  Python SDKs use plain AMQP on the emulator port. The host is `{ns}.servicebus.localhost:{port}` or
  `{ns}.eventhub.localhost:{port}`; the second form is what makes the AMQP side choose Event Hubs.
- Storage account endpoints are plain `http`, as asked; Key Vault, App Configuration, Cosmos DB and Event Grid endpoints
  are `https` (their SDKs insist on it for bearer tokens). The same port answers both.
- `uniqueString`-style names and any other value Azure derives from a real region or a real DNS name differ.
- An Event Hubs namespace and a Service Bus namespace with the same name can both exist; the AMQP side tells them apart
  by the host label (`eventhub` or `servicebus`).

## Real traffic compared

The 34 `azure-mgmt-*` recordings under `refs/recordings` are replayed against ARM by `tools/replay` (a separate "arm" package in `fidelity/SCORES.md`;
the replay tool's README explains what is compared). They are almost all list calls and name checks made against old API versions. Entries that
still fail are: types that are not built (Event Grid domains, `deploymentScripts`), a delete that answers 202 on Azure, `provisioningState` values such as
`Creating` that the emulator never shows because it finishes at once, and the region list. The facts taken from the recordings: the subscription,
tenant, location and resource-list shapes, the headers Azure sends, the `//` in `Microsoft.Compute//availabilitySets`, the name-check answer
(`nameAvailable`, `reason`, `message`), and that `GET /providers/Microsoft.Resources/operations` is 404 on Azure (the emulator answers an empty list).

### Template functions compared with Azure

November 2026: nine templates with no resources (`crates/services/arm/tests/azure/`) were deployed to a real resource group, and their outputs, about 230 of them, are checked in
as `{template}.expected.json` (output name to value; `{"__raw__": X}` is an output that Azure answered with a type and no `value`). The test `azure_functions` deploys each template
to the emulator and compares every output; `refused.json` holds six outputs that Azure refused, with its error, and the test checks that the emulator refuses them with the same
sentence. Five small probes (`utcNow` in an output, `newGuid` and `listKeys` in a variable, `listKeys` on a resource group and on an account that does not exist) gave the
error texts above. What the comparison did not cover: the results of `list*` functions (their shape is the direct action's), anything with resources, and `languageVersion` 2.0
beyond one resource-free template.

What differed, and was changed in the emulator to what Azure answered:

- `equals`, `contains` on an array or a string, `distinct`, `union` and `intersection` compare strings with case (the older code did not). Object keys in `contains`, `startsWith`,
  `endsWith`, `indexOf` on a string, `less` and `greater` do not.
- `objectKeys` lists the keys in alphabetical order, like `items`.
- `K` and the end of `o` and `O` are `+00:00` (a `DateTimeOffset`); the standard format `U` and the duration `P1W` are refused.
- `dateTimeAdd` ignores a zone offset in its base (12:00+02:00 is 12:00); `dateTimeToEpoch` does not.
- `string(true())` is `True`; `string` of a float is refused ("expects its first parameter to be one of the following types 'Integer, String, Boolean, Object, Array'"); a number with a
  fraction in an expression is refused when the template is validated ("float point numbers are not supported").
- `dataUri` writes `charset=utf-8` (the page prints `utf8`).
- An output whose value is null has a type and no `value`.
- The errors above for `utcNow`, `newGuid` and `list*`.

Everything else in the nine templates matched without a change: the dates and their formats, every lambda example, the array, object and string functions of the pages, `uri` in all
its forms, user-defined functions and the `languageVersion` 2.0 template.

## Terraform

`terraform` 1.17.1 with the `azurerm` provider 4.81.0 (pinned by `tests/terraform/.terraform.lock.hcl`). It is a repeatable test:

```
cargo build -p azemu
.venv/bin/python tests/terraform/run.py                 # every case, about 14 minutes
.venv/bin/python tests/terraform/run.py base eventgrid  # some cases (--list names them, --keep keeps the work directory)
```

For each case in `tests/terraform/cases/` the runner starts `target/debug/azemu` on a free port with a temporary `--cert-dir`,
runs `init`, `apply`, `plan -detailed-exitcode` (it must say "No changes"), checks with the data plane SDKs that the things exist
and work, runs `destroy`, checks that no resource group is left, prints one line per step, and stops the emulator. The provider
is cached in `refs/terraform-plugin-cache` (not committed; the lock file lets `init` use the cache without the registry; the first
run downloads about 230 MB). It needs the network for that download, so it is not part of `tests/run_sdk_suites.py`. Run one runner
at a time (two `init`s on one cache collide).

The provider block (`tests/terraform/provider.tf`):

```hcl
provider "azurerm" {
  features {}
  metadata_host                   = "management.localhost:PORT"
  subscription_id                 = "11111111-2222-3333-4444-555555555555"
  tenant_id                       = "22222222-2222-2222-2222-222222222222"
  client_id                       = "33333333-3333-3333-3333-333333333333"
  client_secret                   = "anything"
  resource_provider_registrations = "none"
  storage_use_azuread             = false
}
```

**TLS.** `ca.pem` is a root that signs only for `localhost` names, and `SSL_CERT_FILE` replaces the system roots, so the runner builds
one file with the system roots plus `ca.pem` (`ca-bundle.pem` in its work directory) and gives it as `SSL_CERT_FILE` to the terraform
commands that talk to the emulator (apply, plan, destroy, output), not to `init` (which talks to the real registry). By hand:
`cat /etc/ssl/certs/ca-certificates.crt ~/.azemu/ca.pem > bundle.pem; SSL_CERT_FILE=$PWD/bundle.pem terraform apply`.
The provider reads `https://management.localhost:PORT/metadata/endpoints?api-version=2022-09-01` (it asks for
`//metadata/endpoints`, which the router matches by segments), which points it at the identity service
(`https://login.localhost:PORT/`, client secret flow, any secret) for tokens and at ARM for everything else.

**Result** (times are `apply` / `destroy` of the case; every case also had a clean second
plan, passing data plane checks and no resource group left):

| Case | Resources | Result | apply / destroy |
|---|---|---|---|
| `base` | `azurerm_resource_group`, `azurerm_storage_account`, `azurerm_storage_container` (with `storage_account_id`), `azurerm_storage_queue` (by name and by `storage_account_id`), `azurerm_storage_table`, `azurerm_storage_blob` | works | 91 s / 21 s |
| `servicebus` | namespace, queue (lock duration, max delivery count), topic, subscription, subscription rule (SQL filter) | works (after a fix, below) | 31 s / 71 s |
| `eventhubs` | namespace, event hub, consumer group | works | 31 s / 31 s |
| `keyvault` | `azurerm_key_vault` with an access policy (a secret is set and read with the Key Vault SDK) | works | 151 s / 21 s |
| `appconfig` | `azurerm_app_configuration` (a setting is written and read) | works | 31 s / 51 s |
| `cosmos` | account, SQL database (throughput 400), SQL container (an item is written and read) | works (after two fixes, below) | 81 s / 51 s |
| `eventgrid` | storage account, container, two queues, `azurerm_eventgrid_event_subscription` on the account (extension form, StorageQueue), `azurerm_eventgrid_system_topic`, `azurerm_eventgrid_system_topic_event_subscription` (subject filter, StorageQueue) | works: a blob uploaded with the Blob SDK arrives as `BlobCreated` in both queues, `topic` = the account's ARM id | 101 s / 41 s |
| `insights` | `azurerm_log_analytics_workspace`, `azurerm_application_insights` (a connection string that sends one event to the emulator, read back through `/_azemu/monitor/telemetry`) | works | 41 s / 31 s |
| `shares` | `azurerm_storage_share` in both forms (`storage_account_id` and `storage_account_name`), `azurerm_storage_share_directory`, `azurerm_storage_share_file` | works: both forms apply, the second plan is clean, Resource Manager lists the share made with the account key, the file Terraform wrote is read back with the SDK | 121 s / 21 s |
| `template` | `azurerm_resource_group_template_deployment` with a template (storage account, container, an output from `reference()`) | works | 21 s / 21 s |

What the provider needed from the ARM service, found by these cases: Service Bus subscriptions no longer store `clientAffineProperties`
unless `isClientAffine` (the provider sends an empty block and then saw a change that forces replacement); Cosmos DB
`HEAD /providers/Microsoft.DocumentDB/databaseAccountNames/{name}` (the name check before create) answers 200 or 404; a Cosmos
account answers `failoverPolicies` (the provider builds `geo_location` from them) and a default `backupPolicy` when the provider sends
`"backupPolicy": null` (a `null` is filled too). Event Grid resources and the other `Microsoft.EventGrid` paths are
described above.

**Why it is slow** (all of it is the provider waiting; the emulator answered every request in under 12 ms, checked in the emulator
log, which is `azemu.log` in the kept work directory):

* **Key Vault, about 2 minutes 30 s**: after the PUT the provider waits for the vault's own address with
  `StateChangeConf{Delay: 30s, PollInterval: 10s, ContinuousTargetOccurence: 10}` (`key_vault_resource.go`, `keyVaultRefreshFunc`):
  it sends `GET https://{vault}.vault.localhost:PORT/` and is satisfied by **any HTTP answer** (the emulator's 401, as Key
  Vault answers a request without a token, is accepted; only a connection error or a DNS failure counts as "pending"). It needs 10
  such answers in a row, 10 s apart, after a first wait of 30 s, plus one 10 s step after the PUT. Nothing the emulator
  answers can shorten that; it is the same on Azure (where DNS needs the time).
* **Storage account, about 1 minute 30 s with the rest of `base`**: after the PUT the log shows a gap of exactly 10.0 s before each of
  six successive checks (a GET of the account, data plane GETs on the blob, queue and table endpoints, a GET of `fileServices/default`), each answered at once.
  The provider sleeps 10 s before every poll. Returning `Retry-After: 0` on the PUT answer was tried and changed nothing. The resource
  group itself takes 20 s the same way (10 s, then two polls 5 s apart), and each delete adds 10 s steps (Service Bus namespace about 50 s,
  App Configuration about 30 s: a delete is followed by polls of a "deleted" list and a name check, 10 s apart).
* No other service is the cause: Blob, Queue, Table and Key Vault answer immediately.

Not tried: the `azapi` provider, `az` and Bicep, Terraform resources of types ARM does not build (for example Event Grid domains,
private endpoints; role assignments were tried with `tests/terraform/cases/authz`).

## How it drives the other services

Blob, Queue, Table, Cosmos DB, Service Bus and Event Hubs are driven through what they already expose (`Service::handle` for the HTTP APIs, the
public `Shared` state for Service Bus and Event Hubs, `BlobService::set_versioning_enabled` and `EventGridService::put_source_subscription`).
The whole emulator, ARM plus every service, is wired in one place: `azemu::full_stack()` in `crates/azemu/src/lib.rs`.

## Keeping data between runs

With `--data-dir` the service saves `arm.state` (subscriptions, groups, scope tags, tag names) and one `scope-<hash>.state` per resource group (resources
with their private data, deployments). ARM and the data-plane services are saved one after the other, so after a crash they can disagree: ARM lists what
ARM created, a GET of an ARM resource whose data-plane object is gone still answers, and nothing at load reaches into the data plane. A stored body holds no
address: endpoints (`serviceBusEndpoint`, `primaryEndpoints`, a topic's `endpoint`, a Cosmos account's `documentEndpoint`) are filled in when
the body is answered, and the port in a deployment's outputs is stored as `{azemu-port}` and replaced by the port of the request that reads it. The
files of this version stay readable: `tests/state-v1/arm/` and `tests/state_v1.rs`. Details: `docs/services/persistence.md`, section "Cosmos, Event Grid, Service Bus, Event Hubs with Kafka, ARM".

## Authorization rules and the data planes

The `authorizationRules` of Service Bus and Event Hubs (namespace, queue, topic and event hub level) are what the data planes check shared access keys against in strict mode
([`../strict-auth.md`](../strict-auth.md)). `listKeys` and the data plane use the same function for a key (derived from the rule's id and the number of times it was regenerated),
so a connection string from `listKeys` works in strict mode and a regenerated key takes effect at once; a rule that is deleted or whose `rights` change is looked at at the next check.
Nothing is copied: the data plane asks Resource Manager's state at every check.

## Role assignments and role definitions (`Microsoft.Authorization`)

Two types at any scope (subscription, resource group, resource, a part of a resource), kept in the registry that `--strict-auth` decides by ([`../strict-auth.md`](../strict-auth.md)) and answered whatever the mode is:

| Type | Operations |
|---|---|
| `roleAssignments` | `PUT` (201 new, 200 the same again), `GET`, `DELETE` (200, 204 for one that is not there) of `{scope}/providers/Microsoft.Authorization/roleAssignments/{guid}`; the list of a scope (`GET .../roleAssignments`: at and below the scope; `$filter=atScope()`: at and above; `$filter=principalId eq '{id}'` or `assignedTo('{id}')`: at, above and below, for that principal) |
| `roleDefinitions` | the built-in roles with their real ids and actions; `GET` by id, the list with `$filter=roleName eq '...'` or `type eq 'CustomRole'`, `PUT` and `DELETE` of a custom role |

Errors: an unknown role definition is 400 `RoleDefinitionDoesNotExist`; the same principal, role and scope under another name is 409 `RoleAssignmentExists`; a name that is taken by another principal or role is 409
`RoleAssignmentUpdateNotPermitted`; a principal id that is not a GUID is 400 `InvalidRequestContent`; a resource group that does not exist is 404 `ResourceGroupNotFound`, a resource that does not exist 404 `ResourceNotFound`
(the parts below a top-level resource, a secret or a container, are not looked up). A role that assignments use cannot be deleted (400 `RoleDefinitionHasAssignments`), a built-in role cannot be changed. These codes and texts
come from the documentation and from memory: the management plane was not probed (see "Could not be confirmed" in the strict authorisation page). A storage account, vault, store, topic, namespace and Cosmos DB account that ARM makes
tells the registry its id, so that a scope at the group or the subscription covers it; for a vault it also says whether it uses roles or `accessPolicies`.

`Microsoft.DocumentDB/databaseAccounts/sqlRoleDefinitions` and `sqlRoleAssignments` are built (Cosmos DB's own role system; a new account has the two built-in definitions `00000000-0000-0000-0000-000000000001` Reader and `...0002` Contributor).
Resource Manager itself is **not** checked in strict mode: any token may call it.
