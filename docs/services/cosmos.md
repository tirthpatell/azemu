# Cosmos DB (NoSQL API)

**What it emulates.** The gateway-mode REST API of Azure Cosmos DB for NoSQL: databases, containers, items, patch, transactional batch and bulk, queries in
the SQL dialect (`docs/services/cosmos-sql.md`), change feed (latest version and "all versions and deletes"), TTL, unique keys, indexing policy,
stored procedures, triggers and user-defined functions (they run in a separate worker process: `docs/services/cosmos-scripts.md`), users and
permissions, offers (stored, not enforced).

**How to reach it.** `https://{account}.documents.localhost:4577/` (also `http://`; every account name is a separate account made on first use). With
`--pin cosmos=8081` or `--standard-ports`: `https://localhost:8081/`, the address of Microsoft's emulator, and `https://azemu:8081/` from a container.
Any key works; a request with no `Authorization` header gets 401. Python: `connection_verify=False` (see the table below for why a CA path alone is ignored).

**Not built.** Direct mode (a client in direct mode gets 501 for `GET //addresses/`, and the message says to use gateway mode), binary-encoded item
bodies (the Rust SDK has a switch), attachments, conflicts, spatial / vector / full text search (501), triggers on patch, batch and bulk, resource
tokens as credentials. Behaviour that is not modelled: one physical partition, no splits, no 429 or request units, one region, no consistency levels
(reads always see the latest write), no analytical store, no backups. Request charges are made up.

Crate `azemu-cosmos` (`crates/services/cosmos`), binary `azemu-cosmos`, in front of the query engine `azemu-cosmos-sql`.

**There are no real-Azure recordings for Cosmos DB.** The outside checks are the SDK test suites (six languages; the Python suite was also run
against Microsoft's vnext emulator and corrected where a test was wrong) and Microsoft's REST reference. Everything not covered by those is
"as documented", not verified against a real account.

## Using it

| | |
|---|---|
| Endpoint (gateway) | `https://{account}.documents.localhost:4577/` (also `http://`; every account name is a separate account created on first use) |
| Endpoint (drop-in) | `azemu --pin cosmos=8081` (or `azemu-cosmos --pin cosmos=8081`): `https://localhost:8081/`, account `localhost` |
| Key | any key; the well-known emulator key `C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==` is what people use. Signatures are not checked. A request without an `Authorization` header gets 401 |
| TLS | the local CA is `~/.azemu/ca.pem` (`GET /_azemu/ca.pem`). **Python SDK:** `connection_verify="<ca.pem>"` alone is ignored by `azure-cosmos` 4.17.1 (it only reads a CA path from `ssl_config`); use `connection_verify=False`, or set `REQUESTS_CA_BUNDLE` (sync) / `SSL_CERT_FILE` (aiohttp, async) to the CA file. .NET / Java / Node: trust the CA or disable verification (`DangerousAcceptAnyServerCertificateValidator`, `NODE_TLS_REJECT_UNAUTHORIZED=0`). A plain `http://` endpoint needs nothing |
| Connection mode | gateway only. The account document's locations point back at the host the client used. **Direct mode is not built and will not be**: a client in direct mode asks `GET //addresses/` and gets `501 NotImplemented` whose message says "use gateway mode" (`ConnectionMode.Gateway` in the .NET SDK, `gatewayMode()` in the Java SDK, `"connectionMode": "Gateway"` in the `host.json` of an Azure Functions app; the Python, JavaScript and Go SDKs only use the gateway). `GET /_azemu/unsupported` lists it as `GET /addresses (direct mode)`. The Functions Cosmos DB extension 4.x defaults to Gateway, so an app that does not set the mode works |

```python
client = CosmosClient("https://myacct.documents.localhost:4577/", credential=KEY, connection_verify=False)
```

## Operations

| Operation | State |
|---|---|
| `GET /` database account (locations, consistency, `queryEngineConfiguration`) | built |
| Databases: create, read, delete, list, query | built (no replace: Cosmos has none) |
| Containers: create, read, replace, delete, list, query | built |
| Container policies: partition key (Hash v1/v2, MultiHash up to 3 paths), indexing policy (defaults filled in), unique keys (enforced), default TTL, conflict resolution, computed properties (stored); enum strings (`indexingMode`, conflict `mode`, partition key `kind`, composite index `order`) in any case, shown in the documented spelling | built |
| Documents: create, upsert, read, replace, delete, list | built |
| Patch (add, set, replace, remove, incr, move, `condition`, max 10 operations; the body is `{"operations": [...], "condition": ...}` or, as the JavaScript SDK sends it without a condition, the bare array) | built |
| `If-Match` / `If-None-Match` (412 / 304), `Prefer: return=minimal`, 2 MB limit (413), id validation, partition key header checks (400, sub-status 1001) | built |
| Partition key `[{}]` (undefined), null, bool, number, string, hierarchical keys, key prefixes | built |
| TTL (container default, per-item `ttl`, lazy, computed from the request time) | built |
| Queries on documents (single partition, cross partition, query plan, per-range rewritten queries, paging) | built |
| Queries on databases, containers, offers, users, permissions, scripts | built |
| Transactional batch (atomic, one partition key from the header, 100 operations, 207 with 424 for the others). JSON body (Python, JavaScript, Java) or, from the .NET SDK, a binary HybridRow body answered in HybridRow (`hybridrow.rs`; see "Design notes") | built |
| Bulk batch (`x-ms-cosmos-batch-atomic: false`, what the JavaScript, Java and .NET bulk executors send; the .NET SDK sends the HybridRow form, not JSON): each operation has its own `partitionKey` (a string holding a JSON array), runs alone and answers with its own `statusCode`, `requestCharge`, `eTag`, `resourceBody`; 200 when all worked, 207 otherwise; `x-ms-cosmos-batch-continue-on-error: false` stops at the first failure (424 for the rest); the partition key range id header is not checked | built |
| Change feed (latest version): start from beginning / now / time, continuation, partition key, feed range, range id | built |
| Change feed "all versions and deletes" (`A-IM: Full-Fidelity Feed`): every create, replace and delete, TTL expiry; start from now or a continuation; partition key, feed range, range id | built (no previous image: Azure has none; see "Design notes") |
| Delete all items by partition key (`POST .../operations/partitionkeydelete`) | built |
| `/pkranges` (one range, `ETag` / 304) | built |
| Offers: read, query, replace (manual and autoscale), created with databases and containers | built |
| Stored procedures, triggers, UDFs: create, upsert, read, replace, delete, list, query | built |
| Execute a stored procedure (`POST .../sprocs/{id}`, parameters as a JSON array, partition key header required, `x-ms-documentdb-script-enable-logging`) | built: JavaScript on `boa_engine`, server-side API with `getContext()`, the collection calls, `__` and the chained query API; atomic, bounded (5 s, loops, recursion, memory, response 2 MB). 400 for an exception, 408 time-out, 449 when the partition kept changing, 429 when too many scripts run. See `cosmos-scripts.md` |
| Pre- and post-triggers named in `x-ms-documentdb-pre-trigger-include` / `x-ms-documentdb-post-trigger-include` on create, upsert, replace, delete | built: checked against `triggerType` and `triggerOperation` (400), run in the same transaction as the operation, a throw undoes everything. Not on patch, batch and bulk |
| UDF called from a query (`udf.name(..)`) in SELECT, WHERE, ORDER BY, GROUP BY | built: one engine context per query, compiled once, called per row, inside the 5 s limit; an unknown function is 400 |
| Users, permissions: create, upsert, read, replace (rename), delete, list, query; `_token` is made up | built (resource tokens are not accepted as credentials) |
| Attachments, conflicts (empty feed), media, spatial / vector / full text search | 501, or an empty feed for conflicts |
| Anything else | 501 `{"code":"NotImplemented","message":"azemu cosmos: <METHOD> <path> is not implemented"}` |

## Design notes

* **State**: one struct behind one lock, keyed by account; `reset()` drops it. Documents live per container
  in a map ordered by (partition key value, id), with a `_rid` index and an LSN-ordered index for the change feed.
  Documents are `Arc<Value>`. What runs under the lock is the work on the stored state only: the JSON body is parsed
  before the lock (`Cx::preparse`), a document query takes a snapshot (pointer copies) and runs after the lock is
  released on `spawn_blocking` (`doc_query::QueryJob`), and responses are serialised after it (the request charge
  of a document is computed from the serialised size there). A long query does not delay point reads or writes.
  Still under the lock: SQL text parsing (at most 512 KB), TTL purge, unique key checks, and building the page of a
  read feed. A read-write lock or a lock per container was not done: reads here write (TTL purge, accounts created on
  first use) and every handler takes `&mut Container`, so it means rewriting `router.rs` and most handlers.
* **Input limits** (`limits.rs`; Microsoft Learn, "Service quotas and default limits"): a document may nest objects
  and arrays 127 levels (Azure says 128; the JSON parser stops at 127), checked on every write and after every
  patch operation, 400; request body 2 MB, 413 (before the body is parsed); SQL text 512 KB, 400; patch 10 operations;
  the SQL parser limits nesting (`cosmos-sql.md`, "Limits"). Nothing recurses on input without a bound.
* **Scripts** (`docs/services/cosmos-scripts.md`): a stored procedure, or a document write that names triggers, is not run under the service lock. Under the lock the
  logical partition it is scoped to is copied (documents are shared `Arc`s; `Container::snapshot`); the script runs in a **separate worker process** (the JavaScript never runs in the emulator; a blocking thread of the emulator answers the worker's collection calls from the copy, through the ordinary document
  handlers); under the lock again the writes are applied in one step if the partition is unchanged since the copy (`apply_snapshot`), otherwise the script runs again (5 tries, then 449).
  So a script is atomic and isolated, and holds nothing while it runs: other containers and other requests on the same container go on. A script's resource ids come from
  counters set aside in the container when the copy is made, so they stay unique. UDFs in queries run inside `QueryJob`, one worker process per query. The emulator kills a worker at the time limit, when the request is cancelled or the emulator shuts down; the worker limits its own memory and processor time (Linux). See "The worker process" and "Bounds" in `cosmos-scripts.md` for what is guaranteed on each platform.
* **Resource ids**: `_rid` values have the real format (base64 with `-` for `/`; 4 bytes for a database, 8 for a container with the
  high bit of byte 4 set, 8 for a user with it clear, 16 for a document, stored procedure, trigger, UDF and partition key range with a
  type in the high four bits of byte 15, 16 for a permission, 3 for an offer; `rid.rs`), because the Java SDK parses every `_rid`
  (`ResourceId.tryParse`). `_self` is `dbs/{rid}/colls/{rid}/docs/{rid}/`. Every path accepts names or rids.
* **Owner headers** (Java SDK): every answer below `dbs/{db}` carries `x-ms-content-path` = the owner's bare rid and
  `x-ms-alt-content-path` = the owner's name path (container: `dbs/{db}`; document, script, partition key range: `dbs/{db}/colls/{c}`;
  permission: the user; a container delete: the container's own rid). Read from the SDK's `SessionContainerUtil`; no recording.
* **One physical partition per container**: `/pkranges` returns range `"0"`, `""` to `"FF"`. Effective partition
  keys (MurmurHash3 as the SDKs compute them: Hash v1, v2 and MultiHash) are computed per document and used only
  to answer EPK-range requests (feed ranges, hierarchical key prefixes, change feed ranges).
* **Cross-partition queries**: a query pinned to one logical partition (partition key header, or a `WHERE`
  that pins the key) runs as written. Without a key the request must carry
  `x-ms-documentdb-query-enablecrosspartition` (else 400, as on the real service). A query with ORDER BY,
  aggregates, GROUP BY, DISTINCT, TOP or OFFSET/LIMIT is then answered with 400, sub-status 1004 and the plan in
  `additionalErrorInfo`; the SDK fetches the plan (`x-ms-cosmos-is-query-plan-request`: `queryInfo` from the
  engine, one query range) and sends the rewritten query to range `"0"` (`x-ms-documentdb-partitionkeyrangeid`),
  which runs as written, and merges client side. MIN and MAX are answered as `{"min", "count"}` / `{"max", "count"}`
  to a client that sends query version 1.4 or higher with `CompositeAggregate` (the JavaScript SDK) and as the bare value to
  the others (the Python SDK); see `cosmos-sql.md`. Simpler queries run directly. This makes the Python SDK (which
  tries the query first) and the SDKs that always ask for a plan (.NET, Java) work, and keeps real behaviour such as
  the client-side "cross partition aggregates need VALUE" error.
* **Paging**: `x-ms-max-item-count` (default 100 for queries); a response holds at most 4 MB of rows. The continuation
  token is a JSON object (`{"token":"+RID:~azemu#SKIP:n#Q:id","range":{...}}`). A query that has more pages is run once
  and its result is kept behind `#Q:id` (at most 256 results and 128 MB estimated, 10 minutes unused, least recently
  used out first, result over 64 MB not kept; the request must match: path, query, parameters, partition headers). Later
  pages are slices of it, so they show the data as of the first page. A token whose result is gone (expired, evicted,
  restart, other service) runs the query again and skips `n` rows, as before. The plain document feed (`GET .../docs`)
  keeps nothing: its token carries the key of the last document and the next page starts behind it, so documents added or
  deleted between pages never shift it. Old tokens with only `#SKIP:n` still work. The engine still builds the whole result
  of a query once (ORDER BY, DISTINCT and aggregates need it).
* **ORDER BY and documents without the sort field**: left out when the container has the default indexing policy and
  one ORDER BY field; returned (undefined first ascending, last descending) when the field has its own included path
  (`/field/?`) or when several fields are sorted (they need a composite index). Source: Microsoft Learn, "Indexing
  policies": "If an explicitly indexed path doesn't exist in an item, a value is added to the index to indicate that the
  path is undefined" (the same for composite indexes), and the Cosmos DB team in dotnet/efcore#13131: missing properties are
  returned "if sorting was done over a composite index". Excluded index paths are not modelled (Azure fails such an ORDER BY).
* **Order by several fields** needs a matching composite index, otherwise 400, as on the real service.
* **Change feed answers** carry `x-ms-item-count` (0 on a 304), `etag`, request charge, activity id, session token and `lsn`;
  a page is at most 4 MB whatever `x-ms-max-item-count` says (always at least one document); an `If-Modified-Since` that cannot be
  read, or an `If-None-Match` position ahead of the container (another container's token, or one from before a restart; in the
  all versions mode only while the history runs), is a 400 and not a silent restart.
* **Change feed**: per-container LSN; the `etag` header is the continuation (`"<lsn>"`); 304 when nothing is newer;
  latest version mode (`A-IM: Incremental Feed`): documents come back in last-modification order with `_lsn`; deletes
  and intermediate versions are not reported; `If-None-Match: *` starts from now, `If-Modified-Since` from a time
  (matched against `_ts`).
  All versions and deletes mode (`A-IM: Full-Fidelity Feed`, with `x-ms-cosmos-changefeed-wire-format-version`):
  history is kept per container from the first read of that container in this mode (so unused containers pay nothing),
  bounded to 100,000 changes, 128 MB estimated and 7 days. Starts from now (`If-None-Match: *`, 304 with the position)
  or from a continuation; a read with neither is 400 (Learn: no start from the beginning or a time); a continuation older
  than the retained history is 400; one from before the history began (after a restart) means "now". Items:
  `{"current": item+_lsn, "metadata": {"operationType": "create"|"replace", "lsn", "crts", ["previousImageLSN"]}}`;
  a delete (also a TTL expiry, written when the document is purged) has no `current` and metadata
  `{"operationType": "delete", "lsn", "crts", "previousImageLSN", "timeToLiveExpired", "id", "partitionKey": {"<path>": value}}`
  (Learn, "Change feed modes"). Learn says there is no way to get the previous version, so there is no `previous`. An undone
  batch leaves no entries. Not modelled: the account setting and continuous backup that Azure needs; every change has its
  own LSN (Azure gives one per transaction); deletes by partition key appear as ordinary deletes.
* **HybridRow batches** (`hybridrow.rs`): the .NET SDK sends every transactional batch and every item
  call of a client with `AllowBulkExecution = true` as a binary HybridRow record stream (no content type, first byte `0x81`) and reads a
  HybridRow stream back). Only the two
  fixed schemas of the batch API (`BatchOperation`, `BatchResult`) are read and written; the layout is in the module comment and
  was checked byte for byte against the real library. `Cx::preparse` turns the body into the JSON operations the batch code already
  runs (outside the lock), and the answer is encoded the same way. A damaged body is a 400 that says so (record checksum, lengths, schema).
  `timeToLiveInSeconds`, `indexingDirective`, `effectivePartitionKey` and `binaryId` of an operation are read and ignored;
  `minimalReturnPreference` leaves the body out of a write result. The answer's content type is `application/octet-stream`
  (the SDK looks at the first byte); not checked against Azure.
* **Query memory**: documents are shared (`Arc`) from the snapshot to the
  result: the engine reads property paths straight from the stored JSON and `SELECT *` / `SELECT VALUE c` / filters return the same
  allocation; only the page that is sent is copied. FROM / JOIN rows are streamed (the product of a join is never held), aggregates
  are running totals, TOP / LIMIT stop the scan, and what a query keeps (ORDER BY rows, DISTINCT keys, groups, the result) is counted:
  over 64 MiB (`limits::QUERY_MEMORY_BYTES`) the answer is a 400 that says so (not 429: the SDKs retry 429, and this limit is fixed).
  At most 4 queries run at once (`limits::MAX_RUNNING_QUERIES`); the others wait on the async side without holding a thread.
  A result of more than about 900,000 scalar rows (or the same in bytes) therefore cannot be returned by one query; a client that
  needs it pages with `OFFSET`/`LIMIT` or a `WHERE` on `_ts` / `id`.
* **Session tokens**: `x-ms-session-token: 0:1#<lsn>` on container operations, parseable by the SDK. Tokens sent by
  clients are ignored (no 404/1002 session errors).
* **Request charge**: plausible numbers from document and result sizes; no throttling (never 429).
* **Errors**: `{"code":"NotFound","message":"Message: {\"Errors\":[\"...\"]}\r\nActivityId: ..."}` with the real
  codes per status and `x-ms-substatus`. Query errors embed the engine's `{"errors":[...]}` document.

## Known differences from Azure Cosmos DB

- **Binary JSON bodies are refused.** The Rust SDK sends item bodies as Cosmos binary JSON by default (first byte `0x80`). The emulator reads
  text JSON only and answers 400 `BadRequest` with a message that names `AZURE_COSMOS_BINARY_ENCODING_ENABLED=false` (Microsoft's Linux emulator
  has the same limit). Answers are always text JSON. HTTP/2 is served (the Rust SDK asks for it first).
- **Item ids and partition key values**. An item id may hold every character except `/` and `\\` (so `USER#123` and `a?b`
  are fine), at most 1,023 bytes; databases, containers, users, permissions and scripts keep the stricter rule (255 characters, and no `?` or `#`
  either). A partition key value (each component of a hierarchical key) is at most 2,048 bytes ("Service quotas and default limits", per-item limits:
  "2,048 bytes (101 bytes if large partition-key isn't enabled)"). The emulator gives every container the larger limit: a key of 102 to 2,048 bytes is
  accepted in a container made with partition key version 1, where Azure refuses it. An id with `#` or `?` goes into a URL percent-encoded; the
  Python, JavaScript (`#`) and Java SDKs refuse such ids themselves, the Go SDK does not (`tests/sdk-go/cosmos/item_ids_test.go`).
- Only a stored procedure run (`POST .../sprocs/{id}`) is dropped when its client disconnects (`Service::abort_on_disconnect`), which ends its worker at
  once. A query with a user-defined function and a write with a trigger cannot be told from a plain one by method and path, so their workers end at the
  time limit. `POST /_azemu/reset?service=cosmos&account=<name>` drops one account (its scripts' workers end, kept query results go).
- A request body over 2 MiB is refused by core with 413 `RequestEntityTooLarge` before it is read (`Service::max_body_bytes`).

* One physical partition, no splits, no 410 partition gone (except for an unknown range id), no 429, no
  geo-replication (one region named `Local`), no multi-write, no analytical store, no backups, no conflicts feed.
* Stored procedures, triggers and UDFs run on Boa, a pure-Rust JavaScript engine (ECMAScript 2015+ with promises and async functions; no `Intl`, `eval` or `Function`), not on Azure's engine, and in a separate worker process (the same executable started with the hidden first argument `__cosmos-script-worker`; programs that mount this service call `azemu_cosmos::script_worker_main_if_requested()` first in `main`). Error texts have no stack lines; a script that runs past 5 s (even one huge native call such as a catastrophic regular expression) is killed with its worker and answers 408; memory is limited by the operating system on Linux only (not on macOS or Windows). Each script run costs a process start (about 27 ms in a debug build). The full list is in `cosmos-scripts.md` ("Bounds", "What the bounds do not cover", "Left out").
* Authentication is not checked: any key (or resource token) is accepted; a missing `Authorization` header is 401.
* Server-side session consistency is not modelled. Reads always see the latest write.
* Throughput is only stored: no RU accounting, no autoscale scaling. Containers without throughput in a database
  without shared throughput get a default 400 RU/s offer (like a provisioned account).
* Query behaviour is that of `azemu-cosmos-sql` (see its "Behaviour that could not be confirmed against Azure" section).
* Request charge, query metrics, index metrics are made up. `x-ms-resource-quota` / usage are constants.

## Differences from Microsoft's vnext emulator

The service enforces what the tests marked `not_on_vnext` check: autoscale offers and their validation, `defaultTtl`
and `ttl` validation, replace with a different partition key / unique key policy (400), hierarchical key arity,
`If-Match: *`, number `5` and string `"5"` being different partitions, the cross-partition flag, the composite index
requirement, non-`VALUE` cross-partition aggregates (client-side 400), change feed from now and by partition key,
`ORDER BY` paging with `max_item_count`, WHERE in metadata queries, canonical-case `triggerType` /
`permissionMode`, an empty batch and 101 operations being 400. These are mostly assumptions from the REST
documentation and Microsoft's SDK tests, not recordings.

## Settings

Read through `azemu_core::settings` and listed in `docs/SETTINGS.md`: `AZEMU_COSMOS_SCRIPT_TIMEOUT_MS`, `_MAX_OPERATIONS`, `_LOOP_ITERATIONS`,
`_MEMORY_MB`, `_MAX_RUNNING` and `AZEMU_SCRIPT_WORKER`. They are read once, when the service is made; the worker process gets its limits from its own
command line and reads no setting. The query limits (4 queries at a time, 64 MB each, 512 KB of text) are constants.

## Keeping data between runs

With `--data-dir` the service saves `cosmos.state` (accounts, databases, containers with every policy, users, permissions, stored procedures, triggers,
UDFs, throughput, id counters) and one `docs-<hash>.state` per container (documents with `_rid`, `_etag`, `_ts` as they were, the container's LSN and
document counter, and the "all versions and deletes" history once it was started). A save writes only the containers that changed. The files of version 1 stay
readable: `tests/state-v1/cosmos/` and `tests/state_v1.rs` fail when a change breaks the reading (raise `VERSION` and keep a reader then). Session tokens,
change feed continuations and ids from before a restart keep working; a query continuation token finds no kept result and the query runs again. Running
scripts are not stored. Details, costs and tests: `docs/services/persistence.md`, section "Cosmos, Event Grid, Service Bus, Event Hubs with Kafka, ARM".
