# Event Hubs (`azemu-eventhubs`)

**What it emulates.** The Event Hubs AMQP 1.0 data plane (send, batches, partition keys, receive from any position, consumer groups, owner levels, the
blob checkpoint store that SDK processors use, `$cbs` and `$management` reads), AMQP over WebSocket, a small JSON admin API, and the Apache Kafka
endpoint (`docs/services/kafka.md`): a Kafka topic is an event hub, offsets are sequence numbers, and events cross between AMQP and Kafka both ways.

**How to reach it.** `Endpoint=sb://{namespace}.eventhub.localhost:4577;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any;UseDevelopmentEmulator=true;EntityPath={hub}`
(the label `eventhub` in the host is what makes it Event Hubs and not Service Bus; both share the AMQP port). With `--standard-ports`: port 5672 (5671 for
TLS), `sb://eventhub:5672` from a container that has the network alias `eventhub`. Kafka: port 9093 or the gateway port. An event hub must be created
first (admin API `PUT /_azemu/eventhubs/{ns}/hubs/{hub}`, a config file, ARM, or `--lenient`); a namespace needs no creation.

**Schema Registry.** The REST API of the namespace's schema registry (`/$schemaGroups` on the namespace host: register, read by id and version, `get-id`, versions, `DELETE`, Avro compatibility
checks in Forward and Backward mode, Avro, Json, custom and Protobuf schemas) is built: [`schema-registry.md`](schema-registry.md). Groups are made as on Azure, outside that API: the admin API
(`PUT /_azemu/eventhubs/{ns}/schemagroups/{group}`), the config file (`SchemaGroups`), Resource Manager (`Microsoft.EventHub/namespaces/schemagroups`) or `--lenient`.

**Not built.** Capture, geo-disaster recovery, clusters, the ATOM/XML HTTP management API (501), AMQP transactions (the attach is refused,
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
`put-token` (answered 202; 404 for a missing event hub unless auto-creation is on), then its links. Addresses arrive as full URLs
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
  hub nobody created fails with `amqp:not-found` and Event Hubs' text
  ("The messaging entity 'sb://ns.servicebus.windows.net/hub' could not be found. To know more visit
  https://aka.ms/sbResourceMgrExceptions. TrackingId:..."); management reads answer status 404; a Kafka client gets
  `UNKNOWN_TOPIC_OR_PARTITION` (`docs/services/kafka.md`; but see the Kafka endpoint below: it makes the hub for a producer and an admin client, as Azure does). **A consumer group nobody created is not an error:** a receiver on it gets the events and
  the group is not added to the hub's list (a real Standard namespace did so; checked in November 2026, not on other tiers). Create the hub first, as a team does against Azure: the admin API
  below (`curl -X PUT http://localhost:4577/_azemu/eventhubs/myns/hubs/orders -H 'Content-Type: application/json' -d '{"partitionCount":4,"consumerGroups":["workers"]}'`),
  the config file below, or ARM (Terraform `azurerm_eventhub`). **Namespaces are different:** the emulator has no way to create a
  namespace outside ARM, so any namespace name works; the first hub or the config file is what makes the name an Event Hubs
  namespace for the shared AMQP port (see the section above).
- **Auto-creation** (`AZEMU_EVENTHUBS_AUTOCREATE=1`, or `AZEMU_LENIENT=1` / `--lenient`; the variable of the service switches this
  one on or off whatever `AZEMU_LENIENT` says): a sender, a receiver or a management read on a missing hub creates
  it with 4 partitions and one day retention. It is a convenience Azure does not have. (The Kafka endpoint makes the hub for a
  producer's or an admin client's `Metadata` request that allows it (`allow.auto.create.topics`), as Azure does, with 1 partition and
  7 days, whatever this setting says.)
- **Config file** (`AZEMU_EVENTHUBS_CONFIG=/path/Config.json`), loaded at start and after every reset, in Microsoft's
  Event Hubs emulator format: `UserConfig.NamespaceConfig[]` (`Name`, optional `Type: "EventHub"`) with `Entities[]`
  (`Name`, `PartitionCount` as number or string, `ConsumerGroups[].Name`). Ours on top: `MessageRetentionInDays`,
  `MessageRetentionInSeconds`, and `SchemaGroups[]` in a namespace (`Name`, `SchemaType`, `SchemaCompatibility`: [`schema-registry.md`](schema-registry.md)). When the file names exactly one namespace, `default` is an alias of it (`sb://localhost` reaches it, as
  with Microsoft's emulator, which ignores the host). If a Service Bus config also names one namespace, the alias is
  claimed by both and `localhost` goes to Event Hubs.
- **Admin API** `/_azemu/eventhubs` (JSON; from a browser a body with any `Content-Type` but `application/json` is a `415`):

| Request | Result |
|---|---|
| `GET /_azemu/eventhubs/namespaces` | namespaces with hub counts |
| `GET /_azemu/eventhubs/{ns}/hubs` | all hubs, with partition counters |
| `PUT /_azemu/eventhubs/{ns}/hubs/{hub}` | create (201) or update retention / add consumer groups (200). Body `{"partitionCount":4,"retentionSeconds":86400,"retentionDays":1,"consumerGroups":["a"]}`, all optional. A different partition count on an existing hub is 400 (Azure Standard cannot change it). |
| `GET`, `DELETE .../hubs/{hub}` | read (200) / delete (204; receivers are detached with `amqp:not-found`) |
| `GET .../hubs/{hub}/partitions[/{n}]` | `beginSequenceNumber`, `lastEnqueuedSequenceNumber`, `lastEnqueuedOffset`, `lastEnqueuedTimeUtc`, `isEmpty`, `eventCount`, `sizeBytes`, `totalEventsReceived` |
| `GET .../hubs/{hub}/consumergroups`, `PUT/GET/DELETE .../consumergroups/{cg}` | consumer groups (`$Default` cannot be deleted) |
| `GET .../schemagroups`, `PUT/GET/DELETE .../schemagroups/{group}` | Schema Registry groups (body `{"schemaType":"Avro","schemaCompatibility":"Forward","groupProperties":{}}`; [`schema-registry.md`](schema-registry.md)) |

  Anything else under the prefix is 501 naming the request. The real ATOM/XML management API
  (`https://ns.eventhub.localhost:{port}/{hub}?api-version=...`) is **not built**: 501 `NotImplemented`.
- `POST /_azemu/reset` drops all hubs, events and consumer groups, detaches every receiver (`amqp:not-found`), then reloads the config file.

## Data plane

| Area | Behaviour |
|---|---|
| Send | sender link to `{hub}` (partition by the `x-opt-partition-key` annotation, hashed as Azure does it, see "Known differences"; else round robin per event) and `{hub}/Partitions/{n}` (key ignored); single events and the batch format `0x80013700` (each data section one event; a batch without keys stays in one partition, keys split it per event; the batch is stored as a whole or rejected as a whole). Every delivery is settled `accepted`. Every body form is accepted and comes back as it was sent: one or several `data` sections, an `amqp-value` section (null included), `amqp-sequence` sections, no body section, and the JavaScript SDK's null body (a `data` section whose content is AMQP null, which is not valid AMQP; it is kept as sent). |
| Limits | link `max-message-size` 1 048 576 (the SDK reads it for `create_batch`); a larger message is rejected with `amqp:link:message-size-exceeded` (library behaviour: the delivery is rejected with the error, the link stays). |
| Producer properties | `com.microsoft:idempotent-producer`, `producer-group-id`, `producer-owner-level`, `com.microsoft:georeplication`, `com.microsoft:timeout` are accepted and ignored: the Python SDK has no idempotent publishing, so there is no sequence bookkeeping. |
| Stored event | all sections as sent (types kept: string, long, uuid, timestamp, data / value / sequence bodies, application properties, user annotations and delivery annotations); the broker-set annotations are removed on the way in, so a client cannot spoof them. |
| Delivered event | pre-settled; message annotations `x-opt-sequence-number-epoch` (int, always -1), `x-opt-sequence-number` (long, per partition from 0, never reused), `x-opt-offset` (string; a running byte count, first event `0`, see "Known differences"), `x-opt-enqueued-time` (timestamp, never goes backwards in a partition), `x-opt-partition-key` when set; an event a Kafka producer wrote also has `x-opt-kafka-key` (its key). No header section. |
| Receive | receiver link from `{hub}/ConsumerGroups/{cg}/Partitions/{n}`; link credit and drain are honoured; reading removes nothing; every link has its own position; a receiver at the end gets new events as they arrive (live tailing). |
| Start positions | selector filter `apache.org:selector-filter:string`: `amqp.annotation.x-opt-offset > '-1'` (start), `> '@latest'` (new events only, fixed at attach), `x-opt-offset >/>= '<offset>'`, `x-opt-sequence-number >/>= '<n>'`, `x-opt-enqueued-time >/>= '<ms>'`; quotes optional. A sequence number or an offset **past the last one** is refused, inclusive or not (`com.microsoft:argument-error`, "The supplied sequence number '3' is invalid. The last sequence number in the system is '2' TrackingId:..."; for an offset "The supplied offset '145' is invalid. The last offset in the system is '144' ..."); the last one itself waits for the next event. An empty partition has last sequence number -1 and last offset `"-1"`, so `0` is refused there. A time is never refused (a time in the future gives nothing, one long ago everything). No filter: from the start of the retained stream (checked against Azure in November 2026). A filter that cannot be read (an offset that is not a number, a name that is not one of the three): `amqp:not-allowed`, "The filter '...' is not a valid amqp filter for EventHub entities". |
| Epochs | link property `com.microsoft:epoch`. Per hub, partition and consumer group: a receiver with an epoch takes over every receiver with a lower or **equal** epoch and every one without (they are detached with `amqp:link:stolen`, "New receiver 'nil' with higher epoch of '2' is created hence current receiver 'nil' with epoch '1' is getting disconnected..."); a receiver with a lower epoch, or none, cannot attach next to an epoch receiver (`amqp:link:stolen`, "Receiver 'nil' with a higher epoch '2' already exists..." and "At least one receiver for the endpoint is created with epoch of '2', and so non-epoch receiver is not allowed..."). At most 5 receivers without an epoch per partition and consumer group (`amqp:resource-limit-exceeded`, "Exceeded the maximum number of allowed receivers per partition in a consumer group which is 5. List of connected receivers - nil, nil, nil, nil, nil."). The texts are those of a real namespace (November 2026); it names a receiver `nil` when the client gave no name, and so does the emulator for every receiver. |
| Runtime metrics | desired capability `com.microsoft:enable-receiver-runtime-metric` (echoed in the attach reply): every delivery carries delivery annotations `last_enqueued_sequence_number`, `last_enqueued_offset`, `last_enqueued_time_utc`, `runtime_info_retrieval_time_utc` (the state of the partition at delivery time) after any delivery annotations the sender set, and `last_enqueued_sequence_number_epoch` (int, -1). |
| Retention | time based, default one day (Azure's default), lazily applied by every append, read, management read and admin read against the clock; there are no timers. Sequence numbers keep counting after events expire; a receiver whose position was trimmed continues at the oldest retained event. |
| Errors on attach | An attach that cannot be served (a missing hub or partition, a start position that cannot exist, a sixth receiver without an owner level, an epoch conflict, a bad address) is refused the way AMQP 1.0 section 2.6.3 says and `Microsoft.Azure.Amqp` (the library under the real service) does it: an attach answer with a **null source and null target**, followed at once by a `detach` that carries the error (`amqp:not-found`, `com.microsoft:argument-out-of-range`, `amqp:resource-limit-exceeded`, `amqp:link:stolen`, ...). The same form is used by Service Bus; it lives in `azemu-amqp` (`AttachDecision::Refuse`). A complete attach followed by a detach would not do: the .NET SDK treats the complete attach as an open link and opens it again in a loop, with no exception. |

### `$management` and `$cbs`

`$cbs`: `put-token` answers `status-code` 202 for SAS and JWT (the credential is not checked, see "Known differences"); other operations 501.
Unless auto-creation is on, a token for an event hub that does not exist is answered 404 with
Azure's "The messaging entity ... could not be found" text, as Azure does; the SDKs report it as `AuthenticationError`
before they open a link. A token for a consumer group that was never created is accepted.
`$management` (the bare node the SDKs attach to, and `{hub}/$management`), `operation` `READ` (spelled so):
`type` `com.microsoft:eventhub` -> `name`, `created_at` (timestamp), `partition_count`, `partition_ids` (array of strings), `georeplication_factor` (1), `type`;
`type` `com.microsoft:partition` -> `name`, `partition`, `begin_sequence_number`, `begin_sequence_number_epoch` (-1), `last_enqueued_sequence_number`,
`last_enqueued_sequence_number_epoch` (-1), `last_enqueued_offset`, `last_enqueued_time_utc` (0001-01-01 for a partition that never had an event),
`is_partition_empty`, `type`. An empty partition reports -1 / -1 / `"-1"`. A type it does not know is answered 200 with no body. Any other operation
(`DELETE`, `CREATE`, `read`, an empty one) is refused when the request is received, `amqp:not-allowed`, "Invalid command 'DELETE'.".
An answer carries the application properties `operation` (`READ`) and `status-code` (200), and the request's message id as correlation id; an error answer carries
`status-code`, `status-description` and `error-condition`. A response goes to the response link whose target address is the request's `reply-to`, else the one on the request's session. Bad
partition: 400 `com.microsoft:argument-out-of-range`, "The specified partition is invalid for an EventHub partition sender or receiver. It should be between 0 and 3." (a sender or receiver
link gets a second line, `Parameter name: PartitionId`); missing hub (unless auto-creation is on): 404, with `SystemTracker:{namespace}.servicebus.windows.net:{hub}`. Checkpoint stores are client side (Blob Storage) and need nothing here.

## Known differences from Azure and from Microsoft's emulator

The Python suite (`tests/sdk-python/eventhubs`) was run against a real Standard namespace with one throughput unit in November 2026, and the emulator
was changed to answer as it did. What follows is what is still different, and what was checked. Other tiers and regions were not tried; where a difference
is a matter of tier, it says so.

- **Credentials are not checked.** Azure answers a bad key, an unknown key name, an expired SAS and a SAS for another event hub with 401 `com.microsoft:auth-failed`
  at `$cbs` ("InvalidSignature: The token has an invalid signature.", "ExpiredToken: The token is expired. ...", "InvalidAudience: The authorization header contains a
  token with a wrong audience."; a SAS for the whole namespace is accepted). The emulator knows no keys and accepts every credential, on purpose, unless `--strict-auth` is on: then the keys, the rights, the expiry and the roles of
  Entra tokens are checked as Azure answered in November 2026 ([`../strict-auth.md`](../strict-auth.md)): a bad credential at `$cbs`, a missing `Send` or `Listen` claim at the link
  attach (the partition of a consumer group in lower case), a principal with no role with a 401 inside the answer to the `$management` read. Rules come from Resource Manager (keys
  from `listKeys`), the config file (`AuthorizationRules`), or `/_azemu/eventhubs/{ns}/authorization-rules`; any other namespace has `RootManageSharedAccessKey` with the key `SAS_KEY_VALUE`.
- **Partition keys** go to the partition the Event Hubs service chooses, which is **not** the one Microsoft's SDKs use for their buffered producers. Worked out from
  where about 600 keys went on event hubs with 2 to 32 partitions (the rule fits every sample): the key is cut to ASCII (a character that is not ASCII, per UTF-16 unit,
  becomes `?`) and put in upper case (`a` and `A` go together), hashed with Bob Jenkins' lookup3 (`hashlittle2`, both seeds 0), the two result words XOR-ed make a 32-bit number, and its remainder modulo 32767
  is the position of the key. The 32767 positions are divided between the partitions in order, `32767 / n` each, with the first `32767 % n` partitions one position more
  (`model::partition_for_key`; tests with the 475 samples: `crates/services/eventhubs/tests/partition_key.rs`). It is used for every AMQP send and batch to the hub without a partition id.
  An empty key is no key (round robin). A Kafka producer picks the partition itself (its partitioner) and the endpoint stores the record where the producer said; the record key is
  kept as the partition key and is not mapped again, as on Event Hubs. Events in a state saved by an older version stay where they are; only new events use the rule. Event Grid
  deliveries to an event hub carry no partition key and go round robin.
- **Offsets** are running byte counts like Azure's, and the first event has `"0"`. The step from one event to the next fits what Azure gave for 45 events of 0 to 1,008 bytes (with and
  without a partition key, properties and a content type): the size of the encoded message, plus 56, plus 3 once the message is 256 bytes or longer, rounded up to a multiple of 8
  (a few bytes of body: 64 per event). The rule was fitted to those samples, not read from a specification; events that came in through Kafka use it with the size of their message
  as the emulator keeps it.
- **A start position that is a time** is compared in milliseconds. Azure compares finer than the millisecond it reports, so with the enqueued time of an event given inclusive the event came
  back 2 times in 10; the emulator always returns it. A program cannot depend on either.
- **Limits of the Standard tier are not copied** where Microsoft documents more for Premium or Dedicated: 10 event hubs per namespace, 20 consumer groups per event hub (excluding `$Default`; the 21st is 409
  `MessagingGatewayForbidden`), 32 partitions per event hub, 7 days retention. The emulator allows 1,000 event hubs, 100 consumer groups (through the admin API and ARM, not by attaching), 1,024 partitions, any retention.
- **Connection settings** in the `open` frame are Event Hubs' own (read from a real namespace): `max-frame-size` 65536, `idle-time-out` 120000 ms, so a client sends an empty frame every minute. Service Bus
  connections on the same port keep the server's settings (1 MiB frames, 240 s).
- **Transports.** Azure serves AMQP in TLS on port 5671 and AMQP over WebSocket on 443. On port 5672 it answers a header without SASL with `AMQP\x02\x01\x00\x00` (TLS required) and offers the mechanisms `MSSBCBS`, `PLAIN`, `ANONYMOUS`,
  `EXTERNAL` after a SASL header (the emulator offers the same four). It does not answer on `ws://` port 80; the emulator serves plain `ws://` (`docs/services/websocket.md`).
- AMQP sessions whose `incoming-window` is 2^31 or more (the Rust SDK uses 2^32-1) work; windows, credits and ids are unsigned and wrap (`crates/amqp`).
- No rights, no per-namespace throughput limits (throttling), no Capture (Kafka: `docs/services/kafka.md`; Schema Registry: `docs/services/schema-registry.md`, which lists its own differences). Idempotent and partitioned AMQP producer bookkeeping is accepted and ignored.
- A hub made by auto-creation (opt-in) has 4 partitions (Azure requires the hub to exist). Microsoft's emulator has one fixed namespace and
  needs a config file; here namespaces appear when the first hub is made.
- Partition count cannot change after creation; no partition scaling, no compaction.
- Message size limit is 1 MiB for every hub (Basic tier would be 256 KiB). An event with a body of 1,048,500 bytes is accepted, 1,048,576 is not (the message is 8 bytes longer than the body); the text is Azure's.
- Receivers are not limited per connection; the 5-receiver quota is the only one enforced.
- Retention is by time and by size: a hub keeps at most 512 MiB of events (split over its partitions, at least 16 MiB each) and
  drops the oldest first. Azure keeps 84 GB per throughput unit and refuses sends when storage is full. Every stored event counts its
  encoded size plus `EVENT_OVERHEAD` (576 bytes: what the event takes in memory besides its data), so a million empty events fill a
  partition the way they fill memory. The same charge applies to events from Kafka and from AMQP.
- One AMQP batch message holds at most 50,000 events (`MAX_EVENTS_PER_BATCH`; a batch of empty events is 5 bytes each, so 1 MiB would
  otherwise be 200,000); a batch over it is rejected with `amqp:link:message-size-exceeded`. The 1 MiB link limit is the library's.
- Limits of the emulator: 1,000 event hubs per namespace, 100 consumer groups per hub, 5,000 connections per server, 1,000
  sessions and 10,000 links per connection (library limits); `$cbs` / `$management` requests up to 1 MiB.

### Checked against Azure in November 2026

Behaviour that was a guess in earlier versions and was compared with a real Standard namespace: a receiver link without a filter reads from the start; an empty partition reports
`begin_sequence_number` -1, `last_enqueued_sequence_number` -1, offset `"-1"` and 0001-01-01; `georeplication_factor` is 1; the epoch fields of a partition are -1; a lower owner level is refused, an equal one takes over, a
receiver without a level is refused next to one with a level; five receivers without a level, the sixth is refused; the 1 MiB link limit and its text; `$Default` and the case of group names;
max 32 partitions and 1 partition allowed on Standard. Not tried: Premium and Dedicated, other regions, expiry of events (retention is at least a day), throttling, Entra tokens, the HTTP APIs.
- **Faults.** Only on request ([`faults.md`](../faults.md)). A throttled send, connection or receiving link is refused with `com.microsoft:server-busy`. The Kafka endpoint takes `latency`, `drop` and `timeout` rules only.

## Reset

`POST /_azemu/reset` empties everything (hubs, events, Kafka groups and offsets, schema groups and schemas) and loads the config file again.
`POST /_azemu/reset?service=eventhubs&account=<namespace>` empties that namespace only: its hubs and events go, receivers waiting there are
woken so each learns that its hub is gone, its Kafka groups and committed offsets end, the config file's hubs for it come back, and other
namespaces are untouched. `Shared::reset_namespace(name)` is the same in Rust. The `kafka` service answers the same request (it has no
state of its own).

## Keeping data between runs

With `--data-dir` the service saves `eventhubs.state` (namespaces, hubs, consumer groups, Kafka committed offsets, next Kafka producer id) and one
`part-<hash>.state` per partition (events with sequence numbers, offsets, enqueue times and the message as sent, counters, Kafka producer sequence state).
The Kafka endpoint (`kafka` service) shares these files and has none of its own. Receivers, epochs and Kafka group members are not stored. Retention is
by absolute time. Details: `docs/services/persistence.md`, section "Cosmos, Event Grid, Service Bus, Event Hubs with Kafka, ARM".
