# Storage events into Event Grid (storage-events)

**What it emulates.** The Azure form of "S3 event notifications": Blob Storage and Data Lake raise events (`Microsoft.Storage.BlobCreated`, `BlobDeleted`,
`DirectoryCreated`, ...) into Event Grid, and an Event Grid subscription on the storage account delivers them to a webhook, a Storage queue, a Service Bus
queue or topic, or an event hub, in Azure's event format (Event Grid schema or CloudEvents), with filters. Key Vault and App Configuration raise events the
same way (smaller; see the end).

**How to reach it.** Subscribe through the Event Grid admin API (`PUT /_azemu/eventgrid/sources/storage/{account}/subscriptions/{name}`, below) or through
ARM (`Microsoft.EventGrid/systemTopics/eventSubscriptions`, Terraform `azurerm_eventgrid_event_subscription`). It works in the `azemu` binary only: the
standalone `azemu-blob` and `azemu-eventgrid` are not connected to each other.

**Not built.** See "Not built" below: one event per child of a recursively deleted directory, SFTP, lifecycle and inventory events; Key Vault certificate and expiry events;
batching and Relay destinations. Which Blob operations raise events was **checked against a real storage account in November 2026** (below): everything that raised nothing there raises nothing here.

## How to use it

1. Start `azemu`. Create what the events should land in (a queue, a Service Bus queue / topic, an event hub) or run a
   webhook.
2. Subscribe to the storage account through the admin API (the ARM layer, when it is mounted, does the same through
   `Microsoft.EventGrid/systemTopics/eventSubscriptions`):

```bash
# to a webhook
curl -X PUT localhost:4577/_azemu/eventgrid/sources/storage/myaccount/subscriptions/hook \
  -H 'Content-Type: application/json' -d '{"webhookUrl":"http://localhost:7071/api/hook"}'

# to a Storage queue (same account here), only new blobs under "inbox/", as CloudEvents
curl -X PUT localhost:4577/_azemu/eventgrid/sources/storage/myaccount/subscriptions/toq -H 'Content-Type: application/json' -d '{
  "destination": {"endpointType":"StorageQueue","storageAccount":"myaccount","queueName":"blob-events"},
  "deliverySchema": "CloudEventSchemaV1_0",
  "filter": {"includedEventTypes":["Microsoft.Storage.BlobCreated"],
             "subjectBeginsWith":"/blobServices/default/containers/inbox/"}}'

# to Service Bus (queue or topic) and to an event hub
... -d '{"destination":{"endpointType":"ServiceBusQueue","namespace":"myns","queueName":"events"}}'
... -d '{"destination":{"endpointType":"ServiceBusTopic","namespace":"myns","topicName":"events"}}'
... -d '{"destination":{"endpointType":"EventHub","namespace":"myns","eventHubName":"blob-events"}}'
```

The same JSON works in the config file (`AZEMU_EVENTGRID_CONFIG`, key `storageAccounts`), so a project can keep its wiring
in git. Filters (event types, `subjectBeginsWith`, `subjectEndsWith`, advanced filters), the two delivery schemas (Event
Grid schema and CloudEvents 1.0), retries and dead letters are those of every Event Grid subscription:
`docs/services/eventgrid.md`.

3. Use the account with any Blob SDK (`http://myaccount.blob.core.localhost:4577`) or Data Lake SDK
   (`http://myaccount.dfs.core.localhost:4577`). Events are raised for the account named in the host (any account name).

Rules:

* Nothing is built or kept for an account nobody subscribed to. A subscription gets events raised after it exists.
* Only operations that succeeded raise events.
* Raising an event never slows or fails the blob request: the event is built from the finished operation, handed over after
  the blob lock is released, and delivered by a separate task.
* `POST /_azemu/reset` removes the subscriptions (the config file is read again).
* Events of different blobs, and the deliveries of one event to several subscriptions, are independent tasks: arrival order
  is not guaranteed. The `sequencer` orders events (Learn: "Users can use standard string comparison to understand the
  relative sequence of two events on the same blob name").

## Event shapes

Sources: the system event schemas in `refs/specs/specification/eventgrid/data-plane/EventGridSystemEvents`
(`Microsoft.Storage/Storage.tsp`, the examples in `examples/2024-01-01/`) and Microsoft Learn, "Azure Blob Storage as an
Event Grid source". Where they differ, the choice is written down below.

Envelope (Event Grid schema): `id` (UUID), `topic`, `subject`, `eventType`, `eventTime` (the request time), `data`,
`dataVersion`, `metadataVersion` (`1`). CloudEvents: `specversion` `1.0`, `id`, `source` (= the topic), `subject`, `type`,
`time`, `data`; an empty `dataVersion` is not sent.

* `topic` / `source`: `/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/azemu/providers/Microsoft.Storage/storageAccounts/{account}`.
  The subscription and resource group are made up (the emulator has none) unless the subscription was created through the
  Rust API with a real resource id, which the ARM layer does.
* `subject`: `/blobServices/default/containers/{container}/blobs/{blob}` (the blob name as is, also for Data Lake paths).
* `data.url`: built from the URL the client used (`http://acct.blob.core.localhost:4577/container/blob`, name
  percent-encoded), not `core.windows.net`. Data Lake events use the `dfs` host, with `blobUrl` / `sourceBlobUrl` /
  `destinationBlobUrl` on the `blob` host where the schema has them.
* `data.requestId`: the `x-ms-request-id` of the response. `data.clientRequestId`: `x-ms-client-request-id` of the request,
  left out when the client sent none. `data.sequencer`: 48 characters, opaque, increasing (process wide).
  `data.storageDiagnostics.batchId`: a random UUID.
* `data.eTag`: the blob's ETag without quotes (`0x8D…`, as the examples).

| Event | Raised by (`data.api`) | `dataVersion` | `data` fields |
|---|---|---|---|
| `Microsoft.Storage.BlobCreated` | Put Blob of any blob type (`PutBlob`; an append blob or page blob too, with `blobType` and its `contentLength`: 0, or the declared size), Put Blob From URL (`PutBlobFromUrl`, name assumed), Put Block List (`PutBlockList`), Copy Blob that finished (`CopyBlob`) | `""` | `api`, `clientRequestId`, `requestId`, `eTag`, `contentType`, `contentLength`, `blobType`, `accessTier`, `url`, `sequencer`, `storageDiagnostics` |
| `Microsoft.Storage.BlobDeleted` | Delete Blob of the base blob (`DeleteBlob`), with or without `x-ms-delete-snapshots: include` | `""` | `api`, `clientRequestId`, `requestId`, `eTag`, `contentType`, `contentLength`, `blobType`, `url`, `sequencer`, `storageDiagnostics` |
| `Microsoft.Storage.BlobTierChanged` | Set Blob Tier (`SetBlobTier`); not in the default set | `""` | `api`, `clientRequestId`, `requestId`, `eTag`, `contentType`, `contentLength`, `blobType`, `accessTier`, `previousTier` (only when the blob had an explicit tier), `url`, `sequencer`, `storageDiagnostics` |
| `Microsoft.Storage.AsyncOperationInitiated` | Set Blob Tier out of Archive (answer 202, rehydration pending); not in the default set | `""` | as BlobTierChanged without `accessTier` and `previousTier` |
| `Microsoft.Storage.BlobCreated` (Data Lake) | Create path, file (`CreateFile`); Flush with `close=true` (`FlushWithClose`) | `"2"` | as BlobCreated plus `contentOffset` (0 for `CreateFile`, the `position` for `FlushWithClose`); `url` on the `dfs` host |
| `Microsoft.Storage.BlobDeleted` (Data Lake) | Delete path, file (`DeleteFile`) | `"2"` | `api`, `clientRequestId`, `requestId`, `contentType`, `blobType`, `url` (dfs), `sequencer`, `storageDiagnostics` (not tried on Azure: no `eTag` or `contentLength`, unlike the blob endpoint) |
| `Microsoft.Storage.BlobRenamed` | Rename path, file (`RenameFile`); subject is the new name | `"1"` | `api`, `clientRequestId`, `requestId`, `destinationBlobUrl`, `sourceBlobUrl`, `destinationUrl`, `sourceUrl`, `sequencer`, `storageDiagnostics` |
| `Microsoft.Storage.DirectoryCreated` | Create path, directory (`CreateDirectory`) | `"1"` | `api`, `clientRequestId`, `requestId`, `eTag`, `blobUrl`, `url` (dfs), `sequencer`, `storageDiagnostics` |
| `Microsoft.Storage.DirectoryRenamed` | Rename path, directory (`RenameDirectory`) | `"1"` | as BlobRenamed |
| `Microsoft.Storage.DirectoryDeleted` | Delete path, directory (`DeleteDirectory`) | `"1"` | `api`, `clientRequestId`, `requestId`, `blobUrl`, `url` (dfs), `recursive` (the string `"true"` / `"false"`), `sequencer`, `storageDiagnostics` |

Of these events only `BlobCreated` and `BlobDeleted` are in the default set ("The default set of event types" above); a subscription has to name the others (`BlobTierChanged`,
`AsyncOperationInitiated`, `DirectoryCreated`, `DirectoryDeleted`, `BlobRenamed`, `DirectoryRenamed`). The Data Lake rows were not tried on Azure (that account has no subscription).

`dataVersion` values are those of the Learn examples (`""` on the blob endpoint, `"2"` for Data Lake file create and
delete, `"1"` for the other Data Lake events). A request that renames a path onto itself raises nothing.

Where the sources disagree, and what was followed:

* `accessTier` on `BlobCreated`: required in `Storage.tsp` and in the spec example, in the Learn CloudEvents example, absent
  from the Learn Event Grid example and from the property table ("Appears only for the event BlobTierChanged"). Sent, with
  the blob's explicit tier or `"Default"`. Not sent on `BlobDeleted` (not in the model).
* `eTag` and `contentLength` on `BlobDeleted`: in the spec's example JSON, not in `Storage.tsp` and not in Learn. **Azure sent both** for Delete Blob (November 2026), so the blob endpoint sends
  both (the Data Lake delete does not: not tried). `eTag` is in `BlobTierChanged` and `AsyncOperationInitiated` too: not in the sources either, Azure sent it.
* `DirectoryCreated`: `Storage.tsp` has `eTag`, the spec example also has `directoryProperties` (an ACL list) and `blobUrl`,
  Learn has neither. `eTag` and `blobUrl` are sent; `directoryProperties` is not (its `acl.access` is written in an
  abbreviated form no document explains).
* `BlobRenamed` / `DirectoryRenamed`: the `sourceBlobUrl` / `destinationBlobUrl` pair is in the spec example only. Sent.
* `NBF` / `EXP` (Key Vault, below): the spec example and Learn example have strings, `KeyVault.tsp` has numbers and the
  Learn table says number. Strings are sent.
* Not given by any source, so chosen here: `id` is a random UUID (Azure's looks like the request id); `sequencer` format;
  the batch id.

## The default set of event types

A subscription on a storage account that names no `includedEventTypes` gets only the default set of the account's topic type: `Microsoft.Storage.BlobCreated` and `Microsoft.Storage.BlobDeleted`. The other types
(`BlobTierChanged`, `AsyncOperationInitiated`, `DirectoryCreated`, `DirectoryDeleted`, `BlobRenamed`, `DirectoryRenamed`, `BlobInventoryPolicyCompleted`, `LifecyclePolicyCompleted`) are delivered only when the subscription lists them,
as on Azure (Microsoft's event type list for `Microsoft.Storage.StorageAccounts` marks just those two as default). The emulator does the same for subscriptions on a storage account, by the admin API,
the config file and ARM. **Key Vault and App Configuration sources, and custom topics, get every event** when no types are named: no default set for them was found in Microsoft's documentation.
Add the types you want to `includedEventTypes`, for example `["Microsoft.Storage.BlobCreated", "Microsoft.Storage.BlobTierChanged"]`.

## Which operations raise an event (checked against Azure in November 2026)

A system topic on a storage account sent every one of its event types to a Storage queue; each operation below was done once (or more) on a blob in a new container, and the queue was read for several minutes. The account had
soft delete off. (The first try named no event types, so the queue got only the default set and the tier events were missed; the second try named all ten and got them.)

| Operation | What came |
|---|---|
| Put Blob (block blob; overwrite of an existing blob too) | `BlobCreated`, `api` `PutBlob` |
| Put Blob, append blob; Put Blob, page blob | `BlobCreated`, `api` `PutBlob`, `blobType` `AppendBlob` with `contentLength` 0, `PageBlob` with the declared size |
| Put Block List | `BlobCreated`, `api` `PutBlockList` |
| Copy Blob (to a new blob, from a SAS URL) | `BlobCreated`, `api` `CopyBlob`, the content type of the source |
| Delete Blob (also with `x-ms-delete-snapshots: include` for a blob with snapshots) | `BlobDeleted`, `api` `DeleteBlob`, with `eTag` and `contentLength` |
| Create Container, Set Container Metadata, Delete Container | nothing (no event type exists for them) |
| Snapshot Blob, Delete Blob of a snapshot, Delete Blob with `x-ms-delete-snapshots: only` | nothing |
| Set Blob Metadata, Set Blob Properties, Set Blob Tags | nothing |
| Set Blob Tier (to Cool, to Hot, to Archive) | `BlobTierChanged`, `api` `SetBlobTier`; `eTag`, `contentType`, `contentLength`, `blobType`, `accessTier`, `previousTier` (when the blob had an explicit tier; absent for the first change), `url` |
| Set Blob Tier out of Archive (answer 202) | `AsyncOperationInitiated`, `api` `SetBlobTier`; `eTag`, `contentType`, `contentLength`, `blobType`, `url` (no `accessTier`, no `previousTier`) |
| Put Block, Append Block, Put Page, Clear Pages | nothing |
| Lease (acquire, release) | nothing |
| Undelete Blob | not tried: soft delete is off on the account, the answer was 404 |

The fields of these events are in the first table (`crates/services/blob/tests/events.rs` checks them). Differences that remain: Azure's `id` looks like a request id, the `url` has `core.windows.net`, the
`sequencer` is another string, `requestId` and `batchId` are Azure's. An event of a blob written by Event Grid itself as a dead letter is not raised here (Azure raises one; `docs/services/eventgrid.md`, "Dead letters as blobs").

## Destinations

How a message looks in each destination (details and sources in `docs/services/eventgrid.md`, "Destinations"):

| Destination | What arrives |
|---|---|
| Webhook | `POST` of a JSON array with the event (Event Grid schema) or the event itself (CloudEvents), `aeg-*` headers |
| Storage queue | the event JSON (one event), base64 encoded, as the message text; time to live 7 days by default, `queueMessageTimeToLiveInSeconds` (`-1`: never) |
| Service Bus queue / topic | body = event JSON, `content-type` `application/json; charset=utf-8`, `MessageId` = event id (or the mapped one), `aeg-*` application properties, delivery properties (`SessionId`, `PartitionKey`, …) |
| Event hub | body = event JSON, content type, `aeg-*` application properties; partition chosen round robin, or from the mapped `PartitionKey` |
| Azure Function | the call a webhook gets, to the Functions host the emulator was told about (`docs/services/eventgrid.md`, "Azure Function destinations") |

The queue, topic or hub has to exist. Delivery uses the retry and dead-letter rules of the subscription
(`retryPolicy`); a missing entity is retried and then dead-lettered (`GET …/deadletters`, `GET …/deliveries`).

The wiring is in `crates/azemu/src/lib.rs`: `BlobService::set_event_sink`, `KeyVaultService::set_event_sink`,
`AppConfigService::set_event_sink` send events to `EventGridService::raise_source_event`; the destinations are registered with
`EventGridService::register_destination` and call `QueueService::enqueue_base64`, `servicebus::Shared::enqueue_message` and
`eventhubs::Shared::enqueue_event` (new `enqueue.rs` files in those crates; no existing code changed).

## Key Vault and App Configuration

Subscribe with `/sources/keyvault/{vault}/subscriptions/{sub}` or `/sources/appconfig/{store}/subscriptions/{sub}`.

* Key Vault: `Microsoft.KeyVault.SecretNewVersionCreated` (Set Secret) and `Microsoft.KeyVault.KeyNewVersionCreated` (Create Key,
  Rotate Key, Import Key). Subject = the object name; `dataVersion` `1`; `data`: `Id` (the object URL with the version),
  `VaultName`, `ObjectType` (`Secret` / `Key`), `ObjectName`, `Version`, `NBF`, `EXP` (strings of seconds since 1970, left out
  when the object has no such date). Source: `examples/2024-01-01/secret_new_version_created.json`, Learn "Azure Key Vault
  as Event Grid source". Topic: `…/providers/Microsoft.KeyVault/vaults/{vault}`.
* App Configuration: `Microsoft.AppConfiguration.KeyValueModified` (Set key-value) and `…KeyValueDeleted` (Delete key-value).
  Subject = `{endpoint}/kv/{key}?label={label}`; `dataVersion` `1` (no source gives a value); `data`: `key`, `label` (null
  without a label), `etag` (no quotes), `syncToken` (the `Sync-Token` of the answer). Source: `keyvalue_modified.json`,
  `keyvalue_deleted.json`. Topic: `…/providers/Microsoft.AppConfiguration/configurationStores/{store}`.

## Not built

* Storage: `AsyncOperationInitiated` / `BlobCreated` for Copy Blob on a hierarchical namespace that does not finish at once
  (copies here finish at once); events for versions, Undelete (not tried on Azure: soft delete was off), immutability and legal hold changes (not tried); one event per child of a recursively deleted
  or renamed directory (one `DirectoryDeleted` / `DirectoryRenamed` only); `directoryProperties`; SFTP events; lifecycle,
  inventory and storage task events; `BlobCreated` for `FlushWithClose` without `close`.
  Snapshots, Set Blob Metadata / Properties / Tags, Append Block, Put Page, leases and container operations raise nothing, as on Azure
  (table above); Delete of a snapshot or a version, and Delete Blob with `x-ms-delete-snapshots: only`, raise nothing.
* Events of the `$logs` and `$blobchangefeed` containers are not raised (Learn).
* Key Vault: certificate events, near-expiry and expiry events, `VaultAccessPolicyChanged`. App Configuration: snapshot events,
  lock / unlock.
* Event Grid: batching and Relay destinations.
* Standalone binaries: `azemu-blob` has the event hook but nothing listens; `azemu-eventgrid` refuses Storage queue, Service
  Bus and Event Hubs destinations with 501.
* Storage queue message encoding: no Microsoft page states that the message is base64 encoded or a single event rather than an
  array; this follows the CycleCloud guide (reads with `TextBase64DecodePolicy`) and the other destinations. Likewise the
  `application/json` content type of a Service Bus message is not stated on the handler page.
