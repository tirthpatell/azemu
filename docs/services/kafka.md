# Kafka endpoint of Event Hubs

**What it emulates.** The Apache Kafka wire protocol that Event Hubs speaks, so that a Kafka application can use an event hub as a topic by changing its
configuration: producers (idempotent ones included), consumers and consumer groups, offsets, the admin calls clients need to start, SASL `PLAIN` and
`OAUTHBEARER` (every credential is accepted). It works across protocols: what a Kafka producer writes an AMQP (Event Hubs SDK) reader reads, and the
other way round. Checked with librdkafka (Python, `kcat`-style clients), the Java `kafka-clients` and franz-go.

**How to reach it.** The gateway port (4577) plain or TLS (the first bytes tell the protocols apart), or port 9093 with `--standard-ports` or
`--pin kafka=9093`. `bootstrap.servers={namespace}.eventhub.localhost:4577` (the namespace is the first label of the TLS server name), or `azemu:9093`
from a container. For TLS the client needs the emulator's certificate authority. The event hub (the topic) must exist first unless `--lenient`.

**Not built.** Transactions, compaction, `DeleteRecords`, ACLs, changing a topic's partition count, `SCRAM` and `GSSAPI`, fetch sessions, quotas and
throttling, Schema Registry, compression other than none and `gzip` (franz-go needs `ProducerBatchCompression(NoCompression())`). One broker (id 0) leads every
partition. See "Differences" for the rest.

## What it is

Event Hubs speaks the Apache Kafka wire protocol (port 9093 on Azure), so that a Kafka application can use an event hub as a
topic by changing its configuration. The emulator does the same on its own port, next to HTTP and AMQP:

| Kafka | Event Hubs here |
|---|---|
| topic | event hub |
| partition | partition of the event hub |
| offset | sequence number of the event in its partition (starts at 0, no gaps, never reused) |
| record key | partition key of the event (`x-opt-partition-key`) |
| record value | event body |
| record headers | application properties of the event |
| consumer group | Kafka group of the namespace: stored apart from the AMQP consumer groups, as on Azure (see below) |
| cluster / bootstrap server | namespace |

## How to connect

One port. The emulator looks at the first bytes of a connection: a Kafka request starts with a 4-byte length (under 16 MiB, so the
first byte is 0) and a known API key, which no HTTP, TLS, AMQP or WebSocket start does. It works on the gateway port (4577 by default)
plain and inside TLS, and on a pinned port: `azemu --pin kafka=9093` (or `azemu-eventhubs --pin kafka=9093`). Both `127.0.0.1` and `::1`
are bound unless `--host` says otherwise.

The broker address in `Metadata` and `FindCoordinator` (Kafka has no host name inside the protocol, so the emulator has to say where the
client should go next) is, in this order:

1. `AZEMU_KAFKA_ADVERTISED_HOST` (and `AZEMU_KAFKA_ADVERTISED_PORT`), when set: for Docker, port mappings, anything where the
   address the client used is not one the emulator can see;
2. the TLS server name the client sent (SNI), such as `myns.eventhub.localhost`: it is also what the client checks the certificate against;
3. the host of the `Endpoint=sb://...` in the SASL PLAIN password, when a client can plausibly reach it (an IP address, `localhost`, a
   name under `.localhost`, a name with no dot). A real Azure name such as `myns.servicebus.windows.net` is skipped, because it would send
   the client to Azure;
4. the local address the connection came in on (the IP the client dialled).

The port is the port the connection came in on (`AZEMU_KAFKA_ADVERTISED_PORT` overrides it). The broker is one node, id 0, and it is
also the coordinator of every group.

The **namespace** of a connection (which event hubs and groups it sees) is, in this order: the first label of the TLS server name
(`myns.servicebus.windows.net`, `myns.eventhub.localhost`; not `localhost`, an IP address, or the bare labels `eventhub`, `eventhubs`,
`servicebus`); else the first label of the host of the `Endpoint=sb://...` in the SASL PLAIN password; else `default`. Namespaces are
the same ones AMQP uses (`sb://myns.eventhub.localhost:{port}`), so what one protocol writes the other reads.

### Authentication

SASL `PLAIN` and `OAUTHBEARER` are offered (`SaslHandshake` v1, `SaslAuthenticate` v0 to 2) and every credential is accepted (the
project's loose-auth rule); the shape is checked (PLAIN needs the three NUL-separated parts, OAUTHBEARER needs `auth=Bearer`). A client
that skips SASL is accepted too. The PLAIN password is read only for its `Endpoint=`. `SCRAM` and `GSSAPI` are refused with
`UNSUPPORTED_SASL_MECHANISM`, and the list of mechanisms is in the answer.

### librdkafka (confluent-kafka for Python, kcat, node-rdkafka, the .NET and Go confluent clients)

Plain, no SASL (namespace `default`):

```properties
bootstrap.servers=127.0.0.1:4577
```

TLS and SASL PLAIN with a connection string (what the Event Hubs documentation shows), after `emu ca.pem` is saved:

```properties
bootstrap.servers=myns.eventhub.localhost:4577
security.protocol=SASL_SSL
ssl.ca.location=/path/to/ca.pem
sasl.mechanism=PLAIN
sasl.username=$ConnectionString
sasl.password=Endpoint=sb://myns.eventhub.localhost:4577/;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any
```

Without TLS: `security.protocol=SASL_PLAINTEXT`, drop `ssl.ca.location`; the namespace then comes from the `Endpoint=` in the
password. `security.protocol=ssl` with no SASL works too (the namespace is the server name). Notes from the tests:
- the default `compression.type` of librdkafka is none; `gzip` works; `snappy`, `lz4` and `zstd` get `UNSUPPORTED_FOR_MESSAGE_FORMAT` (as on Event Hubs);
- librdkafka's consumer asks for metadata of a subscribed topic with auto-creation off whatever `allow.auto.create.topics` says, so a consumer
  does not create a missing hub; a producer does only when the emulator's auto-creation is on (`AZEMU_EVENTHUBS_AUTOCREATE=1` or `AZEMU_LENIENT=1`; off by default, as on Event Hubs, where a missing hub is `UNKNOWN_TOPIC_OR_PARTITION`). Create the hub first (admin API, config file, ARM: `docs/services/eventhubs.md`) or with Kafka's own admin client (`CreateTopics`);
- `session.timeout.ms` must be at least 6000 (Kafka's broker minimum), and Microsoft recommends 30000.

### Java client (`org.apache.kafka:kafka-clients`)

```properties
bootstrap.servers=myns.eventhub.localhost:4577
security.protocol=SASL_SSL
sasl.mechanism=PLAIN
sasl.jaas.config=org.apache.kafka.common.security.plain.PlainLoginModule required username="$ConnectionString" password="Endpoint=sb://myns.eventhub.localhost:4577/;SharedAccessKeyName=k;SharedAccessKey=any";
ssl.truststore.location=/path/to/truststore.p12   # the JDK's cacerts plus ca.pem (keytool -importcert ...)
ssl.truststore.password=changeit
```

The Java producer is idempotent by default (`enable.idempotence=true`, `acks=all`): supported. Compression `gzip` or none. The Java consumer
and producer ask for auto-creation of a missing topic, so a missing hub is created only when the emulator's auto-creation is on (it is off by default). `transactional.id` fails
(transactions are not built; `initTransactions` times out). OAUTHBEARER: `sasl.mechanism=OAUTHBEARER` with any login module and token.

### A pure-language client (franz-go, Go)

```go
kgo.NewClient(
    kgo.SeedBrokers("myns.eventhub.localhost:4577"),
    kgo.DialTLSConfig(&tls.Config{RootCAs: pool}),                      // pool holds ca.pem
    kgo.SASL(plain.Auth{User: "$ConnectionString", Pass: connString}.AsMechanism()),
    kgo.ProducerBatchCompression(kgo.NoCompression()),                  // franz-go's default is snappy; Event Hubs reads gzip only
)
```

Tested with franz-go (`tests/sdk-go/kafka`) and librdkafka. Other clients that should work for the same reasons (not tested here): kafkajs, Sarama, segmentio
kafka-go, Spring Kafka, Kafka Streams without transactions, Kafka Connect without exactly-once.

### Settings

| Variable | Meaning |
|---|---|
| `AZEMU_KAFKA_ADVERTISED_HOST` | host put in `Metadata` and `FindCoordinator`, for connections where nothing tells it (plain TCP without a useful `Endpoint=`) |
| `AZEMU_KAFKA_ADVERTISED_PORT` | port put there, when it is not the one the emulator listens on |
| `AZEMU_KAFKA_INITIAL_REBALANCE_DELAY_MS` | how long the first JoinGroup of an empty group waits for others (Kafka: 3000, here 500) |
| `AZEMU_EVENTHUBS_AUTOCREATE` | off by default (as on Event Hubs): a missing topic is `UNKNOWN_TOPIC_OR_PARTITION`; `1` (or `AZEMU_LENIENT=1`) makes `Metadata` create it when the client allows it, with 4 partitions and one day retention, like an AMQP client does |
| `POST /_azemu/reset` | drops hubs, events, groups and committed offsets; waiting fetches end |

`GET /_azemu/kafka/{namespace}/groups` lists the groups (state, members, committed offsets), for looking at what a test left behind.

## APIs

Served (the `ApiVersions` list; the maximum of each is the last version before the one that needs topic ids or another feature the emulator
does not have, the minimum is what the current Java client still sends):

| API (key) | Versions | Notes |
|---|---|---|
| Produce (0) | 3 to 9 | acks 0 gets no answer; idempotent producers; gzip and no compression; transactional batches refused (`INVALID_TXN_STATE`) |
| Fetch (1) | 4 to 12 | long poll (`max_wait_ms` up to 30 s, `min_bytes`), `max_bytes` and partition limits (at least one event is always returned); no fetch sessions (session id 0) |
| ListOffsets (2) | 1 to 6 | earliest (-2), latest (-1), by time |
| Metadata (3) | 1 to 9 | auto-creation as above |
| OffsetCommit (8) | 2 to 8 | metadata at most 1024 bytes |
| OffsetFetch (9) | 1 to 7 | all partitions of the group with a null topic list |
| FindCoordinator (10) | 0 to 3 | group coordinators only (key type 0) |
| JoinGroup (11) | 1 to 7 | classic protocol; v4+ asks a new member to come back with its id (`MEMBER_ID_REQUIRED`) |
| Heartbeat (12) | 0 to 4 | |
| LeaveGroup (13) | 0 to 4 | |
| SyncGroup (14) | 0 to 5 | |
| DescribeGroups (15) | 0 to 5 | |
| ListGroups (16) | 0 to 4 | |
| SaslHandshake (17) | 1 | |
| ApiVersions (18) | 0 to 4 | a version above 4 is answered as v0 with `UNSUPPORTED_VERSION` and the list, as the protocol asks |
| CreateTopics (19) | 0 to 5 | `retention.ms`; `cleanup.policy=compact` is `INVALID_CONFIG` |
| DeleteTopics (20) | 0 to 5 | |
| InitProducerId (22) | 0 to 4 | ids are handed out for idempotent producers; a `transactional.id` is refused (`UNSUPPORTED_VERSION`); epoch bumps (KIP-360) work |
| DescribeConfigs (32) | 0 to 4 | topics (retention, cleanup policy, max message size, ...) and the broker |
| SaslAuthenticate (36) | 0 to 2 | |
| CreatePartitions (37) | 0 to 3 | always refused: the partition count of an event hub cannot change (`INVALID_PARTITIONS`) |

Not offered in `ApiVersions`, so clients do not call them (a request for one closes the connection): transactions (AddPartitionsToTxn,
AddOffsetsToTxn, EndTxn, WriteTxnMarkers, TxnOffsetCommit), DeleteRecords, DeleteGroups, ACLs, AlterConfigs and IncrementalAlterConfigs,
OffsetForLeaderEpoch, ElectLeaders, partition reassignment, log dirs, quotas, SCRAM credentials, DescribeCluster (the Java admin client
falls back to Metadata), the KIP-848 group protocol (ConsumerGroupHeartbeat), Fetch/Produce versions with topic ids. Event Hubs documents
transactions only for Premium and Dedicated (preview) and compaction for the same tiers; they are not built here. Compression other
than gzip is refused as Event Hubs refuses it ("The message format version on the broker does not support the request").

## Mapping onto Event Hubs

Follows Microsoft Learn, "Exchange events between consumers and producers that use different protocols":

- **Kafka to AMQP**: the value is one `data` section (a null value is an event with no body section); the key becomes the
  `x-opt-partition-key` annotation, a string when it is UTF-8, else binary; each header is an application property whose value is an AMQP
  binary (a null header value is AMQP null), so an AMQP reader gets raw bytes, as the article says. The event's offset is its sequence number.
  An AMQP reader finds it with `event.partition_key`, `event.body`, `event.properties`.
- **AMQP to Kafka**: the body (all `data` sections joined, an `amqp-value` string or binary as its bytes, other values AMQP-encoded) is the
  value; the partition key is the key; each application property is a header whose value is the **AMQP-encoded** bytes, type byte
  included (a string `hello` is `A1 05 68 65 6c 6c 6f`; an int 1234 is `71 00 00 04 d2`), except a binary property, which comes out as its
  raw bytes (so what a Kafka producer wrote is what a Kafka consumer reads back). The properties of the AMQP properties section
  `message-id`, `user-id`, `to`, `reply-to`, `content-type`, `content-encoding`, `creation-time` come out as UTF-8 string headers of
  those names (`creation-time` as an ISO 8601 UTC time with 7 fractional digits: a guess, the article does not say).
- Record timestamps are the time the event was enqueued (the time the emulator stored it), not the producer's `CreateTime`; the producer's
  time is not kept. `ListOffsets` by time uses it.
- A Kafka partition is a partition of the hub. Producers choose partitions themselves (the client's partitioner) and the record is stored in the partition of the
  produce request; its key is kept as the partition key and is not mapped to a partition again. An AMQP event sent without
  a partition goes to a partition by the partition-key hash of the Event Hubs SDKs (`docs/services/eventhubs.md`, "Known differences") or round robin.
- Retention (by time, by hub size), the 1 MiB event limit (a batch or a record over it is `MESSAGE_TOO_LARGE`), auto-creation and reset are
  those of the event hub, for both protocols. A reader whose offset was trimmed gets `OFFSET_OUT_OF_RANGE` and the client's
  `auto.offset.reset` decides; sequence numbers go on counting after events expire.
- **Idempotent producers**: `InitProducerId` hands out a producer id and epoch; every partition remembers, per producer id, the next expected
  sequence number and the last 5 batches (base sequence, count, offset). A retry of a stored batch is answered with the original offset and
  stored once; a gap is `OUT_OF_ORDER_SEQUENCE_NUMBER`; an older epoch is `INVALID_PRODUCER_EPOCH`; an old sequence that is no longer
  remembered is `DUPLICATE_SEQUENCE_NUMBER`. A producer id a partition has not seen starts at whatever sequence it sends (lenient, so a
  producer that carries on after an emulator reset does not fail). At most 10,000 producers per partition are remembered.
- **Consumer groups**: the classic protocol, per namespace. The leader's assignment passes through untouched (any assignor works: range,
  round robin, sticky, cooperative-sticky). `JoinGroup` completes when every known member has joined, or at the rebalance timeout (the
  longest `max.poll.interval.ms` / rebalance timeout of the members) when it does not; the first round of a new group waits the initial delay.
  Members that stop heartbeating are dropped after their session timeout (6 s to 30 min), checked when the group is touched and every second
  while a join waits. A member leaving, joining or expiring starts a rebalance: the others learn of it from `Heartbeat`
  (`REBALANCE_IN_PROGRESS`). Generation ids, `UNKNOWN_MEMBER_ID`, `ILLEGAL_GENERATION` follow Kafka. Committed offsets live in the group and
  survive the members; a commit without a generation (admin tools, `assign()` consumers) is accepted while the group has no members.
  Group ids are at most 256 characters, offset metadata at most 1024 bytes (Event Hubs' limits).

## Bounds

- Frames: a request is at most 16 MiB; a larger length closes the connection. The body is read as it arrives (a length never sizes a
  buffer), within 60 s of the length. An idle connection (no request) is closed after 10 minutes. At most 2,000 connections; more are
  closed at once.
- Decoding (`crates/kafka`): a count or length read from the wire is checked against the bytes left before anything is allocated; a decoded
  request holds at most one value per byte of the request (an array of one-byte elements can cost up to about 32 bytes of memory per byte);
  nesting is bounded by the message layouts; a record batch checks its length, its CRC-32C, its record count (at least 7 bytes a record),
  every record length and header count. Anything
  malformed closes the connection (a corrupt record batch is `CORRUPT_MESSAGE` for that partition). Bytes after the end of a message are
  ignored, as Apache Kafka does (librdkafka writes some for an all-topics Metadata request).
- Requests on a connection are answered one at a time, in order (as Kafka does, a connection is muted while a request is answered), so at
  most one request per connection is in flight and a pipelining client is held back by the socket.
- Fetch: `max_bytes` is capped at 16 MiB, at most 20,000 events per partition per answer; the wait is at most 30 s and needs no lock: the
  fetch waits on a notification of the store and looks again, and a reset ends it. Produce: a record over 1 MiB or a batch
  whose compressed bytes are over 1 MiB is `MESSAGE_TOO_LARGE`. A request has one budget shared by all its batches: a batch may hold 4 MiB of
  records once inflated and 50,000 records, and the whole request 32 MiB where every record counts its bytes plus 576 bytes (the memory of
  the event). The record count is checked before any record is made, and a gzip batch stops inflating as soon as its output passes the limit
  (never inflate first and check after). A batch over its own limit is `MESSAGE_TOO_LARGE`; partitions that do not fit into the request's budget
  are `RECORD_LIST_TOO_LARGE`. A stored event is charged the same overhead against the size limit of its hub as events from AMQP.
  Requests: at most 1,000 topics (groups, config resources) and 20,000 (topic, partition) pairs in one request, else the
  connection is closed (as Apache Kafka does for a request it cannot handle); a Metadata answer lists at most 20,000 partitions (the topics
  after that are `LEADER_NOT_AVAILABLE`) and answers a topic named twice once. Groups: at most 200 members per group, 10,000 groups per namespace, 100,000 committed partitions per group, 1,000 handed-out
  member ids waiting to be used. Nothing waits while holding the Event Hubs state lock.

## Differences from Azure and from Apache Kafka

- One broker, id 0, leader of every partition; replicas and in-sync replicas are `[0]`; no rack, no leader epochs beyond 0.
- No fetch sessions (every fetch is a full fetch), no incremental fetch, no throttling (`throttle_time_ms` is 0), no quotas.
- No authorization and no checks of credentials; no `SCRAM`; SASL sessions do not expire.
- Record timestamps are enqueue times (above); there is no log-append-time versus create-time setting.
- Static group membership (`group.instance.id`) is stored and handed back but does not fence a restarted member or survive a rebalance.
- Compaction, transactions, DeleteRecords, ACLs and changing the partition count are not built. A hub created by a Kafka client has 4
  partitions (Azure requires the hub to exist), like one created by an AMQP client.
- Offsets are sequence numbers, so an AMQP `x-opt-offset` and a Kafka offset are different numbers for the same event.
- Event Hubs throttles offset commits (2 per second per partition); the emulator does not.
- Azure: Kafka needs Standard tier or above and compression needs Premium; neither is checked.
- Not checked against real Azure (no account here): the exact error codes for compaction (`INVALID_CONFIG`) and transactions
  (`UNSUPPORTED_VERSION` for `InitProducerId` with a transactional id), the form of `creation-time`, and whether Azure answers metadata for
  an unknown topic with auto-creation. They follow Kafka's own conventions.

## Keeping data between runs

Topics, records, committed offsets and idempotent producer state live in the Event Hubs state and are saved with it (`docs/services/eventhubs.md`, "Keeping
data between runs"). After a restart a consumer group is empty and its consumers join again; committed offsets stay; a producer that reconnects with
its producer id gets the answers Kafka specifies for a retry, a next batch and a gap.
