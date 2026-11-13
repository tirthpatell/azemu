# Storage events into Event Grid (storage-events)

**What it emulates.** The Azure form of "S3 event notifications": Blob Storage and Data Lake raise events (`Microsoft.Storage.BlobCreated`, `BlobDeleted`,
`DirectoryCreated`, ...) into Event Grid, and an Event Grid subscription on the storage account delivers them to a webhook, a Storage queue, a Service Bus
queue or topic, or an event hub, in Azure's event format (Event Grid schema or CloudEvents), with filters. Key Vault and App Configuration raise events the
same way (smaller; see the end).

**How to reach it.** Subscribe through the Event Grid admin API (`PUT /_azemu/eventgrid/sources/storage/{account}/subscriptions/{name}`, below) or through
ARM (`Microsoft.EventGrid/systemTopics/eventSubscriptions`, Terraform `azurerm_eventgrid_event_subscription`). It works in the `azemu` binary only: the
standalone `azemu-blob` and `azemu-eventgrid` are not connected to each other.

**Not built.** See "Not built" below: events for snapshots, versions, Set Blob Metadata, Put Page and Append Block, one event per child of a recursively deleted
directory, SFTP, lifecycle and inventory events; Key Vault certificate and expiry events; delivery with a managed identity, dead-lettering to a blob container,
batching, Azure Functions and Relay destinations.

## How to use it

1. Start `azemu`. Create what the events should land in (a queue, a Service Bus queue / topic, an event hub) or run a
   webhook.
2. Subscribe to the storage account through the admin API (the ARM layer, when it is mounted, does the same through
   `Microsoft.EventGrid/systemTopics/eventSubscriptions`):

```bash
# to a webhook
curl -X PUT localhost:4577/_azemu/eventgrid/sources/storage/myaccount/subscriptions/hook \
  -d '{"webhookUrl":"http://localhost:7071/api/hook"}'

# to a Storage queue (same account here), only new blobs under "inbox/", as CloudEvents
curl -X PUT localhost:4577/_azemu/eventgrid/sources/storage/myaccount/subscriptions/toq -d '{
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
| `Microsoft.Storage.BlobCreated` | Put Blob (`PutBlob`), Put Blob From URL (`PutBlobFromUrl`, name assumed), Put Block List (`PutBlockList`), Copy Blob that finished (`CopyBlob`) | `""` | `api`, `clientRequestId`, `requestId`, `eTag`, `contentType`, `contentLength`, `blobType`, `accessTier`, `url`, `sequencer`, `storageDiagnostics` |
| `Microsoft.Storage.BlobDeleted` | Delete Blob of the base blob (`DeleteBlob`) | `""` | `api`, `clientRequestId`, `requestId`, `contentType`, `blobType`, `url`, `sequencer`, `storageDiagnostics` |
| `Microsoft.Storage.BlobTierChanged` | Set Blob Tier (`SetBlobTier`) | `""` | `api`, `clientRequestId`, `requestId`, `contentType`, `contentLength`, `blobType`, `accessTier`, `previousTier` (only when the blob had an explicit tier), `url`, `sequencer`, `storageDiagnostics` |
| `Microsoft.Storage.AsyncOperationInitiated` | Set Blob Tier out of Archive (answer 202, rehydration pending) | `""` | as BlobTierChanged without `accessTier` and `previousTier` |
| `Microsoft.Storage.BlobCreated` (Data Lake) | Create path, file (`CreateFile`); Flush with `close=true` (`FlushWithClose`) | `"2"` | as BlobCreated plus `contentOffset` (0 for `CreateFile`, the `position` for `FlushWithClose`); `url` on the `dfs` host |
| `Microsoft.Storage.BlobDeleted` (Data Lake) | Delete path, file (`DeleteFile`) | `"2"` | `api`, `clientRequestId`, `requestId`, `contentType`, `blobType`, `url` (dfs), `sequencer`, `storageDiagnostics` |
| `Microsoft.Storage.BlobRenamed` | Rename path, file (`RenameFile`); subject is the new name | `"1"` | `api`, `clientRequestId`, `requestId`, `destinationBlobUrl`, `sourceBlobUrl`, `destinationUrl`, `sourceUrl`, `sequencer`, `storageDiagnostics` |
| `Microsoft.Storage.DirectoryCreated` | Create path, directory (`CreateDirectory`) | `"1"` | `api`, `clientRequestId`, `requestId`, `eTag`, `blobUrl`, `url` (dfs), `sequencer`, `storageDiagnostics` |
| `Microsoft.Storage.DirectoryRenamed` | Rename path, directory (`RenameDirectory`) | `"1"` | as BlobRenamed |
| `Microsoft.Storage.DirectoryDeleted` | Delete path, directory (`DeleteDirectory`) | `"1"` | `api`, `clientRequestId`, `requestId`, `blobUrl`, `url` (dfs), `recursive` (the string `"true"` / `"false"`), `sequencer`, `storageDiagnostics` |

`dataVersion` values are those of the Learn examples (`""` on the blob endpoint, `"2"` for Data Lake file create and
delete, `"1"` for the other Data Lake events). A request that renames a path onto itself raises nothing.

Where the sources disagree, and what was followed:

* `accessTier` on `BlobCreated`: required in `Storage.tsp` and in the spec example, in the Learn CloudEvents example, absent
  from the Learn Event Grid example and from the property table ("Appears only for the event BlobTierChanged"). Sent, with
  the blob's explicit tier or `"Default"`. Not sent on `BlobDeleted` (not in the model).
* `eTag` and `contentLength` on `BlobDeleted`, `eTag` on `BlobTierChanged`: in the spec's example JSON, not in `Storage.tsp`
  and not in Learn. Not sent.
* `DirectoryCreated`: `Storage.tsp` has `eTag`, the spec example also has `directoryProperties` (an ACL list) and `blobUrl`,
  Learn has neither. `eTag` and `blobUrl` are sent; `directoryProperties` is not (its `acl.access` is written in an
  abbreviated form no document explains).
* `BlobRenamed` / `DirectoryRenamed`: the `sourceBlobUrl` / `destinationBlobUrl` pair is in the spec example only. Sent.
* `NBF` / `EXP` (Key Vault, below): the spec example and Learn example have strings, `KeyVault.tsp` has numbers and the
  Learn table says number. Strings are sent.
* Not given by any source, so chosen here: `id` is a random UUID (Azure's looks like the request id); `sequencer` format;
  the batch id.

## Destinations

How a message looks in each destination (details and sources in `docs/services/eventgrid.md`, "Destinations"):

| Destination | What arrives |
|---|---|
| Webhook | `POST` of a JSON array with the event (Event Grid schema) or the event itself (CloudEvents), `aeg-*` headers |
| Storage queue | the event JSON (one event), base64 encoded, as the message text; time to live 7 days by default, `queueMessageTimeToLiveInSeconds` (`-1`: never) |
| Service Bus queue / topic | body = event JSON, `content-type` `application/json`, `MessageId` = event id, `aeg-*` application properties |
| Event hub | body = event JSON, content type, `aeg-*` application properties; partition chosen round robin |

The queue, topic or hub has to exist. Delivery uses the retry and dead-letter rules of the subscription
(`retryPolicy`); a missing entity is retried and then dead-lettered (`GET …/deadletters`, `GET …/deliveries`).

The wiring is in `crates/azemu/src/main.rs`: `BlobService::set_event_sink`, `KeyVaultService::set_event_sink`,
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
  (copies here finish at once); events for blob snapshots, versions, Undelete, Append Block, Put Page, Set Blob Metadata /
  Properties / Tags, container operations, immutability and legal hold changes; one event per child of a recursively deleted
  or renamed directory (one `DirectoryDeleted` / `DirectoryRenamed` only); `directoryProperties`; SFTP events; lifecycle,
  inventory and storage task events; `BlobCreated` for `AppendBlock` and `FlushWithClose` without `close`.
  Delete of a snapshot or a version, and Delete Blob with `x-ms-delete-snapshots: only`, raise nothing (the docs list
  only `DeleteBlob` of the blob).
* Events of the `$logs` and `$blobchangefeed` containers are not raised (Learn).
* Key Vault: certificate events, near-expiry and expiry events, `VaultAccessPolicyChanged`. App Configuration: snapshot events,
  lock / unlock.
* Event Grid: delivery properties (`MessageId` from event data, `SessionId`, `PartitionKey`, static headers), delivery with
  managed identity, dead-letter to a blob container, batching, Azure Functions and Relay destinations.
* Standalone binaries: `azemu-blob` has the event hook but nothing listens; `azemu-eventgrid` refuses Storage queue, Service
  Bus and Event Hubs destinations with 501.
* Storage queue message encoding: no Microsoft page states that the message is base64 encoded or a single event rather than an
  array; this follows the CycleCloud guide (reads with `TextBase64DecodePolicy`) and the other destinations. Likewise the
  `application/json` content type of a Service Bus message is not stated on the handler page.
