# Table Storage (`table`)

**What it emulates.** The Azure Table Storage REST API (OData v3 JSON): tables, entities, queries with `$filter`, `$select` and `$top`, entity group
transactions (`$batch`), stored access policies, service properties, CORS and SAS. Crate `azemu-table`, standalone binary `azemu-table`.

**How to reach it.** `http(s)://{account}.table.core.localhost:4577/{table}` (any account name works without being created first). With `--standard-ports`:
`http://azemu:10002/{account}/{table}`, the Azurite address, and `UseDevelopmentStorage=true` works.

**Not built.** The Cosmos DB Table API (`*.table.cosmos.azure.com`), throttling, quotas on tables or entities, logging and metrics (the settings are stored,
nothing is logged or measured). A page is always full (Azure may return a short page). Signatures are verified only for `devstoreaccount1`; a SAS is
checked for expiry and permissions for every account.

| | |
|---|---|
| Service name | `table` |
| Gateway URL | `http://{account}.table.core.localhost:{port}/{table}(...)` (also `https://`, same port) |
| Pinned URL (Azurite style) | `http://127.0.0.1:{port}/{account}/{table}(...)` |
| Default account on a pinned port | `devstoreaccount1` |
| Accounts | any name works without being created first; state is kept per account |
| State | in memory, and on disk too with `--data-dir`; `POST /_azemu/reset` drops everything, `POST /_azemu/reset?account=<name>` one storage account (tables, entities, service properties) |

## Pointing a client at it

```
azemu --pin table=10002                        # all services, table also on 10002
azemu-table --port 4580 --pin table=10002      # only the table service
```

Azurite-style connection string (the well-known development account and key):

```
DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;TableEndpoint=http://127.0.0.1:10002/devstoreaccount1;
```

Gateway (host style), any account name:

```
DefaultEndpointsProtocol=http;AccountName=myaccount;AccountKey=<any base64>;TableEndpoint=http://myaccount.table.core.localhost:4577;
```

```python
from azure.data.tables import TableServiceClient
svc = TableServiceClient.from_connection_string(conn_str)
t = svc.create_table("people")
t.create_entity({"PartitionKey": "eu", "RowKey": "001", "name": "Ada"})
print(list(t.query_entities("name eq 'Ada'")))
```

With `UseDevelopmentStorage=true` the SDKs use Azurite's default ports, so pin the
table service to `10002` to make that work unchanged. HTTPS works on the same port;
trust the CA from `/_azemu/ca.pem`. A host ending in `-secondary` is the read-only
secondary endpoint (see below).

## Operations

| Operation | REST | Status |
|---|---|---|
| Query Tables (`$filter`, `$top`, `NextTableName`) | `GET /Tables` | built |
| Create Table (`Prefer: return-no-content` / `return-content`) | `POST /Tables` | built |
| Delete Table | `DELETE /Tables('{table}')` | built |
| Get Table | `GET /Tables('{table}')` | built |
| Get Table ACL | `GET /{table}?comp=acl` | built |
| Set Table ACL (max 5 policies) | `PUT /{table}?comp=acl` | built |
| Get Table Service Properties | `GET /?restype=service&comp=properties` | built |
| Set Table Service Properties | `PUT /?restype=service&comp=properties` | built |
| Get Table Service Stats | `GET /?restype=service&comp=stats` | built (always `live`) |
| Insert Entity (`Prefer` honoured) | `POST /{table}` | built |
| Query Entities (`$filter`, `$select`, `$top`, continuation) | `GET /{table}()` | built |
| Get Entity (`$select`) | `GET /{table}(PartitionKey='..',RowKey='..')` | built |
| Update Entity (replace) | `PUT` on the entity URL with `If-Match` | built |
| Merge Entity | `MERGE` or `PATCH` on the entity URL with `If-Match` | built |
| Insert Or Replace / Insert Or Merge | `PUT` / `MERGE` / `PATCH` without `If-Match` | built |
| Delete Entity (`If-Match` required) | `DELETE` on the entity URL | built |
| `X-HTTP-Method` tunnelling | `POST` on the entity URL with `X-HTTP-Method: PUT\|MERGE\|PATCH\|DELETE` | built |
| Entity group transaction | `POST /$batch` (`multipart/mixed`, one change set of up to 100 operations, or one query) | built |
| Preflight Table Request (CORS) | `OPTIONS` on any path | built: see "CORS" below |
| anything else (`HEAD`, other verbs and paths) | | 501 `NotImplemented`, body names the operation (`table: GET /?comp=list`) |

Payload formats `nometadata`, `minimalmetadata` and `fullmetadata` (`Accept:
application/json;odata=...`) are supported for tables, entities, queries and batch
parts. `DataServiceVersion` / `MaxDataServiceVersion` sent by the client are
answered with `DataServiceVersion: 3.0;` (or `1.0;` for an empty body).

## What is checked

- **Names and keys.** Table names `^[A-Za-z][A-Za-z0-9]{2,62}$`, unique ignoring case
  (the case it was created with is kept), `Tables` is reserved. Bad characters answer
  `InvalidResourceName`, bad length `OutOfRangeInput`. Keys: at most 512 characters
  (1 KiB of UTF-16), no `/ \ # ?` or control characters (`InvalidInput`); empty keys
  are allowed. Keys sort by UTF-16 code unit, so results come back in
  PartitionKey, RowKey order the way Azure orders them.
- **Entities.** Up to 252 custom properties (`TooManyProperties`), property names of
  at most 255 characters that start with a letter or `_` (`PropertyNameTooLong`,
  `PropertyNameInvalid`), strings up to 32K UTF-16 units and binary values up to 64 KiB
  (`PropertyValueTooLarge`), entities up to 1 MB (`EntityTooLarge`). `null` drops a
  property. `Timestamp` and `odata.*` in a payload are ignored. Property order is the
  order the client sent. Types: String, Int32, Int64, Double (including `NaN` and
  `Infinity`), Boolean, DateTime (100 ns ticks, year 1 to 9999, trailing zeros dropped
  on output), Guid, Binary.
- **Etags.** `W/"datetime'2022-10-27T22%3A06%3A34.6827829Z'"`, the entity's
  `Timestamp`. Every write moves it on, even when nothing changed. `If-Match: *` or an
  etag; a mismatch is `412 UpdateConditionNotSatisfied`; a missing entity is `404
  ResourceNotFound` (404 wins over a failed condition). Delete without `If-Match` is
  `400 MissingRequiredHeader`.
- **`$filter`.** A real parser and evaluator: `eq ne gt ge lt le`, `and` / `or` / `not`,
  parentheses, `'text'` with `''`, `123`, `123L`, `1.5`, `1e3`, `true` / `false`,
  `null`, `NaN`, `INF`, `datetime'..'`, `guid'..'`, `binary'..'` / `X'..'`, literals on
  either side, `Timestamp`, `PartitionKey`, `RowKey`. A syntax error is
  `400 InvalidInput` with `Syntax error at position N in '<filter>'.` Nesting is capped
  at 100 levels and 100 comparisons.
- **Paging.** At most 1000 entities (tables: 1000) per page; `$top` lowers it.
  `x-ms-continuation-NextPartitionKey` / `NextRowKey` (`NextTableName`) are sent only
  when more matching entities exist, and come back as query parameters. The token
  format is `1!<length>!<base64>`, like Azure's.
- **Batches.** One change set per batch (plus, optionally, a single query). All
  operations in the change set run or none do. At most 100; one table; one
  PartitionKey (`CommandsInBatchActOnDifferentPartitions`); every entity at most once
  (`InvalidDuplicateRow`). A failure returns one error part with the failing
  operation's index in front of the message (`2:The specified entity already exists.`);
  `ResourceNotFound` and `UpdateConditionNotSatisfied` carry no index, as recorded.
  The outer status is always 202.
- **Stored access policies** (`?comp=acl`): up to 5, unique ids, permissions from
  `r a u d`, times normalised to 7 fraction digits, an identifier may have no
  `AccessPolicy`.
- **Service properties.** Logging, hour and minute metrics and CORS rules are stored and
  returned, validated as Azure does (retention 1 to 365 days, at most 5 CORS rules,
  `IncludeAPIs` when metrics are enabled, known methods). Invalid XML is `400
  InvalidXmlDocument`. A section that is not in the request keeps its value.

## Authentication

- `Bearer` credentials are accepted without being verified. `SharedKey` and `SharedKeyLite`
  are verified for `devstoreaccount1` only (see below); for any other account any
  non-empty `Authorization` header counts. A request with no credentials
  answers `403 AuthenticationFailed`, or, when its `x-ms-version` is `2020-12-06` or
  later, `401 InvalidAuthenticationInfo` with the `WWW-Authenticate: Bearer authorization_uri=...`
  challenge (the SDKs' token policies rely on it).
- **SAS.** Honoured: expiry `se` and start `st` (`403 AuthorizationFailure`), protocol
  `spr=https`, and the signature (see below). *Account SAS*: services `ss` must include
  `t` (`AuthorizationServiceMismatch`), resource types `srt` (entity operations need
  `o`, Create/Delete Table `c`, Query Tables `s` or `c`, service properties `s`:
  `AuthorizationResourceTypeMismatch`), permissions `sp` (`r` query/get, `a` insert,
  `u` update/merge, `a`+`u` upsert, `d` delete, `l` or `r` query tables:
  `AuthorizationPermissionMismatch`). Access policies are not available to an account SAS.
  *Table SAS*: only entity operations; `tn` and the signed table must match the URL's
  table (case-insensitive); permissions `r a u d` (upsert needs `a`+`u`); the key range
  `spk`/`srk`/`epk`/`erk` limits point operations (`403 AuthorizationFailure`) and
  filters query results to the range; a stored policy `si` supplies permissions and
  times and must exist (`403 AuthorizationFailure` if not). Inside a batch each
  operation is checked against the SAS. `/$batch` with a table SAS needs `tn`.

## Known differences from Azure

- **SAS signatures are checked only for the development account** `devstoreaccount1`
  (its key is well known) and only when the signature looks real (32 or more
  characters), so a tampered signature or widened permissions are rejected with `403
  AuthenticationFailed`. For any other account name the key is derived by azemu and no
  client can know it, so its signatures are accepted; permissions, expiry, ranges and
  policies are still enforced. Placeholder signatures such as `Sanitized` (recorded
  traffic) pass. `sip` (client IP) is not checked.
- **Shared Key and Shared Key Lite are verified for `devstoreaccount1` only**, with the Table
  string to sign (Microsoft Learn, "Authorize with Shared Key", Table service): Shared Key is
  `VERB \n Content-MD5 \n Content-Type \n Date \n CanonicalizedResource` (no canonicalized
  headers; the `Date` line holds `x-ms-date` when the request has it, as the Python SDK signs it),
  Shared Key Lite is `Date \n CanonicalizedResource`, and the resource is `/account` + the URL path
  as sent (the account twice on a path-style URL) + `?comp=value` when the query has `comp`; no
  other query parameter is signed. A well-formed signature (base64 of 32 bytes) that matches none
  is `403 AuthenticationFailed`, with Azure's message and no string to sign. As in Blob and Queue,
  other accounts, placeholder signatures (under 20 characters, such as `Sanitized`) and
  `AZEMU_LOOSE_AUTH=1` (read once at start; it also turns the SAS signature check off) are not
  checked. The code is `azemu_core::storage::sig::verify_shared_key_table`. An invalid or expired
  bearer token is not detected.
- A comparison against a property the entity lacks, or has with another type, is false
  for every operator including `ne` (so `not (ghost eq 'x')` matches). `$select` of a
  property the entity lacks leaves it out. Numbers compare across Int32, Int64 and
  Double. Azure documents a limit of 15 comparisons per filter; azemu allows 100.
- Pages are always full: Azure may return a short page (with a continuation token) at a
  partition boundary or after a time limit.
- The `-secondary` endpoint refuses writes (`403 InsufficientAccountPermissions`,
  `Write operations are not allowed.`) but reads the same data, with no replication lag.
  Statistics answer on any host and always report `live`.
- Logging and metrics settings are stored but nothing is logged or measured.
- **CORS** follows the rules set with Set Table Service Properties
  (`azemu_core::storage::cors`, shared with Queue): an `OPTIONS` request needs no credentials and
  is judged against the service, not the table. Rules are tried in order; the first whose origin
  (case-sensitive, `*` or a pattern like `http://*.contoso.com`) and method match is the only one that
  counts, and every header in `Access-Control-Request-Headers` must be in its `AllowedHeaders` (names
  without case, a trailing `*` is a prefix). A match is `200` with `Access-Control-Allow-Origin` (the
  origin, or `*` when the rule says `*`), `-Allow-Methods`, `-Allow-Headers`, `-Max-Age`, and for an
  exact origin `-Allow-Credentials: true` and `Vary: Origin`. No match, or no rules, is `403
  CorsPreflightFailure` (XML body); no `Origin` or no `Access-Control-Request-Method` is `400
  MissingRequiredHeader`. An actual request with `Origin` gets `Access-Control-Allow-Origin` and
  `Access-Control-Expose-Headers` from the matching rule (also on errors); a GET/HEAD without a match
  gets `Vary: Origin`. Not copied from a recording of real Azure (there is none); the error body form is a guess.
- Table names are unique per account; there is no limit on the number of tables or
  entities, no request size limit beyond core's, and no throttling.
- The service answers any `x-ms-version`; it is echoed back.
- JSON answers use `Content-Type: application/json; odata=...; streaming=true; charset=utf-8`
  (a space after each semicolon, as in 1,752 recorded answers) and escape non-ASCII characters as `\uXXXX`, as Azure does. The parts
  inside a `$batch` answer have the form without spaces (233 recorded parts). With `$select`, `odata.metadata` ends in `&$select=<the select>`
  (spaces as `%20`).
- Choices not backed by a recording, made from the documentation: Delete Entity without
  `If-Match` is `400 MissingRequiredHeader`; a batch with operations on different tables
  is `400 InvalidInput`; the 401 / 403 split for missing credentials follows the two
  recorded API versions (`2020-12-06` and `2019-07-07`).
- Cosmos DB Table API (`*.table.cosmos.azure.com`) behaviour is not emulated (it allows
  underscores in table names, empty keys, other error codes).

## Keeping data between runs (`--data-dir`)

`<data-dir>/table/table.state` (kind `table`, version 1; `crates/services/table/src/persist.rs`): for each account its tables (name as created, access
policies, every entity with its typed properties and its timestamp, from which the ETag is derived) and the service properties. Entities are written in
key order and put back in the ordered map. Reads are not changes; every other request is. The file is written only when its bytes differ from the last save.

Version 1 of `table.state` is checked in under `crates/services/table/tests/state-v1/table/` and loaded by `tests/state_v1.rs`. A request no route matches is `501` and is named `Table_Unknown (<METHOD> <Service|Tables|TableItem|Batch|Table|Query|Entity|NestedPath|BadKeys>)` in `GET /_azemu/unsupported`.
