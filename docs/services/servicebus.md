# Service Bus (`azemu-servicebus`, `azemu-amqp`)

**What it emulates.** Service Bus over AMQP 1.0 (also over WebSocket, `docs/services/websocket.md`): queues, topics, subscriptions and rules (true, false, SQL and
correlation filters, SQL actions), peek-lock and receive-and-delete, locks and lock renewal, delivery count and dead-lettering, time to live, scheduled
messages, deferral, sessions, duplicate detection, forwarding, transactions (commit, rollback, time out), `$cbs` token puts, the `$management` operations,
and the HTTP management API (the ATOM/XML calls the administration clients make). It runs without SQL Server.

**How to reach it.** `Endpoint=sb://{namespace}.servicebus.localhost:4577;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any;UseDevelopmentEmulator=true`
(`sb://localhost:4577` also works). With `--standard-ports`: `sb://azemu:5672` from a container (the namespace is the first label of the host; the
administration client uses https on the same port, so the container needs the emulator's CA and `AZEMU_TLS_NAME=azemu`). A host with the label `eventhub`
is Event Hubs and not Service Bus (`docs/services/eventhubs.md`). Queues, topics and subscriptions must be created first (administration client, config
file, ARM) unless `--lenient`; a namespace needs no creation.

**Not built.** Authentication, rights and SAS expiry (every credential is accepted), partitioned entities (the flag is stored), `AutoDeleteOnIdle` (stored, nothing
is deleted), Azure's "one entity per transaction unless via" rule (not enforced), the REST message operations (`/{entity}/messages`, 501), geo-replication and
Premium features. Timing is not Azure's. The suite was never compared with Microsoft's own Service Bus emulator.

## How clients connect

| Variant | Works | How |
|---|---|---|
| Plain AMQP, `UseDevelopmentEmulator=true` | yes | `Endpoint=sb://default.servicebus.localhost:{port};SharedAccessKeyName=k;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true`. SDK 7.15.0 turns TLS off and keeps the port from `Endpoint=`. `Endpoint=sb://127.0.0.1:{port}` and `sb://localhost:{port}` work too (namespace `default`). |
| Custom port | yes | the port in `Endpoint=` is honoured (7.15.0). |
| AMQP inside TLS on the same port | yes | `ServiceBusClient("default.servicebus.localhost", cred, custom_endpoint_address="sb://default.servicebus.localhost:{port}", transport_type=TransportType.Amqp, connection_verify="~/.azemu/ca.pem")`. The SDK always dials 5671 for TLS unless a custom endpoint gives the port. The server offers ALPN `h2` and `http/1.1`; a client that offers neither (for example only `amqp`) is not refused. |
| SASL | yes | the SDK always starts with the SASL header; `ANONYMOUS`, `PLAIN`, `EXTERNAL`, `MSSBCBS` are offered and every credential is accepted. A client that sends the plain AMQP header first works too. |
| AMQP over WebSocket (`/$servicebus/websocket`, subprotocol `AMQPWSB10` or `amqp`) | **yes** | `transport_type=TransportType.AmqpOverWebsocket` with `custom_endpoint_address="http://myns.servicebus.localhost:{port}/"` (`https://` for TLS). The SDK ignores the port in `Endpoint=` over WebSocket. See `docs/services/websocket.md`. |

Other SDKs: any AMQP 1.0 client that speaks to Service Bus works the same way (the .NET emulator
connection string `Endpoint=sb://localhost;SharedAccessKeyName=...;UseDevelopmentEmulator=true;` reaches
namespace `default`).

## Namespaces: how HTTP and AMQP line up

- HTTP: the namespace is the first label of the `Host` (core: `{ns}.servicebus.localhost:{port}`).
- AMQP: the namespace is the first DNS label of the `hostname` field of the `open` frame (the SDK sends
  the host of `Endpoint=` without the port). `localhost`, an IP address, an empty hostname and a bare
  `servicebus` label select the namespace `default`.
- So `https://myns.servicebus.localhost:{port}` (admin) and `sb://myns.servicebus.localhost:{port}`
  (AMQP) are the same namespace `myns`, and `sb://localhost:{port}` is `default`, which is also what
  `https://default.servicebus.localhost:{port}` reaches.
- Namespaces exist for any name (nothing creates them; an entity is what makes one visible). Entity names are case-insensitive (original casing is kept).

## Auth

Every SASL mechanism and every credential is accepted. `$cbs` accepts `put-token` (SAS and JWT) with
`status-code` 202 `Accepted`; any other `$cbs` operation answers 501. The emulator does not check tokens,
rights or expiry. HTTP: any `Authorization` header is accepted; none gets 401.

## Entities

Queues, topics, subscriptions, rules. Created through the HTTP API (the same management calls as the Azure
administration clients), through a config file, or, only when you ask for it, by auto-creation.

- **Defaults as on Azure.** Nothing comes into being by being named. A sender, a receiver or a management call
  on a queue, topic or subscription nobody created fails with `amqp:not-found` and the message real Service Bus uses
  ("The messaging entity 'sb://...' could not be found. To know more visit https://aka.ms/sbResourceMgrExceptions.
  TrackingId:..."); the Python, JavaScript, .NET, Java and Go clients show it as their "entity not found" error.
  Create entities first, as a team does against Azure: with the administration client of the SDK
  (`ServiceBusAdministrationClient.create_queue(name)` and the same in the other languages), with the config file
  below, or with ARM (Terraform `azurerm_servicebus_queue`).
  **Namespaces are different:** the emulator has no way to create a namespace outside ARM, so any namespace name
  works (`myns.servicebus.localhost`), as soon as you create an entity in it.
- **Auto-creation** (`AZEMU_SERVICEBUS_AUTOCREATE=1`, or `AZEMU_LENIENT=1` / `--lenient`, which also turns on the other
  conveniences; the variable of the service can switch this one on or off whatever `AZEMU_LENIENT` says): attaching to a missing
  entity creates it: `name` becomes a queue, `topic/Subscriptions/sub` creates the topic (if missing) and the subscription
  with a `$Default` rule, a session receiver creates a session-enabled queue. A queue created this way
  is still adjustable: a later session receiver turns it into a session queue, a later subscription
  receiver replaces it by a topic (messages sent before any subscription existed are lost, as they
  would be on a topic). It is a convenience Azure does not have; use it for quick scripts, not for tests that should hold
  against Azure.
- **Config file** (`AZEMU_SERVICEBUS_CONFIG=/path/Config.json`), loaded at start and again after every
  reset: the format of Microsoft's Service Bus emulator (`UserConfig.Namespaces[].Queues / Topics /
  Subscriptions / Rules`, `Properties` with `LockDuration`, `MaxDeliveryCount`, `DefaultMessageTimeToLive`,
  `DeadLetteringOnMessageExpiration`, `RequiresSession`, `RequiresDuplicateDetection`,
  `DuplicateDetectionHistoryTimeWindow`, `ForwardTo`, `ForwardDeadLetteredMessagesTo`; rules with
  `FilterType` `Sql` (`SqlFilter.SqlExpression`, optional `Action.SqlExpression`), `Correlation`, `True`,
  `False`). When the file names exactly one namespace, `default` is an alias of it, so
  `Endpoint=sb://localhost` reaches the configured entities (Microsoft's emulator has one namespace and
  ignores the host). A short file that creates one queue and one topic with a subscription, so that a test run needs no
  administration calls:

  ```json
  {"UserConfig": {"Namespaces": [{"Name": "default",
    "Queues": [{"Name": "orders", "Properties": {"LockDuration": "PT30S"}}],
    "Topics": [{"Name": "events", "Subscriptions": [{"Name": "all"}]}]}]}}
  ```

  Start with `AZEMU_SERVICEBUS_CONFIG=/path/Config.json azemu`; the entities are back after every `POST /_azemu/reset`.

## Messaging semantics built

| Area | Behaviour |
|---|---|
| Send | single message and the batch format `0x80013700`; every section is preserved; a batch is validated as a whole before anything is stored |
| Properties | message-id (a message sent without one gets a GUID, 32 hex digits, when it is stored; the JavaScript SDK cannot settle a message that has none), group-id (session), correlation-id, to, reply-to, reply-to-group-id, subject, content-type, ttl, application properties, annotations (`x-opt-partition-key`, ...). Set on delivery: `x-opt-enqueued-time`, `x-opt-sequence-number`, `x-opt-locked-until`, `x-opt-enqueue-sequence-number` (forwarded), `x-opt-deadletter-source`, header delivery-count (0 on first delivery), creation-time, absolute-expiry-time; the delivery tag is the lock token (GUID bytes) |
| Receive modes | peek-lock (rcv-settle-mode second: unsettled disposition from the client is answered by a settled disposition that echoes the applied outcome: accepted, modified, rejected-without-error for a dead-letter; a failure is `rejected` with `com.microsoft:message-lock-lost` / `session-lock-lost`) and receive-and-delete (pre-settled) |
| Settlement | complete = accepted; abandon = `modified` without `undeliverable-here` (the delivery count goes up whatever `delivery-failed` says: Python sends it as true, the JavaScript and Go SDKs leave it out or send false); defer = `modified{undeliverable-here}`; dead-letter = `rejected` with `com.microsoft:dead-letter`, reason/description from `info`; released = unlock without counting. The `message_annotations` of abandon and defer, and every `info` entry of a dead-letter other than the reason and the description, are written to the application properties of the message (`propertiesToModify` of the SDKs). By lock token over `$management` `update-disposition` (`completed`, `abandoned`, `defered`, `suspended`) |
| Locks | `LockDuration`, expiry returns the message with delivery count + 1; at `MaxDeliveryCount` it goes to the DLQ with `MaxDeliveryCountExceeded`; `renew-lock` |
| TTL | entity default is a ceiling for the message TTL; expiry is lazy (applied by a 100 ms background tick and by every operation), dead-lettered with `TTLExpiredException` when `DeadLetteringOnMessageExpiration`; a locked message does not expire |
| DLQ | `q/$DeadLetterQueue`, `t/Subscriptions/s/$DeadLetterQueue`; `$Transfer/$DeadLetterQueue` addresses are accepted (always empty) |
| Sessions | session queues/subscriptions, specific session or next available (waits up to `com.microsoft:timeout`, default 65 s, then detaches with `com.microsoft:timeout`), a wait for the next available session ends when its link, session or connection ends (it never locks a session for a receiver that is gone, and the receiver is bound to the session it got), session lock with renewal, state get/set, session listing, ordering by sequence number (an abandoned message keeps its place), `session-cannot-be-locked` for a held session; closing a session receiver releases the session and its locked messages without counting a delivery; a settlement, `update-disposition`, `get-session-state`, `set-session-state` or `renew-session-lock` after the session lock ran out fails with `com.microsoft:session-lock-lost` (not `message-lock-lost`); session state is limited to 256 KB; scheduled messages of a topic count against the topic's `MaxSizeInMegabytes` |
| Topics | copy to each subscription; rules: SQL filters (`azemu-servicebus-sql`), correlation, true, false; actions `SET`/`REMOVE`; a rule without action ORs into one copy, each matching rule with an action gives its own copy |
| Forwarding | `ForwardTo`, `ForwardDeadLettered MessagesTo` for queues and subscriptions, chains up to 4 hops (more is `amqp:resource-limit-exceeded`) |
| Scheduling | `x-opt-scheduled-enqueue-time` on a send, `schedule-message`, `cancel-scheduled-message`; topic sends are scheduled at the topic and fan out when due |
| Duplicate detection | by message-id inside `DuplicateDetectionHistoryTimeWindow`; a duplicate is accepted and dropped |
| Limits | `MaxMessageSizeInKilobytes` (256 default; `amqp:link:message-size-exceeded` and the link's max-message-size), `MaxSizeInMegabytes` (`amqp:resource-limit-exceeded`, counts main + DLQ bytes), disabled entities (`com.microsoft:entity-disabled`) |
| Credit | link credit and the session window are respected; competing receivers get a message exactly once; drain works |
| Receiver gone | locks of a closed link or dropped connection stay until the lock expires (documented Service Bus behaviour for peek-lock); a session receiver releases its session at once. A `released` disposition (what the SDK sends for prefetched messages it will not use) unlocks immediately |
| Reset | drops every entity and message, closes no connection; a link on a dropped entity is detached with `amqp:not-found` the next time it is served, sends to it are rejected with the same error; then the config file is loaded again |

### `$management` operations

A response goes to one response link: the link whose target address is the request's `reply-to`, else the link on the
request's session (the pair the request link attached with), else every response link of the node. `peek-message` returns at most 250 messages and about 8 MiB per answer; request links
accept messages up to 16 MiB (`$management`) and 1 MiB (`$cbs`).

`renew-lock`, `peek-message`, `schedule-message`, `cancel-scheduled-message`,
`receive-by-sequence-number`, `update-disposition`, `renew-session-lock`, `get-session-state`,
`set-session-state`, `get-message-sessions`, `add-rule`, `remove-rule`, `enumerate-rules`. Responses carry
`statusCode`, `statusDescription`, `errorCondition` and the request's correlation id (`$cbs` uses
`status-code` / `status-description`). Any other operation answers 501 naming it.
`add-rule` takes the `rule-description` as the JavaScript SDK sends it: a map with `sql-filter` (`expression`, `parameters`),
`correlation-filter` (`correlation-id`, `message-id`, `to`, `reply-to`, `label`, `session-id`, `reply-to-session-id`,
`content-type`, `properties`) and optionally `sql-rule-action` (`expression`); the described-list form is accepted too.
`enumerate-rules` answers each rule as the described list `[filter, action, name, created-at]` with the codes of
`@azure/core-amqp`: rule description `0x0000013700000004`, empty action `0x...05`, SQL action `0x...06`; filters are one
digit shorter: SQL `0x0000001370000006`, true `...07`, false `...08`, correlation `...09`. The `$Default` rule is answered as a true
filter. The correlation filter always has its `properties` map (the JavaScript SDK throws on null). SQL parameters have no place in
the answer. Tested with the JavaScript SDK (`tests/sdk-js`, `servicebus_rules`) and `crates/services/servicebus/tests/rules_js.rs`; the Python SDK does not use these calls.

## Known differences from Azure Service Bus and from Microsoft's emulator

- A duration just below `TimeSpan.MaxValue` (what the Python SDK writes back for the maximum, `P10675199DT2H48M5.477539S`) is kept as the maximum `P10675199DT2H48M5.4775807S`; Azure keeps the exact value (durations are stored in milliseconds here).

- No authentication or authorization; no rights, no SAS expiry checks.
- A settlement or session call after the session lock ran out fails with `com.microsoft:session-lock-lost`. That is inferred from the strings in the .NET and Java SDKs and from
  Microsoft Learn ("the session lock has expired"); the real service's exact answer is not recorded.
- Rule-evaluation exceptions never dead-letter (the message simply does not match).
- Sequence numbers are per sub-queue counters starting at 1 (no partition bits); a dead-lettered message
  gets a new sequence number in the DLQ.
- Transactions: see the section below. A transaction may touch several entities with or without a send-via link (Azure's rule for
  that is not checked: the Java SDK does it without a via link); the error for an operation limit of 100 and the time out
  error conditions are the AMQP ones (`amqp:resource-limit-exceeded`, `amqp:transaction:timeout`), Azure's own text is not known.
- `AutoDeleteOnIdle` is stored but nothing is deleted. Partitioning flags are stored, entities are not partitioned.
- Settlement in receiver settle mode `second` completes in one step (the emulator's settled disposition follows the
  client's unsettled one): the Python SDK's sender never sends the extra settling disposition.
- A scheduled message in a topic is stored at the topic and fans out when due; it is visible in no subscription's peek before that.
- Round-robin among waiting receivers is "whoever pulls first"; fairness is not guaranteed.
- The idle timeout the server advertises is 240 s and the server closes a connection that sends nothing
  for that long; the server sends empty frames at half of the peer's advertised idle timeout.
- Frame size advertised 1 MiB, session window 2048 transfers, link credit granted to senders 1000.
- Limits of the emulator: 5,000 connections per server, 1,000 sessions and 10,000 links per
  connection, 64 MiB of half-received messages per connection, 10,000 queues + topics per namespace, 2,000 subscriptions per
  topic (the last two are Azure's quotas). Nothing caps the total size of all entities together.
- The config file is checked as a whole: a duration that is not ISO 8601 (`LockDuration`, `DefaultMessageTimeToLive`,
  `DuplicateDetectionHistoryTimeWindow`, `AutoDeleteOnIdle`) refuses the file with a message naming the entity (logged at
  error level) and nothing from it is created. `config::try_apply_properties` does the same check for other callers;
  `config::apply_properties` stays lenient.

## Transactions

Built for Service Bus (Event Hubs has none: the coordinator attach is still refused there with `amqp:not-implemented`).

- **Coordinator link.** A sender link whose target is `amqp:coordinator` is accepted (no `$cbs` token needed, one link serves
  every entity). A message on it is `declare` (answered `declared{txn-id}`, a new 16 byte id) or `discharge{txn-id, fail}`
  (`fail` false commits, true rolls back; answered `accepted`, or `rejected` with `amqp:transaction:unknown-id` for an id that
  is unknown, already finished, from another connection or whose controller link ended, `amqp:transaction:timeout`,
  `amqp:transaction:rollback` when the commit could not be applied). A receiver attach to the coordinator is refused (`amqp:not-allowed`).
- **Transactional work** carries `transactional-state{txn-id}`: a transfer to a queue or topic (a send, single message or the batch
  format; answered `transactional-state{txn-id, outcome: accepted}`, or with the `rejected` outcome inside it); a disposition of a received
  message (complete, abandon, defer, dead-letter; answered with the same wrapped outcome); and requests to `<entity>/$management`
  that change something: `schedule-message` (the sequence numbers are answered at once and the messages are stored under them at commit),
  `cancel-scheduled-message`, `update-disposition`, `set-session-state`. The Java SDK sends a transaction id with `schedule-message` and
  `update-disposition` only (checked in the class files); the others are built because the AMQP rule is the same. Every other
  `$management` operation (peek, renew lock, receive by sequence number, session listing, rules) ignores a transaction id.
- **How it works.** Nothing changes while a transaction is open. Each operation is checked when it is made (the entity exists and takes
  the message: disabled entity, message size, room, session id; the lock is valid) and remembered. The commit runs under one hold of the state
  lock: it first applies what is due by now (expired locks and TTLs), checks what does not depend on order (entities exist, messages allowed,
  lock tokens valid and not settled twice, scheduled messages to cancel exist, session locks held), then applies the operations in the order they
  were made, through the same code a plain send or settlement uses (duplicate detection, sessions, scheduled messages, forwarding, topic fan-out
  with rules, dead-lettering, waking of waiting receivers), while every store keeps a journal. The first operation that fails (no room left in a
  queue after the earlier operations of the transaction, a subscription that is full, a forwarding chain that is too long, a lock that went away)
  takes everything back (messages, byte counts, duplicate ids, session state) and the discharge is rejected with `amqp:transaction:rollback`: all
  or nothing, and nobody sees half a transaction. Room is therefore decided in the order of the operations (a complete before a send frees room).
  A rollback, a time out, the end of the controller link or of the connection just forget the operations: a message completed in the transaction
  was never touched and stays locked by its receiver until its lock runs out (it can still be settled by lock token over `$management`; the
  delivery itself is settled on the wire).
- **Scope: one entity per transaction.** A transaction belongs to the connection that declared it. It works on one entity (queue or
  subscription; a topic send counts as the topic) from any link of the connection. Work on a second entity is accepted only when it comes over a link on the
  same AMQP session as the link that first used the transaction, or over a send-via link (a sender attached with
  `com.microsoft:transfer-destination-address`, target = the via entity: the property is echoed and the messages go to the destination); otherwise it is
  rejected with `amqp:not-allowed` and the transaction stays usable. Why the session: the .NET SDK (`Azure.Messaging.ServiceBus` 7.21) with
  `EnableCrossEntityTransactions` puts every link of the client and the coordinator on one session, and without the option gives each link its own
  (seen on the wire); the Java SDK passes its cross-entity test the same way. The SDKs send no via property in either mode.
  The error condition and text are a guess (no recording of Azure's answer).
- **Time out and limits.** A transaction times out 2 minutes after it was declared (`AZEMU_SERVICEBUS_TXN_TIMEOUT_SECONDS`, or `Shared::set_txn_timeout`);
  the work it holds is let go by a sweep on its connection, and later work or a discharge gets `amqp:transaction:timeout` once, then
  the id is unknown. At most 100 operations per transaction (messages sent, scheduled or cancelled, and settlements each count one; the one that goes
  over is rejected with `amqp:resource-limit-exceeded` and the transaction stays usable) and 1,000 open transactions per connection. Bytes held by open
  transactions (messages and what settlements carry) are limited to 128 MiB per transaction, 256 MiB per connection and 1 GiB for the service
  (`AZEMU_SERVICEBUS_TXN_MAX_MB`, `AZEMU_SERVICEBUS_TXN_CONNECTION_MAX_MB`, `AZEMU_SERVICEBUS_TXN_TOTAL_MAX_MB`); past a bound the operation is rejected with
  `amqp:resource-limit-exceeded`. The bytes are let go on commit, rollback, time out, end of the controller link, session or connection.
- **Not built / not known.** Azure's exact error text for the limits and the one entity rule; a transaction spanning connections; a message settled in a
  transaction does not pin its lock (a lock that runs out before the commit fails the commit).

## HTTP management API

### Status (admin module finished)

Wire format is copied from the recordings in `refs/recordings/python/sdk/servicebus/azure-servicebus/tests/recordings/mgmt_tests`.
Replay: see `fidelity/SCORES.md` (the entries that fail are DELETEs of entities that
earlier tests in the real account had left behind).

| Operation | Status |
|---|---|
| `PUT/GET/DELETE /{queue}` and `/{topic}` (create 201, update with `If-Match` 200) | built |
| `GET /$Resources/queues`, `/$Resources/topics` (`$skip`, `$top`, next link) | built |
| `PUT/GET/DELETE /{topic}/subscriptions/{sub}`, `GET .../subscriptions` | built |
| `PUT/GET/DELETE .../rules/{rule}`, `GET .../rules` (true, false, SQL, correlation filters, SQL actions) | built |
| `GET /$namespaceinfo`, `GET /` (service document) | built |
| runtime data (`CountDetails`, `SizeInBytes`, `MessageCount`, `SubscriptionCount`, filter counts) | built, from `Core::counts()` / `size_bytes()` |
| REST message operations (`/{entity}/messages`), `$Resources/` of other kinds, any other path | 501 naming the request |

Behaviour worth knowing:

- Any `Authorization` header is accepted; none gets 401 `MissingToken`. `PUT /` (empty name) is 401 as recorded.
- Update is `PUT` with `If-Match`; it replaces everything the description carries (a missing element goes back
  to its default, so omitting `ForwardTo` clears it). `RequiresSession`, `EnablePartitioning`,
  `RequiresDuplicateDetection` cannot change (400). Validation is done before the existence check, as in the recordings.
- Validation (400): lock duration over 5 min (queues, subscriptions), duplicate window outside 20 s to 7 days,
  `MaxMessageSizeInKilobytes` over 256 (Standard) or sent with api-version before 2021-05, `MaxDeliveryCount` under 1,
  `AutoDeleteOnIdle` under 5 min, unknown `Status`, bad values, bad SQL in rules.
- `GET` of an unknown queue or topic answers 200 with the "Publicly Listed Services" feed, as the real service does
  (the SDK turns the missing content into `ResourceNotFoundError`). Unknown subscription or rule: 404.
- A partitioned entity shows 16 times the requested `MaxSizeInMegabytes` and `SupportOrdering` false; sending the shown
  total back on update does not multiply it again.
- `ForwardTo` is stored as the entity path and shown as `sb://{Host header}/{path}`; the target is not checked.
- The ids and links of the entries of a list (`<id>`, `<link rel="self">`) repeat the casing of the request path: a client that asks
  for `/{topic}/Subscriptions` or `.../Rules` gets `/Subscriptions/` and `/Rules/` back, one that asks in lower case (the Python SDK)
  gets lower case. The JavaScript SDK finds the names in the entries by matching `/Subscriptions/` and `/Rules/` exactly.
  Single entries already echoed the request URL. The recordings only hold lower case requests, so the real service's own casing for
  a capitalised request is inferred from that SDK.
- `DefaultRuleDescription` on create subscription (the JavaScript SDK's `defaultRuleOptions`) replaces `$Default`: the first rule has
  the name and filter that were sent (`$Default` when the description has no `Name`).
- The `$Default` rule is the engine's SQL `1=1` and is shown as `TrueFilter`; a `TrueFilter` created over HTTP is `RuleFilter::True`.
- A .NET duration in a correlation property is kept as a marked string (`\0azemu-duration:PT1H`) because the SQL value
  type has no duration; it round-trips through the API but never matches a message property.
- `SqlFilterCount` / `CorrelationFilterCount` of a topic are sent from api-version 2024-05 (the SDK default), not for 2021-05.
- Not checked: that `ForwardTo` targets exist, entity name characters, `MaxSizeInMegabytes` values, auto-delete on idle
  (stored only), authorization rule enforcement.

## Behaviour that could not be confirmed against Azure

- Value of `x-opt-deadletter-source` for a subscription: the docs say "the name of the queue or subscription",
  so the entity's own name is used (`sub`, not `topic/Subscriptions/sub`).
- `add-rule` / `enumerate-rules`: the encoding is the JavaScript SDK's (tested against it, no recording of the live service).

## Reset

`POST /_azemu/reset` empties everything and loads the config file again. `POST /_azemu/reset?service=servicebus&account=<namespace>` empties
that namespace only (`myns` for `myns.servicebus.localhost`), puts back what the config file declares for it, releases receivers waiting
in it (they are told the entity is gone) and leaves the other namespaces alone. `Shared::reset_namespace(name)` is the same in Rust.

## Keeping data between runs

With `--data-dir` the service saves `servicebus.state` (namespaces, entities, settings, rules) and one file per queue, subscription and topic (messages in
every state, sequence counters, session state, duplicate ids). Locks, connections and transactions in flight are not stored; a message that was locked
comes back with one more delivery attempt counted (dead-lettered at the delivery limit). With saved state the config file is not applied again. Details:
`docs/services/persistence.md`, section "Cosmos, Event Grid, Service Bus, Event Hubs with Kafka, ARM".

## Limits that keep a small request from filling the memory

- A batch (one AMQP transfer in the batch format, or one `schedule-message` request) holds at most 50,000 messages (`MAX_MESSAGES_PER_BATCH`). Microsoft
  Learn's quotas page limits a batch by size only (1 MB), so the number is ours; it is what Event Hubs uses. Over it the transfer is rejected with
  `amqp:link:message-size-exceeded` before any message is decoded. Without it an entity with a larger `MaxMessageSizeInKilobytes` accepted a batch of
  hundreds of thousands of empty messages (a few bytes each on the wire, about 800 bytes each in memory); 50,000 empty messages were stored and
  charged a few hundred bytes against `MaxSizeInMegabytes`, over and over.
- Every stored message is charged its encoded size plus `MESSAGE_OVERHEAD` (1,024 bytes; a test checks it against `size_of`) against `MaxSizeInMegabytes`, in
  every path that stores a message (send, batch, scheduled, transaction commit, topic copies, Event Grid deliveries, forwards, dead-letter moves). `SizeInBytes`
  shows the charge. A batch for a queue or subscription must fit as a whole or it is refused (`amqp:resource-limit-exceeded`).
