# Persistence (`--data-dir`)

**What it does.** State is in memory unless the emulator is started with a data directory (`--data-dir <path>` or `AZEMU_DATA_DIR`). Then every
service's state is read at start and written back while it runs and when it stops. Every service keeps everything a client can observe:
blobs and Data Lake paths, queues and their messages, tables, vaults (secrets, keys, certificates), App Configuration stores, Cosmos accounts and
documents, Service Bus and Event Hubs entities with their messages and events (and Kafka offsets), Event Grid topics and subscriptions, ARM resources,
and the identity signing key.

**What a user should know.**

- It is saved every `--save-interval` seconds (default 15) when something changed, on `POST /_azemu/save` (the answer comes when the files are on disk),
  and on Ctrl-C or SIGTERM (requests that are running get a few seconds to finish; a second signal quits at once without saving). **A crash (kill -9, power
  loss) loses at most the changes since the last save.** Services are saved one after the other, so after a crash two services can be a few seconds
  apart; a Service Bus message that was moving between two entities can be in both or in neither; ARM and the data-plane services can disagree about what exists.
- Writes are atomic (temporary file, flush, rename, directory flush), so a kill in the middle of a save leaves the previous save readable. This was tried:
  twelve kills during saves, twelve clean starts.
- One emulator uses a directory at a time (`LOCK`). If a state file cannot be read, the emulator does not start and names the file; `--data-discard-unreadable`
  moves that service's files to `<service>.unreadable-<time>` and starts it empty.
- **The files are as sensitive as the services.** Key Vault secret values and private keys, and the identity signing key, are written in the clear. The data
  directory is mode 0700 and the state files are mode 0600 (set again at every start). Do not copy them anywhere others can read.
- Files written by this version stay readable by later ones: each state file kind has a version-1 fixture under its crate's `tests/state-v1/` and a test
  that loads it. (Nothing else tests that a directory moves between versions.)
- Not kept: open connections, message locks of a receiver that went away, Kafka group members, cached query results, running scripts, and the settings of the run (ports and `AZEMU_*` variables).
- Blob data is kept in write-once piece files under `blob/pieces/`, so a save after a small change writes the small change; a started emulator still holds all of it in memory.

What follows is the contract between core and the services (for people who write or change a service), and per-service notes on what is stored.

## The contract

Three methods on `azemu_core::Service`, all with defaults, so a service that does nothing keeps working:

```rust
/// None: nothing to keep. Some(n): n changes whenever the state that `save` writes has changed.
fn persist_generation(&self) -> Option<u64> { None }
/// Write the state into `dir` (owned by this service, exists). Blocking thread, one call at a time.
fn save(&self, dir: &Path) -> io::Result<()> { Ok(()) }
/// Read back what `save` wrote. Once, at start, on an empty service, before any listener opens.
fn load(&self, dir: &Path) -> io::Result<()> { Ok(()) }
```

Rules for a service:

- `save` must not hold the service's lock while it encodes or writes. Take what it needs under the lock (cheap clones
  of `Arc`s and `Bytes`), release, then write.
- Files are replaced as a whole (`persist::save_file`, `persist::write_atomic`); files that no longer belong to the
  state are removed. A crash during a save leaves the previous save readable.
- One format for every state file: `persist::encode` / `decode` (a header line with kind and version, then
  MessagePack with field names). A service raises its version when an old file can no longer be read, and may keep
  reading older versions. A file it cannot read is an error that names the file: the emulator does not start.
- Times are stored as absolute times, so leases, locks, TTLs, retention and scheduled work mean the same after a
  restart. What cannot survive a restart (open connections and links, a message locked by a receiver that is gone,
  consumer group members, cached query results, running scripts) is not stored, and the state after `load` is what
  Azure would show after every client disconnected.
- `persist_generation` is a counter raised on every change (a reset is a change). It is polled, so it must be cheap.
- Loading is bounded like any other input: a state file is not trusted more than a request (no panics, no
  allocation from a count before the bytes are there, depth limits), because people copy data directories around.

What core does with it: `--data-dir <path>` (also `AZEMU_DATA_DIR`); `<path>/<service name>/`
per service; load at start; save every service whose generation changed every `--save-interval` seconds (default
15; 0 means only on request and at shutdown); save on SIGINT / SIGTERM before exit; `POST /_azemu/save` saves now
and answers when it is on disk; `POST /_azemu/reset` empties the state and the next save writes that.


## Core driver, Blob and Data Lake, Queue, Table, Key Vault, App Configuration, Identity

All of it is built and tested in Rust and Python. Nothing in
`crates/azemu` needs a line: `run_cli` does the wiring for the unified binary and for every per-service binary.

**Helpers added to `persist.rs`** (nothing existing changed): `write_atomic_private` and `save_file_private` (file mode 0600),
`save_file_if_changed(dir, file, kind, version, &value, private) -> io::Result<bool>` (writes nothing when the bytes are the same; the
encoding must not depend on hash order, so snapshot a `HashMap` into a `BTreeMap` first). Core makes each service directory mode 0700
and tightens an older one to 0700 at start.

**One change inside an existing function (read this before you write a reader).** `persist::decode` now reads MessagePack with a nesting limit of
`persist::MAX_DEPTH` = 200 levels instead of the reader's default of 1024. A state file with a JSON document nested a thousand deep made the
load abort the whole process with a stack overflow (blocking threads have 2 MiB; a debug build needs more than that for 1024 levels). A JSON
request is limited to 128 levels, so a stored document never needs more; a file with more is an error that names the file. The signatures, the
file format and what a valid file looks like are unchanged. A service with its own readers of untrusted data should set a limit too.

**Things I learned that the contract text does not say** (none of them needs a change to the three methods):

- Bump the generation *after* the change, inside the lock that made it. A save that reads the number and then copies the state must never see
  a number that is ahead of the state. Bumping later than the change only costs one extra save.
- A request that reads is not a change, but check what the service does on reads: Blob's Data Lake side flips a container to hierarchical on a
  first read and takes in blob-endpoint writes; those set a flag that counts the request as a change. Time applied lazily (an expired lease,
  retention, a message past its time to live) is not a change as long as the absolute times are saved.
- `load` runs on a service that already exists, so anything the constructor started in the background (Identity made its key in a task) has to
  give way to what `load` read. Identity now has a slot that the load fills and the generating task does not overwrite.
- A stored absolute URL is wrong after a restart on another port. Key Vault stored the `kid` of the secret behind a certificate as a full URL; it
  is now stored as a path and given the request's address when it is returned (the only behaviour change outside persistence). Look for the same in
  your services: any `format!("{base}/...")` kept in state.
- `RunningEmulator`/`TestServer` dropped without `shutdown` does not save. A test that wants the state on disk calls `save_now()` or `shutdown()`.
- Saves do not hold the service lock for encoding or writing, but every service here copies its state under the lock first. Where the state is
  big and made of owned strings (Table entities, Queue messages) that copy is a deep copy; the black-box test in each crate (a request during a
  save of a big state must not wait for it) is the check that it stays small.

**What is stored where** (kind / version / file; layout details are in each `docs/services/<name>.md`, section "Keeping data between runs"):

| Service | Directory | Files | Kind, version |
|---|---|---|---|
| `blob` (also holds all Data Lake state; `dfs` returns `None`) | `blob/` | `blob.state`, `pieces/<32 hex>` (data, written once) | `blob` 1 |
| `queue` | `queue/` | `queue.state` | `queue` 1 |
| `table` | `table/` | `table.state` | `table` 1 |
| `keyvault` (0600, private keys in the clear) | `keyvault/` | `keyvault.state` | `keyvault` 1 |
| `appconfig` | `appconfig/` | `appconfig.state` | `appconfig` 1 |
| `identity` (0600, the signing key) | `identity/` | `identity.state` | `identity` 1 |

**Not stored**, by design: connections; settings of the run (`AZEMU_*` variables, limits); account keys (derived from the account name); the
App Configuration `Sync-Token` counter; events waiting to be delivered to Event Grid (the sinks are other services' state); a Blob copy that was still
running is stored with the state it had (copies finish at once here, so there is none).

## Cosmos, Event Grid, Service Bus, Event Hubs with Kafka, ARM

All five implement the three methods of the contract unchanged. Files are in `<data-dir>/<service name>/`, mode 0600, kind
`AZEMU-STATE <kind> 1`. Large state is split so that a save writes only what changed: every service keeps a revision number per file (a number taken
from a process-wide counter at every change, never reused, so a deleted and re-made entity cannot look like the saved one), the highest number is
`persist_generation`, and a save copies under the lock only the entities whose number differs from the one at the last save. The copy is an `Arc`
per document / message / event (Service Bus `Msg.message` and Cosmos `Doc.value` are `Arc`s; Event Hubs events already were), encoding and writing
happen after the lock is released, straight into the file (`crates/statefile`, a new small crate: header line, MessagePack with names, temporary
file, `fsync`, rename, mode 0600; reading refuses a file over 2 GiB before reading it).

| Service | Files (in its directory) | Kind |
|---|---|---|
| `cosmos` | `cosmos.state` (accounts, databases, containers and every policy, users, permissions, scripts, offers, id counters), `docs-<hash>.state` per container (documents, LSN and document counters, the "all versions and deletes" history) | `cosmos`, `cosmos-docs` |
| `servicebus` | `servicebus.state` (namespaces, entities, settings, rules), `queue-<hash>.state`, `sub-<hash>.state`, `topic-<hash>.state` (messages of one queue / subscription / the scheduled messages of a topic, sequence counters, session state, duplicate ids, messages waiting to be forwarded) | `servicebus`, `servicebus-entity` |
| `eventhubs` (Kafka shares it; `kafka` returns `None`) | `eventhubs.state` (namespaces, hubs, consumer groups, Kafka committed offsets, next Kafka producer id), `part-<hash>.state` per partition (events, counters, Kafka producer sequence state) | `eventhubs`, `eventhubs-partition` |
| `eventgrid` | `eventgrid.state` (topics, domains, namespaces, event sources, subscriptions with their configuration and validation state), `sub-<hash>.state` per subscription (pull queue with locks and delivery counts, dead letters, delivery log, unfinished push deliveries) | `eventgrid`, `eventgrid-sub` |
| `arm` | `arm.state` (subscriptions, groups, scope tags, tag names), `scope-<hash>.state` per resource group (and per subscription for what lives outside groups): resources with their private data and the deployment history | `arm`, `arm-resources` |

The `<hash>` is a 64 bit hash of the names; the file says which entity it holds and a file that holds another one is an error naming it.

**Order and crashes.** Entity files are written first, the definitions file last, files nothing refers to are removed at the end. A kill in the middle of
a save leaves a save that loads, but the entity files can come from two moments: a Service Bus message that was moving between two queues
(auto-forwarding, a transaction) at that moment can be in both or in neither. The whole of a single entity is always from one moment.

**Costs** (debug build, this VM, disk not on `/tmp`; numbers from the `#[ignore]` tests `cost_of_a_small_save_next_to_100_mb` in each crate; run with
`AZEMU_PERSIST_BENCH_DIR=<dir> cargo test -p <crate> --test persist -- --ignored --nocapture`):

| Service | State | First save | Save after one small change in another entity | Restart (load) |
|---|---|---|---|---|
| Cosmos | 50,000 documents, 116 MB | 0.72 s | 15 ms | 0.61 s |
| Service Bus | 35,000 messages of 3 KB, 111 MB | 0.38 s | 19 ms | 0.29 s |
| Event Hubs | 35,000 events of 3 KB, 108 MB | 0.39 s | 24 ms | 0.17 s |
| Event Grid | 60,000 events of 1.5 KB, 98 MB | 0.56 s | 31 ms | 0.38 s |
| ARM | 16,000 resources of 6 KB, 105 MB | 0.59 s | 28 ms | 0.21 s |

A change inside the big entity itself writes that entity's file again (about the time of the first save). Splitting one queue or one container into
several files would avoid that; it was left out on purpose. A save with nothing changed costs nothing (the generation did not move) and a poll that
did move it (a query, a read that took a lock) finds every file's number unchanged and writes nothing.

**Cosmos.** Stored: everything in the table above plus the counters (`lsn`, document counter, resource id counters), so ids and LSNs never repeat and
session tokens and change feed continuations from before the restart work. Cached query results are not stored: a continuation token from before
the restart finds no result and the query runs again and skips what was delivered (tested). Running scripts are not stored. A document is its JSON
with `_rid`, `_etag`, `_ts` as they were. The generation rises after every request that is not a GET, HEAD or OPTIONS (a query is a POST and raises it; the
save that follows writes nothing). A document that expired is removed when something reads it; after a restart it is gone anyway, because its time is absolute.

**Service Bus.** Stored: namespaces, queues, topics, subscriptions, rules (filters are stored as text and compiled at load), every setting, messages
in every state with sequence numbers, enqueue times, expiry, delivery counts, dead-letter reasons (they are message properties), deferred and scheduled
messages, the topic's scheduled messages, session state, the duplicate-detection history that is still inside its window, messages waiting to be forwarded.
Not stored: connections, links, message locks, session locks, transactions in flight (they roll back), receivers waiting. A message that was locked
when the state was copied comes back as when its lock runs out: one more delivery attempt counted, dead-lettered at the delivery limit
(`MaxDeliveryCountExceeded`). The messages are saved as their AMQP encoding, so a message comes back exactly as it was sent. With a data directory that
has Service Bus state the config file (`AZEMU_SERVICEBUS_CONFIG`) is not applied again (the saved state wins); with an empty directory it is.

**Event Hubs and Kafka.** One state, one set of files; the `kafka` service object keeps nothing. Stored: namespaces, hubs (partitions, retention,
consumer groups, the round-robin counter, hub ids), events with sequence numbers, offsets, enqueue times, partition keys and the message as sent, the
counters that make the next sequence number and offset, Kafka committed offsets per group, Kafka idempotent producer state per partition (epoch, next
sequence, the last five batches) and the next producer id. Retention is by absolute time and is applied by the first read after the restart. Not stored:
receivers, epochs, Kafka group members and generations: after a restart a Kafka group is empty and its consumers join again (a rebalance); a commit with the
old member id gets `UNKNOWN_MEMBER_ID`. A Kafka producer that reconnects keeps its producer id: a retry of its last batch is answered with the original offset
(tested), the next batch goes on, a gap is out-of-order. `InitProducerId` never gives an id twice.

**Event Grid.** Stored: topics, domains and their topics, namespaces and their topics, event sources (storage accounts and so on), subscriptions with
filters, destinations, retry policy and validation state; for pull subscriptions every event with its delivery count, **locks** (the token and the
end of the lock stay good after a restart: a client of the HTTP API is not tied to a connection), release delays, dead letters; the delivery log and
counters; push deliveries that were not finished, with the attempts made and when the first was made. Times are saved on the wall clock and come back as
moments of the monotonic clock (a moment older than the machine's uptime is as old as the clock allows). Unfinished push deliveries start again half a
second after the start (so every service has loaded: a delivery to a Service Bus queue needs the queue), with the attempt count they had; the wait
between two attempts is not kept, the next attempt is made at once. Not stored: tokens of locks that already ended (answered `BadToken` instead of
`TokenLost`).

**ARM.** Stored: subscriptions used, groups (location, tags, managed-by), scope tags, tag names, every resource (the JSON the client sees and the
provider's private data), deployments and their history (bounded as before). **Reconciling ARM and the data plane:** ARM and the data-plane services are
saved at the same call of the driver, one after the other, so a kill or a crash can leave them from slightly different moments. ARM does nothing about it
at load and keeps the rule of `arm.md`: ARM lists what ARM created, a GET of an ARM resource whose data-plane object is gone still answers from what ARM
stored, a data-plane object ARM does not know is not listed. A PUT through ARM makes the object again. Tested by saving ARM and Service Bus at different moments
(`arm_and_the_data_plane_saved_at_different_moments_still_agree_on_the_rule`). Known difference: a stored body that names the address of the run that made it
(`serviceBusEndpoint`, a custom topic's `endpoint`) still names that address after a restart on another port; run on the same port or make the resource again.

**Contract.** Nothing needed changing. Two requests for core: (1) `Service::load` cannot tell a service that the whole set of services has
loaded; Event Grid uses a half second delay for its resumed deliveries instead. (2) A helper in `persist.rs` that writes a value straight to the file (no
`Vec` of the whole encoding) would replace `crates/statefile`.

**Tests.** Rust, per crate (`tests/persist.rs`): a restart test per kind of state, the lock-hold test (a request during a save of a big state), a save with
nothing changed writes nothing and a change writes one file, a reset is saved, files of a deleted entity are removed, files are 0600, truncated and
flipped files (every byte position), another kind, a newer version, garbage, nesting 100,000 deep, an array that claims four billion entries, missing and
extra files, a file that holds another entity. Python: `tests/sdk-python/persistence/test_persist_{cosmos,servicebus,eventhubs_kafka,eventgrid,arm}.py` (SIGTERM, and SIGKILL after
`POST /_azemu/save`).

