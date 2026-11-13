# Event Hubs (`azemu-eventhubs`)

**What it emulates.** The Event Hubs AMQP 1.0 data plane (send, batches, partition keys, receive from any position, consumer groups, owner levels, the
blob checkpoint store that SDK processors use, `$cbs` and `$management` reads), AMQP over WebSocket, a small JSON admin API, and the Apache Kafka
endpoint (`docs/services/kafka.md`): a Kafka topic is an event hub, offsets are sequence numbers, and events cross between AMQP and Kafka both ways.

**How to reach it.** `Endpoint=sb://{namespace}.eventhub.localhost:4577;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any;UseDevelopmentEmulator=true;EntityPath={hub}`
(the label `eventhub` in the host is what makes it Event Hubs and not Service Bus; both share the AMQP port). With `--standard-ports`: port 5672 (5671 for
TLS), `sb://eventhub:5672` from a container that has the network alias `eventhub`. Kafka: port 9093 or the gateway port. An event hub must be created
first (admin API `PUT /_azemu/eventhubs/{ns}/hubs/{hub}`, a config file, ARM, or `--lenient`); a namespace needs no creation.

**Not built.** Schema Registry, Capture, geo-disaster recovery, clusters, the ATOM/XML HTTP management API (501), AMQP transactions (the attach is refused,
as Azure does), idempotent and partitioned producer bookkeeping (accepted, ignored), authentication, rights and throughput limits. Retention is limited
to 512 MiB per hub.

Implemented with the AMQP library `azemu-amqp` that Service Bus uses; checked with the Python, JavaScript, .NET, Java, Go and Rust Event Hubs SDKs.

Code: `crates/services/eventhubs/src/` (`amqp_conn.rs` links, `mgmt.rs` `$cbs` + `$management`, `state.rs`
namespaces / receiver registry / epochs, `model.rs` hubs, partitions, events, `selector.rs` start positions,
`address.rs`, `config.rs`, `admin.rs`, `mux.rs` the shared-port dispatcher, `errors.rs`, `kafka/` the Kafka endpoint on top of the
same state: `crates/kafka` is the protocol library).

## How clients connect

| Variant | Works | How |
|---|---|---|
| Plain AMQP, `UseDevelopmentEmulator=true` | yes | `Endpoint=sb://myns.eventhub.localhost:{port};SharedAccessKeyName=k;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true;EntityPath=myhub`. The SDK turns TLS off and keeps the port of `Endpoint=` (5.15.1). `sb://localhost:{port}` and `sb://127.0.0.1:{port}` work too (namespace `default`). |
| AMQP inside TLS on the same port | yes | `EventHubProducerClient("myns.eventhub.localhost", hub, cred, custom_endpoint_address="sb://myns.eventhub.localhost:{port}", transport_type=TransportType.Amqp, connection_verify="~/.azemu/ca.pem")`. Without `transport_type=Amqp` a custom endpoint means WebSocket, which is not available. |
| SASL | yes | `ANONYMOUS`, `PLAIN`, `EXTERNAL`, `MSSBCBS` offered; every credential accepted; a client that skips SASL works too. |
| AMQP over WebSocket | yes | `transport_type=TransportType.AmqpOverWebsocket` with `custom_endpoint_address="http://myns.eventhub.localhost:{port}/"` (`https://` for TLS, plus `connection_verify`). Over WebSocket the SDK ignores the port in `Endpoint=`, so the custom endpoint is required. See `docs/services/websocket.md`. |

The SDK dials `host:port` for TCP, sends the host (without port) as the `hostname` of the AMQP `open`, then `$cbs`
`put-token` (answered 202; 404 for a missing event hub or consumer group unless auto-creation is on), then its links. Addresses arrive as full URLs
(`amqps://host:port/hub/ConsumerGroups/$Default/Partitions/0`); scheme and authority are dropped.
Each SDK operation uses its own TCP connection.

## One port for Service Bus and Event Hubs: which namespace is which

Both services speak AMQP and, on real Azure, both live under `*.servicebus.windows.net`; a namespace is
one kind or the other. The `azemu` binary registers ONE raw protocol, the mux (`AmqpMux`), which owns each AMQP
connection through the handshake and chooses at the `open` frame (the host name the client dialled):

1. **An event hub exists in the namespace** (created through the admin API, the config file, or by an earlier
   client; or the config file declares the namespace): Event Hubs. The namespace is the first label of the host.
2. **The host has the label `eventhub` or `eventhubs`**: `myns.eventhub.localhost`, `eventhub.localhost`,
   `myns.eventhubs.core.windows.net`: Event Hubs. This needs no setup.
3. Anything else (`myns.servicebus.localhost`, `localhost`, an IP): Service Bus.

Fallback at link attach: on a connection rule 3 sent to Service Bus, a link to an address only Event Hubs has
(`{hub}/Partitions/{n}`, `{hub}/ConsumerGroups/{cg}/Partitions/{n}`, the bare `$management` node) is served by Event
Hubs, and the event hub it creates makes the namespace an Event Hubs namespace from then on. A plain `{hub}` sender
address and `$cbs` stay with Service Bus on such a connection (both accept every token).

So: **write `sb://{ns}.eventhub.localhost:{port}` for Event Hubs** and `sb://{ns}.servicebus.localhost:{port}` (or
`sb://localhost:{port}`) for Service Bus. HTTP is routed by host label too: `https://{ns}.eventhub.localhost:{port}/...`
(mounts `eventhub` and `eventhubs`), `/_azemu/eventhubs/...` for the admin API.

The standalone `azemu-eventhubs` binary treats every AMQP connection as Event Hubs, whatever the host. The namespace
is the first label of the host except for `localhost`, IP addresses, an empty host and the bare labels `eventhub`,
`eventhubs`, `servicebus`, which are the namespace `default`.

How the two are registered (`crates/azemu/src/lib.rs`, `full_stack()`):

```rust
let (sb_service, _sb_amqp) = azemu_servicebus::pair();   // the Service Bus raw protocol is not registered any more
let (eh_service, eh_amqp) = azemu_eventhubs::pair();
let sb_factory = Arc::new(azemu_servicebus::amqp_conn::SbFactory { shared: sb_service.shared.clone() });
let mux = eh_amqp.mux(sb_factory);                         // Arc<AmqpMux>
// ... services gets sb_service and eh_service; raw protocols: vec![mux]
azemu_core::run_cli(services, vec![mux]).await
```
The mux is generic over any `Arc<dyn HandlerFactory>`; its AMQP server settings are Event Hubs' (1 MiB frames, container id `azemu-eventhubs-...`).

## The Kafka endpoint next to AMQP

`azemu_eventhubs::kafka::KafkaEndpoint` (`EventHubsAmqp::kafka()`) is a second raw protocol and a service named `kafka` (so `--pin kafka=9093`
works), registered next to the AMQP mux in `crates/azemu/src/lib.rs` and in the `azemu-eventhubs` binary. Core hands a connection to the
first raw protocol whose `sniff` matches: AMQP says yes to `AMQP`, Kafka to a 4-byte length under 16 MiB followed by a known API key and a
plausible version; neither matches HTTP, TLS (inside TLS the decrypted bytes are sniffed the same way) or `PRI `. Kafka groups and
committed offsets live in `Shared::kafka` (cleared by `reset`); events are the same events: a Kafka offset is the sequence number, so
retention, the size limit, auto-creation, hub deletion and reset apply to both protocols. `State::notify` wakes Kafka fetches that wait for
data. Everything about it is in `docs/services/kafka.md`.

## Entities

Event hubs with N partitions (`"0"`..`"N-1"`), consumer groups (`$Default` always exists), retention. Created by:

- **Defaults as on Azure.** Nothing comes into being by being named. A sender, a receiver or a management read on an event
  hub nobody created, or a receiver on a consumer group nobody created, fails with `amqp:not-found` and Event Hubs' text
  ("The messaging entity 'sb://ns.servicebus.windows.net/hub' could not be found. To know more visit
  https://aka.ms/sbResourceMgrExceptions. TrackingId:..."); management reads answer status 404; a Kafka client gets
  `UNKNOWN_TOPIC_OR_PARTITION` (`docs/services/kafka.md`). Create the hub first, as a team does against Azure: the admin API
  below (`curl -X PUT http://localhost:4577/_azemu/eventhubs/myns/hubs/orders -d '{"partitionCount":4,"consumerGroups":["workers"]}'`),
  the config file below, or ARM (Terraform `azurerm_eventhub`). **Namespaces are different:** the emulator has no way to create a
  namespace outside ARM, so any namespace name works; the first hub or the config file is what makes the name an Event Hubs
  namespace for the shared AMQP port (see the section above).
- **Auto-creation** (`AZEMU_EVENTHUBS_AUTOCREATE=1`, or `AZEMU_LENIENT=1` / `--lenient`; the variable of the service switches this
  one on or off whatever `AZEMU_LENIENT` says): a sender, a receiver or a management read on a missing hub creates
  it with 4 partitions and one day retention; a receiver on a missing consumer group creates the group. The Kafka endpoint does the
  same for a producer's or an admin client's `Metadata` request that allows it (`allow.auto.create.topics`). It is a convenience
  Azure does not have.
- **Config file** (`AZEMU_EVENTHUBS_CONFIG=/path/Config.json`), loaded at start and after every reset, in Microsoft's
  Event Hubs emulator format: `UserConfig.NamespaceConfig[]` (`Name`, optional `Type: "EventHub"`) with `Entities[]`
  (`Name`, `PartitionCount` as number or string, `ConsumerGroups[].Name`). Ours on top: `MessageRetentionInDays`,
  `MessageRetentionInSeconds`. When the file names exactly one namespace, `default` is an alias of it (`sb://localhost` reaches it, as
  with Microsoft's emulator, which ignores the host). If a Service Bus config also names one namespace, the alias is
  claimed by both and `localhost` goes to Event Hubs.
- **Admin API** `/_azemu/eventhubs` (JSON):

| Request | Result |
|---|---|
| `GET /_azemu/eventhubs/namespaces` | namespaces with hub counts |
| `GET /_azemu/eventhubs/{ns}/hubs` | all hubs, with partition counters |
| `PUT /_azemu/eventhubs/{ns}/hubs/{hub}` | create (201) or update retention / add consumer groups (200). Body `{"partitionCount":4,"retentionSeconds":86400,"retentionDays":1,"consumerGroups":["a"]}`, all optional. A different partition count on an existing hub is 400 (Azure Standard cannot change it). |
| `GET`, `DELETE .../hubs/{hub}` | read (200) / delete (204; receivers are detached with `amqp:not-found`) |
| `GET .../hubs/{hub}/partitions[/{n}]` | `beginSequenceNumber`, `lastEnqueuedSequenceNumber`, `lastEnqueuedOffset`, `lastEnqueuedTimeUtc`, `isEmpty`, `eventCount`, `sizeBytes`, `totalEventsReceived` |
| `GET .../hubs/{hub}/consumergroups`, `PUT/GET/DELETE .../consumergroups/{cg}` | consumer groups (`$Default` cannot be deleted) |

  Anything else under the prefix is 501 naming the request. The real ATOM/XML management API
  (`https://ns.eventhub.localhost:{port}/{hub}?api-version=...`) is **not built**: 501 `NotImplemented`.
- `POST /_azemu/reset` drops all hubs, events and consumer groups, detaches every receiver (`amqp:not-found`), then reloads the config file.

## Data plane

| Area | Behaviour |
|---|---|
| Send | sender link to `{hub}` (partition by the `x-opt-partition-key` annotation with a stable FNV-1a hash, else round robin per event) and `{hub}/Partitions/{n}` (key ignored); single events and the batch format `0x80013700` (each data section one event; a batch without keys stays in one partition, keys split it per event; the batch is stored as a whole or rejected as a whole). Every delivery is settled `accepted`. Every body form is accepted and comes back as it was sent: one or several `data` sections, an `amqp-value` section (null included), `amqp-sequence` sections, no body section, and the JavaScript SDK's null body (a `data` section whose content is AMQP null, which is not valid AMQP; it is kept as sent). |
| Limits | link `max-message-size` 1 048 576 (the SDK reads it for `create_batch`); a larger message is rejected with `amqp:link:message-size-exceeded` (library behaviour: the delivery is rejected with the error, the link stays). |
| Producer properties | `com.microsoft:idempotent-producer`, `producer-group-id`, `producer-owner-level`, `com.microsoft:georeplication`, `com.microsoft:timeout` are accepted and ignored: the Python SDK has no idempotent publishing, so there is no sequence bookkeeping. |
| Stored event | all sections as sent (types kept: string, long, uuid, timestamp, data / value / sequence bodies, application properties, user annotations and delivery annotations); the broker-set annotations are removed on the way in, so a client cannot spoof them. |
| Delivered event | pre-settled; message annotations `x-opt-sequence-number` (long, per partition from 0, never reused), `x-opt-offset` (string; increases by size + 32 per event, first event `0`), `x-opt-enqueued-time` (timestamp, never goes backwards in a partition), `x-opt-partition-key` when set. |
| Receive | receiver link from `{hub}/ConsumerGroups/{cg}/Partitions/{n}`; link credit and drain are honoured; reading removes nothing; every link has its own position; a receiver at the end gets new events as they arrive (live tailing). |
| Start positions | selector filter `apache.org:selector-filter:string`: `amqp.annotation.x-opt-offset > '-1'` (start), `> '@latest'` (new events only, fixed at attach), `x-opt-offset >/>= '<offset>'`, `x-opt-sequence-number >/>= '<n>'` (beyond the end of the partition: `com.microsoft:argument-out-of-range`), `x-opt-enqueued-time >/>= '<ms>'`; quotes optional. No filter: from the start of the retained stream (assumption, Azure's default is not documented). A bad filter: `com.microsoft:argument-error`. |
| Epochs | link property `com.microsoft:epoch`. Per hub, partition and consumer group: a receiver with an epoch takes over every receiver with a lower or **equal** epoch and every one without (they are detached with `amqp:link:stolen`, "New receiver with higher epoch of 'N' is created hence current receiver ... is getting disconnected"); a receiver with a lower epoch, or none, cannot attach next to an epoch receiver (`amqp:link:stolen`). At most 5 receivers without an epoch per partition and consumer group (`amqp:resource-limit-exceeded`, Event Hubs' quota). |
| Runtime metrics | desired capability `com.microsoft:enable-receiver-runtime-metric` (echoed in the attach reply): every delivery carries delivery annotations `last_enqueued_sequence_number`, `last_enqueued_offset`, `last_enqueued_time_utc`, `runtime_info_retrieval_time_utc` (the state of the partition at delivery time) after any delivery annotations the sender set. |
| Retention | time based, default one day (Azure's default), lazily applied by every append, read, management read and admin read against the clock; there are no timers. Sequence numbers keep counting after events expire; a receiver whose position was trimmed continues at the oldest retained event. |
| Errors on attach | An attach that cannot be served (a missing hub, partition or consumer group, a start position that cannot exist, a sixth receiver without an owner level, an epoch conflict, a bad address) is refused the way AMQP 1.0 section 2.6.3 says and `Microsoft.Azure.Amqp` (the library under the real service) does it: an attach answer with a **null source and null target**, followed at once by a `detach` that carries the error (`amqp:not-found`, `com.microsoft:argument-out-of-range`, `amqp:resource-limit-exceeded`, `amqp:link:stolen`, ...). The same form is used by Service Bus; it lives in `azemu-amqp` (`AttachDecision::Refuse`). A complete attach followed by a detach would not do: the .NET SDK treats the complete attach as an open link and opens it again in a loop, with no exception. |

### `$management` and `$cbs`

`$cbs`: `put-token` answers `status-code` 202 for SAS and JWT (the credential is not checked); other operations 501.
Unless auto-creation is on, a token for an event hub or consumer group that does not exist is answered 404 with
Azure's "The messaging entity ... could not be found" text, as Azure does; the SDKs report it as `AuthenticationError`
before they open a link.
`$management` (the bare node the SDKs attach to, and `{hub}/$management`), `operation` `READ`:
`type` `com.microsoft:eventhub` -> `name`, `created_at` (timestamp), `partition_count`, `partition_ids` (array of strings);
`type` `com.microsoft:partition` -> `name`, `partition`, `begin_sequence_number`, `last_enqueued_sequence_number`,
`last_enqueued_offset`, `last_enqueued_time_utc` (0001-01-01 for a partition that never had an event), `is_partition_empty`.
An empty partition reports -1 / -1 / `"-1"`. Responses carry `status-code`, `status-description`, `error-condition`
and the request's message id as correlation id; a response goes to the response link whose target address is the request's `reply-to`, else the one on the request's session. Bad
partition: 400 `com.microsoft:argument-out-of-range`; missing hub (unless auto-creation is on): 404; other operations and
types: 501 naming them. Checkpoint stores are client side (Blob Storage) and need nothing here.

## Known differences from Azure and from Microsoft's emulator

- `$management` READ replies carry the keys the SDKs read: hub `name`, `created_at`, `partition_ids`, `partition_count`, `georeplication_factor` (always 1), `type`;
  partition `name`, `partition`, `begin_sequence_number`, `last_enqueued_sequence_number`, `last_enqueued_offset`, `last_enqueued_time_utc`,
  `is_partition_empty`, `type`, `last_enqueued_sequence_number_epoch` (always 0; the value is a guess, the Rust SDK only checks the key).
- AMQP sessions whose `incoming-window` is 2^31 or more (the Rust SDK uses 2^32-1) work; windows, credits and ids are unsigned and wrap (`crates/amqp`).

- No authentication, no rights, no SAS expiry checks; no per-namespace throughput limits; no Capture, Schema Registry (Kafka: `docs/services/kafka.md`).
- Offsets are small numbers (byte-like, increasing) and sequence numbers have no gaps; real offsets are larger.
- **Partition keys** go to the partition the Event Hubs service chooses. The hash is the one Microsoft's SDKs carry for their
  buffered producers: Bob Jenkins' lookup3 over the UTF-8 bytes of the key, the two result words combined with XOR and cut to a
  signed 16-bit number, then the absolute value of that modulo the number of partitions. It is a port of
  `partitionKeyToIdMapper.js` of `@azure/event-hubs` (the .NET SDK's `PartitionResolver` is the same function) and is tested against
  20 keys and 7 partition counts whose partitions were worked out by running that JavaScript function
  (`crates/services/eventhubs/tests/partition_key.rs`, values made again by `partition_key_values.mjs`). It is used for every AMQP
  send and batch to the hub without a partition id. A Kafka producer picks the partition itself (its partitioner) and the endpoint
  stores the record where the producer said; the record key is kept as the partition key and is not mapped again, as on Event Hubs.
  Events in a state saved before this change stay in the partitions they are in; only new events use the new hash. Event Grid
  deliveries to an event hub carry no partition key and go round robin.
- A hub made by auto-creation (opt-in) has 4 partitions (Azure requires the hub to exist). Microsoft's emulator has one fixed namespace and
  needs a config file; here namespaces appear when the first hub is made.
- Partition count cannot change after creation; no partition scaling, no compaction.
- Receiver without a filter reads from the start of the retained stream.
- Message size limit is 1 MiB for every hub (Basic tier would be 256 KiB).
- The empty-partition `begin_sequence_number` is -1 (assumed from the .NET and Python SDK docs).
- Receivers are not limited per connection; the 5-receiver quota is the only one enforced.
- Idle timeout 240 s and the other connection settings are those of the Service Bus server (shared library).
- Retention is by time and by size: a hub keeps at most 512 MiB of events (split over its partitions, at least 16 MiB each) and
  drops the oldest first. Azure keeps 84 GB per throughput unit and refuses sends when storage is full. Every stored event counts its
  encoded size plus `EVENT_OVERHEAD` (576 bytes: what the event takes in memory besides its data), so a million empty events fill a
  partition the way they fill memory. The same charge applies to events from Kafka and from AMQP.
- One AMQP batch message holds at most 50,000 events (`MAX_EVENTS_PER_BATCH`; a batch of empty events is 5 bytes each, so 1 MiB would
  otherwise be 200,000); a batch over it is rejected with `amqp:link:message-size-exceeded`. The 1 MiB link limit is the library's.
- Limits of the emulator: 1,000 event hubs per namespace, 100 consumer groups per hub, 5,000 connections per server, 1,000
  sessions and 10,000 links per connection (library limits); `$cbs` / `$management` requests up to 1 MiB.

## Reset

`POST /_azemu/reset` empties everything (hubs, events, Kafka groups and offsets) and loads the config file again.
`POST /_azemu/reset?service=eventhubs&account=<namespace>` empties that namespace only: its hubs and events go, receivers waiting there are
woken so each learns that its hub is gone, its Kafka groups and committed offsets end, the config file's hubs for it come back, and other
namespaces are untouched. `Shared::reset_namespace(name)` is the same in Rust. The `kafka` service answers the same request (it has no
state of its own).

## Keeping data between runs

With `--data-dir` the service saves `eventhubs.state` (namespaces, hubs, consumer groups, Kafka committed offsets, next Kafka producer id) and one
`part-<hash>.state` per partition (events with sequence numbers, offsets, enqueue times and the message as sent, counters, Kafka producer sequence state).
The Kafka endpoint (`kafka` service) shares these files and has none of its own. Receivers, epochs and Kafka group members are not stored. Retention is
by absolute time. Details: `docs/services/persistence.md`, section "Cosmos, Event Grid, Service Bus, Event Hubs with Kafka, ARM".
