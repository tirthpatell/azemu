# Kafka endpoint of Event Hubs

**What it emulates.** The Apache Kafka wire protocol that Event Hubs speaks, so that a Kafka application can use an event hub as a topic by changing its
configuration: producers (idempotent ones included), consumers and consumer groups, offsets, the admin calls clients need to start, SASL `PLAIN` and
`OAUTHBEARER` (every credential is accepted). It works across protocols: what a Kafka producer writes an AMQP (Event Hubs SDK) reader reads, and the
other way round. Checked with librdkafka (Python, `kcat`-style clients), the Java `kafka-clients` and franz-go.

**How to reach it.** The gateway port (4577) plain or TLS (the first bytes tell the protocols apart), or port 9093 with `--standard-ports` or
`--pin kafka=9093`. `bootstrap.servers={namespace}.eventhub.localhost:4577` (the namespace is the first label of the TLS server name), or `azemu:9093`
from a container. For TLS the client needs the emulator's certificate authority. A producer or an admin client that names a topic that does not exist makes the event hub (1 partition, 7 days), as on Azure; a consumer's request does not.

**Not built.** Transactions, compaction, `DeleteRecords`, `DeleteGroups`, ACLs, `AlterConfigs`, changing a topic's partition count, `SCRAM` and `GSSAPI`, fetch sessions, quotas and
throttling, compression other than none and `gzip` (franz-go needs `ProducerBatchCompression(NoCompression())`). One broker (id 0) leads every
partition. See "Differences" for the rest. The Schema Registry of the namespace is served over HTTP, not by this endpoint, and it is Azure's registry, not Confluent's ([`schema-registry.md`](schema-registry.md), "Kafka").

**Checked against Azure.** The Python suite (`tests/sdk-python/kafka`, librdkafka) was run against the Kafka endpoint of a real Standard namespace in November 2026, and the emulator was changed to answer as it
did. "Differences" lists what is still different. Only librdkafka and hand-made `ApiVersions` requests were used; the Java client and franz-go were not run against Azure, and Premium and Dedicated were not tried.

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
project's loose-auth rule; with `--strict-auth` they are checked, see below); the shape is checked (PLAIN needs the three NUL-separated parts, OAUTHBEARER needs `auth=Bearer`). A client
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
- the default `compression.type` of librdkafka is none; `gzip` works (Azure Standard refuses it, see "Differences"); `snappy`, `lz4` and `zstd` get `UNSUPPORTED_FOR_MESSAGE_FORMAT` (as on Event Hubs);
- librdkafka's consumer asks for metadata of a subscribed topic with auto-creation off whatever `allow.auto.create.topics` says, so a consumer
  does not create a missing hub; a producer and the admin client's `list_topics` do (the request allows it): the event hub comes into being with 1 partition and 7 days retention and the record is delivered, as on Azure. `AZEMU_EVENTHUBS_AUTOCREATE` does not switch this; it is about AMQP. Create the hub first (admin API, config file, ARM: `docs/services/eventhubs.md`) or with Kafka's own admin client (`CreateTopics`) when you want other settings;
- `session.timeout.ms` must be between 6000 and 300000 (Azure refuses 1000 and 3000, and 450000 and more), and Microsoft recommends 30000.

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
and producer ask for auto-creation of a missing topic, so a missing hub is created, as on Azure. `transactional.id` fails
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
| `AZEMU_EVENTHUBS_AUTOCREATE` | about AMQP only: a missing event hub is made by an AMQP client (4 partitions, one day). The Kafka endpoint makes a missing topic for a `Metadata` request that allows it (a producer, an admin client) whatever this says, with 1 partition and 7 days |
| `POST /_azemu/reset` | drops hubs, events, groups and committed offsets; waiting fetches end |

`GET /_azemu/kafka/{namespace}/groups` lists the groups (state, members, committed offsets), for looking at what a test left behind.

### Strict authorisation

With `--strict-auth` ([`../strict-auth.md`](../strict-auth.md)) the login is checked and nothing but `ApiVersions` and the login is answered before it succeeded (the connection is closed). `PLAIN`
needs the user `$ConnectionString` ("Invalid SASL PLAIN user name.") and a connection string ("The Service Bus connection string is not of the expected format. ...") whose
`SharedAccessKeyName` and `SharedAccessKey` are a rule of the namespace, or of the event hub in `EntityPath` ("Invalid SASL PLAIN password." for a wrong key and for an unknown name).
`OAUTHBEARER` needs an Entra token ("Invalid SASL OAUTHBEARER message." for a string that is no token, `InvalidSignature: ...` for a changed signature, "Invalid URI: The format of the URI could
not be determined." for another audience). A failed login is answered with `SASL_AUTHENTICATION_FAILED` and the connection is closed. After it a produce needs `Send`, a fetch, a list of
offsets, an offset fetch and an offset commit need `Listen`, and creating or deleting a topic needs `Manage`: the answer is `TOPIC_AUTHORIZATION_FAILED` (29) for that topic (a consumer with a
send-only key gets it at the offset fetch, as on Azure). A principal with no role is closed at the coordinator lookup, and after the answer to a produce. Checked against Azure in November 2026
with the four principals and the root, send-only and listen-only strings; not tried: the admin calls, a produce that makes a topic, `DescribeConfigs`.

## APIs

Served (the `ApiVersions` list; the maximum of each is the last version before the one that needs topic ids or another feature the emulator
does not have, and never more than a real namespace offers; the minimum is what the current Java client still sends. Azure lists 27 APIs, from Produce 0 to 7 to OffsetDelete):

| API (key) | Versions | Notes |
|---|---|---|
| Produce (0) | 3 to 7 | acks 0 gets no answer; idempotent producers; gzip and no compression; transactional batches refused (`INVALID_TXN_STATE`) |
| Fetch (1) | 4 to 12 | long poll (`max_wait_ms` up to 30 s, `min_bytes`), `max_bytes` and partition limits (at least one event is always returned); no fetch sessions (session id 0) |
| ListOffsets (2) | 1 to 6 | earliest (-2), latest (-1), by time; a time after the last event gives the end offset (Apache Kafka: -1) |
| Metadata (3) | 1 to 9 | auto-creation as above; replicas and in-sync replicas are empty lists |
| OffsetCommit (8) | 2 to 8 | metadata at most 1024 bytes |
| OffsetFetch (9) | 1 to 7 | all partitions of the group with a null topic list (nothing for a group without members); named partitions always get their commit |
| FindCoordinator (10) | 0 to 3 | group coordinators only (key type 0) |
| JoinGroup (11) | 1 to 7 | classic protocol; v4+ asks a new member to come back with its id (`MEMBER_ID_REQUIRED`) |
| Heartbeat (12) | 0 to 4 | |
| LeaveGroup (13) | 0 to 4 | |
| SyncGroup (14) | 0 to 5 | |
| DescribeGroups (15) | 0 to 5 | a group without members is `Dead` |
| ListGroups (16) | 0 to 4 | groups with members only |
| SaslHandshake (17) | 1 | |
| ApiVersions (18) | 0 to 3 | a version above 3 is answered as v0 with `UNSUPPORTED_VERSION` and the list, as the protocol asks (Azure resets the connection) |
| CreateTopics (19) | 0 to 5 | 1 partition and 7 days when not given; `retention.ms`; unknown settings are ignored; `cleanup.policy=compact` is `INVALID_CONFIG`; a bad name is `INVALID_CONFIG`; an existing topic is `TOPIC_ALREADY_EXISTS` |
| DeleteTopics (20) | 0 to 5 | |
| InitProducerId (22) | 0 to 4 | ids are handed out for idempotent producers; a `transactional.id` is refused (`UNSUPPORTED_VERSION`); epoch bumps (KIP-360) work |
| DescribeConfigs (32) | 0 to 2 | topics: `cleanup.policy`, `message.timestamp.difference.max.ms`, `message.timestamp.type` (`LogAppendTime`), `retention.ms`; a topic that does not exist has no settings and no error; the broker is `INVALID_REQUEST` |
| SaslAuthenticate (36) | 0 to 2 | |
| CreatePartitions (37) | 0 to 1 | always refused: the partition count of an event hub cannot change (`INVALID_REQUEST`) |

Not offered in `ApiVersions`, so clients do not call them (a request for one closes the connection): transactions (AddPartitionsToTxn,
AddOffsetsToTxn, EndTxn, WriteTxnMarkers, TxnOffsetCommit), ACLs, ElectLeaders, partition reassignment, log dirs, quotas, SCRAM credentials, DescribeCluster (the Java admin client
falls back to Metadata), the KIP-848 group protocol (ConsumerGroupHeartbeat), Fetch/Produce versions with topic ids. Azure does offer five that the emulator
does not: DeleteRecords (accepted and does nothing), DeleteGroups, OffsetForLeaderEpoch, AlterConfigs (`retention.ms` changes the event hub) and IncrementalAlterConfigs (answers OK and changes nothing), and OffsetDelete.
Event Hubs documents transactions only for Premium and Dedicated (preview) and compaction for the same tiers; they are not built here. Compression other
than gzip is refused as Event Hubs refuses it ("The message format version on the broker does not support the request").

## Mapping onto Event Hubs

Follows Microsoft Learn, "Exchange events between consumers and producers that use different protocols":

- **Kafka to AMQP**: the value is one `data` section (a null value is an event with no body section); the key becomes the
  `x-opt-partition-key` annotation, a string when it is UTF-8, else binary; each header is an application property whose value is an AMQP
  binary (a null header value is AMQP null), so an AMQP reader gets raw bytes, as the article says. The key is also kept as the annotation `x-opt-kafka-key`
  (Azure does so). The event's offset is its sequence number.
  An AMQP reader finds it with `event.partition_key`, `event.body`, `event.properties`.
- **AMQP to Kafka**: the body (all `data` sections joined, an `amqp-value` string or binary as its bytes, other values AMQP-encoded) is the
  value; the partition key is the key; each application property is a header whose value is the **AMQP-encoded** bytes, type byte
  included (a string `hello` is `A1 05 68 65 6c 6c 6f`; an int 1234 is `71 00 00 04 d2`), except a binary property, which comes out as its
  raw bytes (so what a Kafka producer wrote is what a Kafka consumer reads back). The properties of the AMQP properties section
  `message-id`, `user-id`, `to`, `reply-to`, `content-type`, `content-encoding`, `creation-time` come out as UTF-8 string headers of
  those names (`creation-time` as an ISO 8601 UTC time with 7 fractional digits, e.g. `2020-09-13T12:26:40.0000000Z`: checked against Azure in November 2026, which also passes on
  `message-id`, `to`, `reply-to`, `user-id`, `content-type` and `content-encoding` as text and drops `subject`, `correlation-id`, `absolute-expiry-time`, `group-id`, `group-sequence` and `reply-to-group-id`).
- Record timestamps are the time the event was enqueued (the time the emulator stored it), not the producer's `CreateTime`; the producer's
  time is not kept. A record that came in through Kafka is read as `LogAppendTime` (timestamp type 2, as on Azure, whose topic setting `message.timestamp.type` says so);
  an event that came in over AMQP is read as `CreateTime` (type 1) with its enqueued time. `ListOffsets` by time uses the enqueued time.
- A Kafka partition is a partition of the hub. Producers choose partitions themselves (the client's partitioner) and the record is stored in the partition of the
  produce request; its key is kept as the partition key and is not mapped to a partition again. An AMQP event sent without
  a partition goes to a partition by the partition-key rule of the Event Hubs service (`docs/services/eventhubs.md`, "Known differences") or round robin.
- Retention (by time, by hub size), the 1 MiB event limit (a batch or a record over it is `MESSAGE_TOO_LARGE`) and reset are
  those of the event hub, for both protocols. A reader whose offset was trimmed (below the first event) gets `OFFSET_OUT_OF_RANGE` and the client's
  `auto.offset.reset` decides; sequence numbers go on counting after events expire. A reader whose offset is **past the end** gets no error and no records, whatever
  `auto.offset.reset` says, and waits for the log to grow (Azure does so; Apache Kafka answers `OFFSET_OUT_OF_RANGE`).
- **Idempotent producers**: `InitProducerId` hands out a producer id and epoch; every partition remembers, per producer id, the next expected
  sequence number and the last 5 batches (base sequence, count, offset). A retry of a stored batch is answered with the original offset and
  stored once; a gap is `OUT_OF_ORDER_SEQUENCE_NUMBER`; an older epoch is `INVALID_PRODUCER_EPOCH`; an old sequence that is no longer
  remembered is `DUPLICATE_SEQUENCE_NUMBER`. A producer id a partition has not seen starts at whatever sequence it sends (lenient, so a
  producer that carries on after an emulator reset does not fail). At most 10,000 producers per partition are remembered.
- **Consumer groups**: the classic protocol, per namespace. The leader's assignment passes through untouched (any assignor works: range,
  round robin, sticky, cooperative-sticky). `JoinGroup` completes when every known member has joined, or at the rebalance timeout (the
  longest `max.poll.interval.ms` / rebalance timeout of the members) when it does not; the first round of a new group waits the initial delay.
  Members that stop heartbeating are dropped after their session timeout (6 s to 5 min; Azure refuses 3 s and 450 s, and the value between 300 s and 450 s where it stops was not found), checked when the group is touched and every second
  while a join waits. A member leaving, joining or expiring starts a rebalance: the others learn of it from `Heartbeat`
  (`REBALANCE_IN_PROGRESS`). Generation ids, `UNKNOWN_MEMBER_ID`, `ILLEGAL_GENERATION` follow Kafka. Committed offsets live in the group and
  survive the members; a commit without a generation (admin tools, `assign()` consumers) is accepted while the group has no members. **A group without members is forgotten**
  the way Azure forgets it: it is not listed, it describes as `Dead`, and the request for the offsets of every partition returns nothing; a request that names the partitions
  still gets the commits. (Azure did so 2 seconds after the last member left; the emulator does so at once.)
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

Compared with a real Standard namespace in November 2026 (librdkafka, Python `confluent-kafka` 2.16). Each line says what Azure does and why the emulator does not.

- **Compression.** Azure Standard refuses every codec, `gzip` included, with `UNSUPPORTED_FOR_MESSAGE_FORMAT`. Microsoft documents compression for Premium and Dedicated, and the emulator stands for the service and not for
  one tier, so it accepts `gzip`. `snappy`, `lz4` and `zstd` are not built and are refused with the code Azure Standard uses.
- **Compaction.** Azure Standard made an event hub with `cleanup.policy=compact` (a hub with `cleanupPolicy` Compact); the emulator does not compact, so it refuses with `INVALID_CONFIG` instead of keeping a setting it does not act on.
  `compact,delete` is `INVALID_CONFIG` on both.
- **Limits of the Standard tier are not copied** where Microsoft documents more for Premium or Dedicated: at most 32 partitions (33 is `INVALID_CONFIG`; the emulator allows 1,024), at most 7 days retention (8 is `INVALID_CONFIG`; the emulator keeps any
  positive `retention.ms`), 10 event hubs per namespace. Azure also rounds a retention up to whole days (2 hours became 1 day); the emulator keeps what it is given, to the millisecond. A retention of -1 is `INVALID_CONFIG` on both.
- **A produce request over about 1 MiB.** Azure resets the connection while the request is being sent (the client reports a timeout after its message timeout); the emulator answers `MESSAGE_TOO_LARGE`, which a program can act on. Up to
  1,048,000 bytes of record are delivered on both.
- **Several produce requests in flight.** Azure does not keep them in order (6 batches of 500 records: the first two were stored swapped); the emulator answers the requests of a connection one at a time, in order. With one request in flight, or an idempotent producer, Azure keeps the order too.
- **Authentication.** Azure closes the connection after the SASL exchange for a wrong key, an unknown policy, a user name other than `$ConnectionString`, a password that is not a connection string, a connection string of another
  namespace (`Invalid tenant name`), a bad `OAUTHBEARER` token, and for `SCRAM-SHA-256` without a reason; plain text and `SSL` without SASL do not work on 9093. The emulator accepts every credential and every shape, on purpose, and a plain connection too.
  `SCRAM` is refused with `UNSUPPORTED_SASL_MECHANISM`.
- **`ApiVersions` above the served versions.** Azure resets the connection; the emulator answers as the protocol asks (v0 with `UNSUPPORTED_VERSION` and the list).
- **APIs Azure has and the emulator has not:** DeleteRecords, DeleteGroups, OffsetForLeaderEpoch, AlterConfigs, IncrementalAlterConfigs, OffsetDelete. The versions of the others are Azure's or lower.
- **Offset commits.** Event Hubs documents a throttle of 2 commits per second per partition; 8 synchronous commits in a row all succeeded on Azure, and the emulator does not throttle.
- **Transactions** are not built. On Azure `init_transactions` times out (retriable `_TIMED_OUT`) because the transaction APIs are not offered; the emulator's `InitProducerId` with a transactional id is refused with `UNSUPPORTED_VERSION` and the client times out too. The code Azure sends on the wire was not seen.
- **Session timeout** between 300 s and 450 s: Azure's limit is somewhere there (300 s accepted, 450 s refused); the emulator's is 300 s.
- One broker, id 0, leader of every partition, no rack, no leader epochs beyond 0; `Metadata` lists no replicas and no in-sync replicas, as Azure does.
- No fetch sessions (every fetch is a full fetch), no incremental fetch, no throttling (`throttle_time_ms` is 0), no quotas.
- No authorization and no checks of credentials; SASL sessions do not expire.
- Static group membership (`group.instance.id`) is stored and handed back but does not fence a restarted member or survive a rebalance.
- Offsets are sequence numbers, so an AMQP `x-opt-offset` and a Kafka offset are different numbers for the same event.

### Checked against Azure in November 2026

Plain produce and fetch, keys, headers (repeated, null, binary), null key and value, explicit partitions, offsets per partition from 0, group join, sync, heartbeat, rebalance with a leaver and with a killed member,
commit and resume, auto commit, seek, assign, watermarks, the idempotent producer, `TOPIC_ALREADY_EXISTS` and `UNKNOWN_TOPIC_OR_PART`, regex subscriptions, the AMQP/Kafka mapping both ways, Kafka consumer groups kept
apart from the event hub's AMQP groups, `session.timeout.ms` of 6 s and more, the form of `creation-time` (7 fractional digits), and that a consumer that subscribes to a missing topic does not create it. Not tried: Premium and
Dedicated, the Java client and franz-go against Azure, Entra tokens, Kafka Streams and Connect, static membership, cooperative rebalancing.

## Keeping data between runs

Topics, records, committed offsets and idempotent producer state live in the Event Hubs state and are saved with it (`docs/services/eventhubs.md`, "Keeping
data between runs"). After a restart a consumer group is empty and its consumers join again; committed offsets stay; a producer that reconnects with
its producer id gets the answers Kafka specifies for a retry, a next batch and a gap.
