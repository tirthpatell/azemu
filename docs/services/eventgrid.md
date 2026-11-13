# Event Grid (eventgrid)

**What it emulates.** Event Grid publishing and delivery, which Microsoft has no emulator for: classic publish to custom topics and domains
(`POST /api/events`), namespace topics with pull delivery (`:publish`, `:receive`, `:acknowledge`, `:release`, `:reject`, `:renewLock`), push delivery to
webhooks (the validation handshake, retries, dead-lettering) and to a Storage queue, a Service Bus queue or topic, or an event hub, filters, and the
events of other emulated services (Blob Storage, Data Lake, Key Vault, App Configuration: `docs/services/storage-events.md`).

**How to reach it.** `http(s)://{topic}.eventgrid.localhost:4577/api/events` (the topic is the first label of the host; a namespace host is the namespace).
With `--standard-ports`: `https://azemu:10012/api/events`, which serves one topic named by the host label (`default` for `localhost`). Any key, SAS
token or bearer token is accepted, but a request with none gets 401. Topics and subscriptions must be created first: through the admin API
(`/_azemu/eventgrid`), a JSON file (`AZEMU_EVENTGRID_CONFIG`) or ARM; with `--lenient` a topic named by a publish is made on first use.

**Not built.** Regions, keys and `provisioningState` of topics made outside ARM, the 64 KB event rule, range checks on lock
durations, a blob dead-letter destination, `api-version` validation. Retry waits are much shorter than Azure's schedule. Webhooks are limited to local targets
unless `--allow-webhook-host` says otherwise.

Crate `azemu-eventgrid` (lib + binary `azemu-eventgrid`). Mounts: `Mount::AccountHost("eventgrid")` and `Mount::PathPrefix("/_azemu/eventgrid")` (the admin API).

Microsoft has no Event Grid emulator. This one covers four things:

1. **Classic publish** to custom topics, domains and partner topics (`POST /api/events`).
2. **Namespace topics** with pull delivery (`:publish`, `:receive`, `:acknowledge`, `:release`,
   `:reject`, `:renewLock`).
3. **Push delivery** to webhooks (the emulator POSTs events to a URL you run), with the
   validation handshake, retries and dead-lettering, and to a Storage queue, a Service Bus queue or topic,
   or an event hub (unified `azemu` binary only).
4. **Events of other emulated services** (Blob Storage, Data Lake, Key Vault, App Configuration): subscriptions
   on an event *source* such as a storage account. See `docs/services/storage-events.md`.

Topics and subscriptions are created through ARM (Terraform `azurerm_eventgrid_topic`; `docs/services/arm.md`), through the admin API, or from a JSON
file. Nothing comes into being by being named, unless `--lenient` (or `AZEMU_EVENTGRID_AUTOCREATE=1`) is set.

## Pointing the SDKs at it

The host is `{name}.eventgrid.<anything>`; `{name}` is a custom topic, a domain or a namespace.
Only the first two labels matter, so a real-looking host also works.

```python
from azure.core.credentials import AzureKeyCredential
from azure.eventgrid import EventGridPublisherClient, EventGridConsumerClient, EventGridEvent

key = AzureKeyCredential("anything")           # any key, SAS token or bearer token is accepted

# custom topic or domain (Event Grid Basic): the endpoint includes /api/events
pub = EventGridPublisherClient("http://mytopic.eventgrid.localhost:4577/api/events", key)
pub.send(EventGridEvent(subject="s", event_type="t", data={"a": 1}, data_version="1"))

# namespace topic: the endpoint is the namespace host
ns = "http://myns.eventgrid.localhost:4577"
EventGridPublisherClient(ns, key, namespace_topic="jobs").send(CloudEvent(source="/s", type="t"))
c = EventGridConsumerClient(ns, key, namespace_topic="jobs", subscription="workers")
for d in c.receive(max_events=10, max_wait_time=5):
    c.acknowledge(lock_tokens=[d.broker_properties.lock_token])
```

Plain `http://` works. For `https://` trust the CA from `GET /_azemu/ca.pem` (or the cert dir). Note
that the Python Basic client (`EventGridPublisherClient` without `namespace_topic`) does not pass
`connection_verify` on: use `REQUESTS_CA_BUNDLE` for it. A token credential needs `https://`
(the SDK refuses to send a bearer token over plain HTTP).

## Operations

| Operation | Status |
|---|---|
| `POST /api/events` (Event Grid schema, CloudEvents batch and single, CloudEvents binary mode, custom schema) | built |
| Domains: routing by the event's `topic` (`source` for CloudEvents), domain topics created on first use | built |
| Partner topic publish (`aeg-channel-name` header) | accepted (no partner-specific behaviour) |
| `POST /topics/{topic}:publish` (batch, single, binary mode) | built |
| `POST /topics/{topic}/eventsubscriptions/{sub}:receive` (`maxEvents`, `maxWaitTime` long poll) | built |
| `:acknowledge`, `:release` (`releaseDelayInSeconds`), `:reject`, `:renewLock` | built |
| Push delivery to webhooks: handshake, `aeg-*` headers, retries, dead-letter, delivery log | built |
| `https://` webhooks | built (certificate is not verified) |
| Filters: event types, subject begins/ends with, case sensitivity, all advanced operators | built |
| Event Grid system events from other emulated services | built: subscriptions on a source (a storage account, a key vault, an App Configuration store), `EventGridService::raise_source_event`; see "Event sources" below. `publish_system_event` (events on a custom topic) remains |
| Anything else (other paths, other methods) | `501` with `{"error":{"code":"NotImplemented","message":"eventgrid: <METHOD> <path>"}}` |
| Delivery to a Storage queue, a Service Bus queue or topic, an event hub | built in the unified `azemu` binary (handlers registered by `crates/azemu`); the standalone `azemu-eventgrid` answers 501 |
| Delivery to Azure Functions, Relay hybrid connections, partner destinations; delivery properties (custom headers, `SessionId`, `PartitionKey`); managed identity delivery | not built |
| Dead-letter to a storage account, managed identity, delivery with resource identity | not built (dead letters are kept in memory and readable through the admin API) |
| Event batching on push delivery (`maxEventsPerBatch`), input mappings, MQTT broker | not built |

### Authentication

Presence only. A request needs one of `aeg-sas-key`, `aeg-sas-token` or `Authorization`
(`SharedAccessKey`, `SharedAccessSignature` or `Bearer`); its value is never checked. A request with none
gets `401` with the recorded error body (`Unauthorized`, with a `details` entry).

### Classic publish details

* Body limit: 1,536,000 bytes (as recorded: `413 RequestEntityTooLarge`, details `PayloadTooLarge`).
* Event Grid schema: `id`, `subject`, `eventType`, `eventTime` (must parse), `dataVersion` are required, `data`
  is optional. A missing field is `400 BadRequest` with a message in Event Grid's form
  ("This resource is configured to receive event in 'EventGridEvent' schema. ... Required property
  'subject' not found in JSON. Path '[0]'.") and `details[0].code = InputJsonInvalid`.
* CloudEvents: `specversion` must be `1.0`; `id`, `source`, `type` required; `data` and `data_base64` are exclusive;
  `time` must parse. `application/cloudevents-batch+json` needs an array, `application/cloudevents+json` an object.
* A topic created by publishing has input schema `Auto`: the schema is worked out from the content type and the first event
  (`specversion` means CloudEvents; `eventType`, `dataVersion` or `eventTime` mean Event Grid; anything else is custom).
  Create the topic through the admin API with `inputSchema` to fix it (`EventGridSchema`, `CloudEventSchemaV1_0`,
  `CustomEventSchema`); events of another shape are then rejected with `400`.
* Response: `200`, empty body, `api-supported-versions: 2018-01-01`, `Server: Microsoft-HTTPAPI/2.0`,
  `Strict-Transport-Security`, `x-ms-request-id`.

### Namespace topics (pull)

* `maxEvents` 1 to 100 (default 1). `maxWaitTime` is the long-poll time in seconds, default 60, at most 120. Azure documents
  a minimum of 10; the emulator accepts 0 and up so tests need not wait. The wait uses no thread and no lock; an event
  published, released or expiring wakes the waiter.
* A received event is locked for the subscription's lock duration (default 60 s) and gets a lock token (opaque, base64).
  `deliveryCount` starts at 1 and increases on every delivery.
* Lock lost (expiry or `release`): the event is offered again, or, when `deliveryCount` has reached `maxDeliveryCount`
  (default 10), dead-lettered (`MaxDeliveryCountExceeded`). `reject` dead-letters (`Rejected`). An event not received within
  its time to live (default one day) is dead-lettered (`TimeToLiveExpired`). Order is FIFO among available events.
* Results: `{"failedLockTokens":[{"lockToken","error":{"code","message"}}],"succeededLockTokens":[...]}`. Error codes:
  `BadToken` (never issued, as in the spec's example) and `TokenLost` (issued but expired or already used).
* `releaseDelayInSeconds`: Azure allows 0, 10, 60, 600, 3600. The emulator takes any number from 0 to 86400.
* A subscription only gets events published after it exists.

### Push delivery

* **Handshake** when a webhook subscription is created through the admin API (skipped for config-file subscriptions
  and with `"skipValidation": true`). Event Grid schema: POST of a `Microsoft.EventGrid.SubscriptionValidationEvent` with
  `aeg-event-type: SubscriptionValidation`; the webhook must answer `{"validationResponse": "<code>"}`. CloudEvents schema:
  `OPTIONS` with `WebHook-Request-Origin: eventgrid.azure.net`; the webhook must answer `WebHook-Allowed-Origin`
  (`eventgrid.azure.net` or `*`). The result is stored on the subscription as `validation.status`:
  `Succeeded`, `Failed` (with `detail`), `AwaitingManualAction` (the webhook answered 200 without the code: `GET` the
  `validationUrl`, which points back at the emulator, or call `POST …/validate`), `Skipped`, `NotRequired` (pull).
  A subscription whose status is `Failed` or `AwaitingManualAction` does not receive events.
* **Delivery**: `POST` with `aeg-event-type: Notification`, `aeg-subscription-name` (upper case, as Azure),
  `aeg-delivery-count` (0 for the first attempt), `aeg-data-version`, `aeg-metadata-version: 1`. Event Grid schema and custom
  schema: a JSON array with one event (`Content-Type: application/json; charset=utf-8`); CloudEvents: the event itself
  (`application/cloudevents+json; charset=utf-8`). `deliverySchema` converts between Event Grid and CloudEvents shapes; the
  service fills `topic` and `metadataVersion` in Event Grid events.
* **Retries**: any status outside 200 to 204, a timeout or a connection error is retried. 400, 413 and 414 are not (dead-lettered
  at once). Wait before retry n: `retryPolicy.retryIntervalsMs[n-1]` (the last value repeats) or, by default,
  `AZEMU_EVENTGRID_RETRY_BASE_MS` (500) doubling up to `AZEMU_EVENTGRID_RETRY_MAX_MS` (10000). After
  `retryPolicy.maxDeliveryAttempts` (30) attempts or `eventTimeToLiveInMinutes` (1440) the event is dead-lettered
  (`MaxDeliveryAttemptsExceeded`, `TimeToLiveExpired`, or `NonRetryableStatusCode`).
* Every event is delivered by its own tokio task; the state lock is never held during a call or a sleep. `reset`, deleting
  the subscription, or deleting the topic stops pending deliveries.
* **Limits on deliveries that are waiting (webhook down for hours).** Each pending delivery keeps its event in memory until it is
  delivered or dead-lettered, which for a webhook that is down takes up to 24 hours. At most 1000 deliveries and 32 MiB of events are
  pending per subscription, and 5000 deliveries and 128 MiB in all (settings below). When a new event passes a limit, the **oldest**
  pending deliveries are dead-lettered (reason `PendingQueueFull`, the log shows `DeadLettered`) and stop retrying, as Event Grid does
  when delivery fails for good; the newest events keep their retries. The dead-letter list itself keeps at most 10,000 events and 16 MiB per
  subscription (oldest dropped first). Statistics show `pendingPush`, `pendingPushBytes` and `shedPush`. A webhook's reply is read up to
  64 KiB and no further (the connection is closed).
* **Delivery log**: the last 200 attempts per subscription (time, event id, attempt, outcome, status code, detail, duration).
  `GET /_azemu/eventgrid/topics/{topic}/subscriptions/{sub}/deliveries`.
* This is the only place the emulator makes outbound calls, and only to the webhook URL you gave. `https://` webhooks work
  through rustls with the `ring` provider (no aws-lc, no openssl, no cmake); the certificate is **not** verified.
* **Webhooks go only where you said (`AZEMU_ALLOW_WEBHOOK_HOST`).** By default a webhook target must be a loopback or private
  address (127.0.0.0/8, ::1, 10/8, 172.16/12, 192.168/16, 100.64/10, IPv6 fc00::/7 and fe80::/10; not 169.254.0.0/16, where cloud
  metadata services answer), a name with no dot (a Docker Compose service name), `localhost` or a name under `.localhost`,
  `host.docker.internal`, `gateway.docker.internal`, or a name the host check serves (equal to or under a suffix of
  `AZEMU_ALLOW_HOST` or `AZEMU_TLS_NAME`; only the variables are looked at here, not the command-line options of the same names).
  Every other target is refused with a `400` when the subscription is made, by the admin API, by ARM (the subscription is put through
  the same code) and, for the config file, by skipping the entry with an error in the log. The message names the option. To open
  more, set `AZEMU_ALLOW_WEBHOOK_HOST` to a comma separated list of host names (a name also covers the names under it) and IP
  addresses, or `*` for every host; `--allow-webhook-host <name>` (repeatable) does the same on the command line. The rule is applied
  again when a delivery connects: the name is looked up (the lookup the connection needs anyway), addresses that are not private
  are dropped, and when nothing is left the attempt fails with the same message; so a name that changes what it points to
  cannot lead the emulator to a public address. A host you named is not checked at that step. Subscriptions loaded from an
  older state file are checked when they are called, not when they are loaded.

### Filters

`filter` on a subscription (the namespace name `filtersConfiguration`/`filters` is accepted too):

```json
{ "includedEventTypes": ["Order.Created"], "subjectBeginsWith": "/orders/", "subjectEndsWith": ".json",
  "isSubjectCaseSensitive": false,
  "advancedFilters": [ {"operatorType": "StringIn", "key": "data.color", "values": ["red", "blue"]},
                       {"operatorType": "NumberGreaterThan", "key": "data.qty", "value": 5} ] }
```

Operators: `NumberIn`, `NumberNotIn`, `NumberLessThan`, `NumberGreaterThan`, `NumberLessThanOrEquals`,
`NumberGreaterThanOrEquals`, `NumberInRange`, `NumberNotInRange`, `BoolEquals`, `StringIn`, `StringNotIn`,
`StringBeginsWith`, `StringNotBeginsWith`, `StringEndsWith`, `StringNotEndsWith`, `StringContains`, `StringNotContains`,
`IsNullOrUndefined`, `IsNotNull`. Keys: `id`, `subject`, `eventType`/`type`, `topic`, `source`, any top-level field,
`data.a.b`, `data.list[0]`. All conditions are ANDed; values of one operator are ORed; string comparison is
case-insensitive; at most 25 advanced filters and 25 values. A missing key makes the filter false, except
`IsNullOrUndefined` and the negative operators (`…NotIn`, `…NotBeginsWith`, …), which are true.

## Admin API

Under `/_azemu/eventgrid` on the gateway (any host) or a pinned listener. JSON in and out.

| Request | Result |
|---|---|
| `GET /` | settings and every topic |
| `GET /topics` | `{"value":[{name, kind, subscriptions, published, …}]}` (`kind`: `custom`, `domain`, `domainTopic`, `namespaceTopic`) |
| `PUT /topics/{name}` | create (201) or update (200) a **custom topic**. Body `{"kind":"custom"\|"domain"\|"namespace","inputSchema":"Auto"}` |
| `PUT /topics/{name}?namespace=N` | create the namespace topic `name` in namespace `N` (created if new). With body `kind: "namespace"` and no parameter, `N` is `default` |
| `PUT /topics/{name}?domain=D` | create the domain topic `name` in domain `D` (created if new) |
| `GET` / `DELETE /topics/{name}` (same parameters) | read / delete. Without a parameter, `name` is a custom topic or domain, else a topic of the namespace `default` |
| `GET /topics/{name}/subscriptions` | list |
| `PUT /topics/{name}/subscriptions/{sub}` | create (201) or replace the configuration (200; the queue is kept) |
| `GET` / `DELETE …/subscriptions/{sub}` | read, with `validation` and live `stats` / delete |
| `GET …/subscriptions/{sub}/deliveries` | push delivery log |
| `GET …/subscriptions/{sub}/deadletters` | dead-lettered events: `event`, `reason`, `description`, `deliveryCount`, `deadLetteredAt` |
| `GET …/subscriptions/{sub}/events` | events still in a pull queue: `event`, `deliveryCount`, `state` (`available`, `locked`, `delayed`) |
| `POST …/subscriptions/{sub}/validate` | run the webhook handshake again |
| `GET /validate/{code}` | manual validation (the `validationUrl` given to webhooks) |
| `GET /sources` | event sources that have subscriptions: `key`, `resourceId`, `subscriptions`, `published` |
| `PUT /sources/{kind}/{name}/subscriptions/{sub}` | create (201) or replace (200) a subscription on an event source. `{kind}`: `storage` (a storage account), `keyvault`, `appconfig` |
| `GET` / `DELETE /sources/{kind}/{name}/subscriptions/{sub}`, `GET …/subscriptions`, `GET …/{sub}/deliveries`, `GET …/{sub}/deadletters`, `POST …/{sub}/validate` | as for topic subscriptions |

Domain-wide subscriptions: `PUT /topics/{domain}/subscriptions/{sub}`. A domain topic's own: `…?domain=D`.
Names are case-insensitive. A subscription body:

```json
{ "deliveryMode": "pull" | "push",          // default push if webhookUrl is set, else pull (namespace topics only)
  "webhookUrl": "http://localhost:7071/hook",
  "deliverySchema": "EventGridSchema" | "CloudEventSchemaV1_0" | "CustomInputSchema",   // default: as published
  "filter": { ... },
  "lockDurationSeconds": 60, "maxDeliveryCount": 10, "eventTimeToLiveSeconds": 86400,   // pull
  "retryPolicy": { "maxDeliveryAttempts": 30, "eventTimeToLiveInMinutes": 1440, "retryIntervalsMs": [500, 1000] },   // push
  "destination": { "endpointType": "StorageQueue", "storageAccount": "acct", "queueName": "q" },   // instead of webhookUrl, see below
  "skipValidation": false }
```

Invalid bodies (pull on a custom topic, push without a URL, unknown filter operator, …) answer `400`
`{"error":{"code":"BadRequest","message":…}}`. Lock durations and times to live may be fractions of a second here (Azure: 60 to 300 s).

## Destinations: Storage queue, Service Bus, Event Hubs

A subscription can name a `destination` instead of a `webhookUrl`, ARM style or flat (names instead of resource ids):

| `endpointType` | ARM style `properties` | Flat |
|---|---|---|
| `StorageQueue` | `resourceId` (`…/storageAccounts/{acct}`), `queueName`, `queueMessageTimeToLiveInSeconds` | `storageAccount`, `queueName`, `queueMessageTimeToLiveInSeconds` |
| `ServiceBusQueue` | `resourceId` (`…/namespaces/{ns}/queues/{q}`) | `namespace`, `queueName` |
| `ServiceBusTopic` | `resourceId` (`…/namespaces/{ns}/topics/{t}`) | `namespace`, `topicName` |
| `EventHub` | `resourceId` (`…/namespaces/{ns}/eventhubs/{hub}`) | `namespace`, `eventHubName` |
| `WebHook` | `endpointUrl` | `endpointUrl` (same as `webhookUrl`) |

The account or namespace name is what the other emulated service sees: the storage account of the queue endpoint
(`{acct}.queue.core.localhost`), the namespace of the AMQP host (`{ns}.servicebus.localhost`, `{ns}.eventhub.localhost`).
The queue, topic or hub must exist, as on Azure (nothing is created); until it does, deliveries fail and are retried, and
after `maxDeliveryAttempts` they are dead-lettered like a webhook that answers 500 (`GET …/deadletters`; the delivery
log has the reason, for example `QueueNotFound: …`).

What is delivered (one event per message, not an array; schema by `deliverySchema`):

* **Storage queue**: the event JSON, **base64 encoded**, as the message text. Azure's pages do not say this; Microsoft's
  CycleCloud guide reads these messages with the SDK's `TextBase64DecodePolicy`. Message time to live: default 7 days,
  `queueMessageTimeToLiveInSeconds` sets it, `-1` never expires (Learn, "Set custom headers on delivered events":
  "If no time is provided, the message's default time to live is 7 days. You can also set the event to never expire.").
* **Service Bus queue or topic**: the event JSON as the body (one `data` section), `content-type` `application/json`
  (`application/cloudevents+json; charset=utf-8` for CloudEvents), `MessageId` = the event id (Learn: "The default value of
  `MessageId` is the internal ID of the Event Grid event"), application properties `aeg-subscription-name`,
  `aeg-delivery-count` (0 on the first attempt), `aeg-event-type` (`Notification`), `aeg-metadata-version`,
  `aeg-data-version`, `aeg-output-event-id` (Learn, "Service Bus as an event handler / Message headers").
  A topic delivery goes through the topic's rules like any message sent to it.
* **Event Hubs**: the same body, content type and `aeg-*` properties as application properties; the service picks the
  partition (round robin). Learn lists the same `aeg-*` headers for event hubs.

Not built: custom delivery properties (`MessageId` from event data, `SessionId`, `PartitionKey`, static headers): a
session-enabled queue therefore refuses the message and the delivery is dead-lettered. Learn does not say whether the body is
the bare event or an array; the emulator sends the bare event.

The destination handlers are registered by the program that mounts the service
(`EventGridService::register_destination(DestinationKind, handler)`); `crates/azemu` registers them for the queue,
servicebus and eventhubs services. The standalone `azemu-eventgrid` has none: such a subscription is refused with `501`.

## Event sources (system topics)

Other emulated services raise events into a *source*: "the events of storage account `acct`". A subscription on a source is
an ordinary subscription (filters, both schemas, webhook or destination, retries, dead letters, delivery log). On Azure the
source is a system topic (an ARM resource); the ARM control plane maps `Microsoft.EventGrid/systemTopics` and
`eventSubscriptions` onto the Rust API below.

```
PUT /_azemu/eventgrid/sources/storage/acct/subscriptions/orders
{ "webhookUrl": "http://localhost:7071/hook",
  "deliverySchema": "CloudEventSchemaV1_0",
  "filter": { "includedEventTypes": ["Microsoft.Storage.BlobCreated"], "subjectBeginsWith": "/blobServices/default/containers/in/" } }
```

* Source kinds: `storage` (events of a storage account: `docs/services/storage-events.md`), `keyvault` (a vault),
  `appconfig` (an App Configuration store).
* Default delivery schema is the Event Grid schema (the way these services publish); `topic` is the source's resource id:
  `/subscriptions/00000000-…/resourceGroups/azemu/providers/Microsoft.Storage/storageAccounts/{acct}` unless the subscription
  was made through the Rust API with a real resource id.
* Webhook handshake runs when the subscription is created through the admin API, as for topics.
* A subscription only gets events raised after it exists. With no subscription nothing is built or kept (the services ask
  first).
* Config file: `"storageAccounts": [{ "name": "acct", "resourceId": "…optional…", "subscriptions": [ … ] }]` next to `topics`,
  `domains`, `namespaces`; entries are subscription bodies as above (including `destination`). Handshakes are skipped, as for
  the other entries.

## Config file

`AZEMU_EVENTGRID_CONFIG=/path/eventgrid.json` is read at start and again after every reset
(`POST /_azemu/reset`), so a project can keep its wiring in git:

```json
{
  "topics": [
    { "name": "orders", "inputSchema": "EventGridSchema",
      "subscriptions": [
        { "name": "to-hook", "webhookUrl": "http://localhost:7071/api/hook",
          "filter": { "includedEventTypes": ["Order.Created"] } } ] } ],
  "domains": [
    { "name": "tenants", "inputSchema": "Auto",
      "subscriptions": [ { "name": "audit", "webhookUrl": "http://localhost:7071/audit" } ],
      "topics": [ { "name": "acme", "subscriptions": [ { "name": "s", "webhookUrl": "http://localhost:7071/acme" } ] } ] } ],
  "namespaces": [
    { "name": "default",
      "topics": [ { "name": "jobs", "subscriptions": [
        { "name": "workers", "lockDurationSeconds": 30, "maxDeliveryCount": 5 } ] } ] } ]
}
```

A subscription entry is the subscription body above plus `name`. Webhook handshakes are not run for file entries (the
endpoint is usually not up yet); they are marked `Skipped`. A subscription that is invalid is skipped with an error in the log; an
unreadable file is logged and ignored.

## Other settings (environment)

| Variable | Default | Meaning |
|---|---|---|
| `AZEMU_EVENTGRID_AUTOCREATE` | off (on with `AZEMU_LENIENT=1`) | `1`/`true`/`on`: a custom topic, a namespace and a namespace topic that were never created come into being when an event is published to them, and a pull subscription when it is first received from. Off (as on Azure) they are `404 NotFound` |
| `AZEMU_ALLOW_WEBHOOK_HOST` | none | hosts a webhook may point to besides local ones (see "Push delivery"); `*` allows every host. Option `--allow-webhook-host` |
| `AZEMU_EVENTGRID_CONFIG` | none | config file |
| `AZEMU_EVENTGRID_RETRY_BASE_MS` / `_RETRY_MAX_MS` | 500 / 10000 | default webhook retry backoff |
| `AZEMU_EVENTGRID_DELIVERY_TIMEOUT_MS` | 10000 | time a webhook has to answer |
| `AZEMU_EVENTGRID_MAX_PENDING` / `_MAX_PENDING_MB` | 1000 / 32 | deliveries waiting for a retry, per subscription: count and size of the events |
| `AZEMU_EVENTGRID_MAX_PENDING_TOTAL` / `_MAX_PENDING_TOTAL_MB` | 5000 / 128 | the same for all subscriptions together |

Defaults as on Azure: nothing is created by being named. Publishing to a topic, namespace or domain nobody created, or receiving from
a namespace topic or subscription nobody created, is `404 NotFound` (on Azure the host name would not resolve at all; there is no
closer answer to give). Create what a test needs first: with the admin API (`curl -X PUT http://localhost:4577/_azemu/eventgrid/topics/orders -d '{}'`,
then `.../topics/orders/subscriptions/to-hook` with `{"webhookUrl":"http://localhost:7071/hook"}`; add `?namespace=ns` for a
namespace topic and its subscription), with the config file above, or with ARM (Terraform `azurerm_eventgrid_topic`).
**A topic host is its own thing here:** the emulator has no notion of creating a namespace outside ARM and the config file, so
the name of a namespace is only known after it was created in one of those ways.
With auto-creation on (`AZEMU_EVENTGRID_AUTOCREATE=1` or `AZEMU_LENIENT=1`): publishing to an unknown host creates a custom topic (a
namespace topic with `:publish`); `:receive` on an unknown subscription of a namespace topic creates a pull subscription, which
then gets events published from that moment on. `publish_system_event` (the Rust entry point for other services) to a topic nobody created answers `accepted: false` unless auto-creation is on. Domain topics inside a domain you created are always created on
first publish, as on Azure.

## Reset

`POST /_azemu/reset?service=eventgrid&account=<host>` empties one topic host (a custom topic, domain, namespace or event source, by the name
in the host name) and puts back what the config file declares for it. Deliveries of that host that were waiting for a retry stop at their
next round, receivers waiting on its pull subscriptions return, and the other hosts are untouched.

## Known differences from Azure

* Credentials are only checked for presence. Nothing is signed or verified.
* Sources (storage account, key vault, store) have no system topic resource of their own here; the ARM layer adds that.
* Topics, subscriptions and namespaces made through the admin API or the config file are not ARM resources: no resource ids to list (events carry a made-up
  `/subscriptions/00000000-…/providers/Microsoft.EventGrid/…` topic id), no regions, no keys, no `provisioningState`. Topics made through ARM carry their real ARM id.
* The exact texts of validation errors are written in Event Grid's style from memory and the one recorded error; only the 401
  and 413 bodies are copied from recordings. Codes and statuses are what the recordings and the spec give.
* The 64 KB event size rule and the one-event-per-1 MB limit are not applied; only the request body limit is (1,536,000 bytes classic,
  1,048,576 bytes namespace publish).
* `maxWaitTime` below 10 seconds and release delays other than 0/10/60/600/3600 are accepted; lock duration, time to live and retry
  policy values are not range-checked beyond being positive.
* Event type filters are case-insensitive (assumed). Missing-key behaviour of negative advanced filters follows the documentation as read.
* Dead letters stay in memory on the subscription (no blob destination). Push delivery is one event per request (one message for a queue, topic or hub). Retry waits are much
  shorter than Azure's 10 s to 12 h schedule by default.
* No `api-version` validation: any version is accepted on both APIs.
* State lives in memory and is lost on restart unless `--data-dir` is set (the config file is reloaded either way).
* A POST to the root of a topic host (an endpoint given to the Python Basic client without `/api/events`) is `501`, where Azure
  answers an error of its own.

## Keeping data between runs

With `--data-dir` the service saves `eventgrid.state` (topics, domains, namespaces, event sources, subscriptions with configuration and validation
state) and one `sub-<hash>.state` per subscription (pull queue with events, delivery counts, locks and dead letters; the delivery log; push deliveries
that were not finished with the attempts made). Locks stay good after a restart until they end. Unfinished push deliveries start half a second after the
start. Details: `docs/services/persistence.md`, section "Cosmos, Event Grid, Service Bus, Event Hubs with Kafka, ARM".
