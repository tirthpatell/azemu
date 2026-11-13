# Queue Storage (`queue`)

**What it emulates.** The Azure Queue Storage REST API: queues, messages (put, get, peek, update, delete, clear, visibility timeouts, expiry, dequeue count),
service properties (logging, metrics, CORS), stored access policies and SAS. Crate `azemu-queue`, standalone binary `azemu-queue`.

**How to reach it.** `http(s)://{account}.queue.core.localhost:4577/{queue}` (any account name works without being created first). With
`--standard-ports`: `http://azemu:10001/{account}/{queue}`, the Azurite address, and `UseDevelopmentStorage=true` works. A queue must be created before it is used;
nothing creates a queue by naming it.

**Not built.** Nothing answers 501 (an unknown verb is `405 UnsupportedHttpVerb`). No quotas on queues, messages or bytes, `timeout` is ignored, Entra audience
checks do not exist. Signatures are verified only for `devstoreaccount1`; a SAS is checked for expiry and permissions for every account.

| | |
|---|---|
| Service name | `queue` |
| Gateway URL | `http://{account}.queue.core.localhost:{port}/{queue}` (also `https://`, same port) |
| Pinned URL (Azurite style) | `http://127.0.0.1:{port}/{account}/{queue}` |
| Default account on a pinned port | `devstoreaccount1` |
| Accounts | any name works without being created first; state is kept per account |
| State | in memory, and on disk too with `--data-dir`; `POST /_azemu/reset` drops everything, `POST /_azemu/reset?account=<name>` one storage account (queues, messages, service properties) |

## Pointing a client at it

Start it with the full emulator or on its own, with a pinned port for Azurite-style
URLs:

```
azemu --pin queue=10001                       # all services, queue also on 10001
azemu-queue --port 4580 --pin queue=10001     # only the queue service
```

Azurite-style connection string (the well-known development account and key):

```
DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;QueueEndpoint=http://127.0.0.1:10001/devstoreaccount1;
```

Gateway (host style), any account name:

```
DefaultEndpointsProtocol=http;AccountName=myaccount;AccountKey=<any base64>;QueueEndpoint=http://myaccount.queue.core.localhost:4577;
```

```python
from azure.storage.queue import QueueServiceClient
svc = QueueServiceClient.from_connection_string(conn_str)
q = svc.create_queue("jobs")
q.send_message("hello")
print(q.receive_message().content)
```

With `UseDevelopmentStorage=true` the SDKs use Azurite's default ports, so pin the
queue service to `10001` (`--pin queue=10001`) to make that work unchanged. HTTPS
works on the same port; trust the CA from `/_azemu/ca.pem`.

## Operations

| Operation | REST | Status |
|---|---|---|
| List Queues (`prefix`, `marker`, `maxresults`, `include=metadata`) | `GET /?comp=list` | built |
| Get Queue Service Properties | `GET /?restype=service&comp=properties` | built |
| Set Queue Service Properties | `PUT /?restype=service&comp=properties` | built |
| Get Queue Service Stats | `GET /?restype=service&comp=stats` | built |
| Get User Delegation Key | `POST /?restype=service&comp=userdelegationkey` | built (key is not verifiable, see below) |
| Create Queue | `PUT /{queue}` | built |
| Delete Queue | `DELETE /{queue}` | built |
| Get Queue Metadata | `GET`/`HEAD /{queue}?comp=metadata` | built |
| Set Queue Metadata | `PUT /{queue}?comp=metadata` | built |
| Get Queue ACL | `GET`/`HEAD /{queue}?comp=acl` | built |
| Set Queue ACL | `PUT /{queue}?comp=acl` | built |
| Put Message | `POST /{queue}/messages` | built |
| Get Messages | `GET /{queue}/messages` | built |
| Peek Messages | `GET /{queue}/messages?peekonly=true` | built |
| Clear Messages | `DELETE /{queue}/messages` | built |
| Update Message | `PUT /{queue}/messages/{id}` | built |
| Delete Message | `DELETE /{queue}/messages/{id}` | built |
| Preflight Queue Request (CORS) | `OPTIONS` on any path | built: see "CORS" below |

A real resource with a verb Azure does not support answers `405 UnsupportedHttpVerb`;
a URL with too many segments `400 InvalidUri`; an unknown `comp` `400
InvalidQueryParameterValue`.

### Behaviour that is modelled

- **Create Queue**: `201`; the same name with the same metadata again `204`; the same
  name with different metadata `409 QueueAlreadyExists`. Note that
  `azure-storage-queue` raises `ResourceExistsError` for the `204` by itself.
- **Queue names**: 3 to 63 characters, lower-case letters, digits and single hyphens,
  first and last character a letter or digit. Bad characters `400 InvalidResourceName`,
  bad length `400 OutOfRangeInput`. Checked on every queue operation.
- **Metadata**: `x-ms-meta-*` headers, names must be C# identifiers
  (`400 InvalidMetadata`), at most 8 KiB (`400 MetadataTooLarge`).
- **Messages**: FIFO among visible messages. Put returns a pop receipt; Get hides the
  message for `visibilitytimeout` (default 30 s, 1 to 604800), increases
  `DequeueCount` and issues a new receipt; Peek changes nothing and has no receipt or
  `TimeNextVisible`; Update and Delete need the current receipt
  (`400 PopReceiptMismatch`; unknown or expired id `404 MessageNotFound`). Update
  returns `x-ms-popreceipt` and `x-ms-time-next-visible`, keeps the dequeue count and
  replaces the text only when a body is sent.
- **Time to live**: default 7 days, `-1` never (`Fri, 31 Dec 9999 23:59:59 GMT`), any
  positive number up to year 9999. Expired messages are never returned or counted.
  Visibility and expiry are computed from the request time when a message is touched;
  there are no timers.
- **Limits**: `numofmessages` 1 to 32, `visibilitytimeout` 0 to 604800 (Put, Update) or
  1 to 604800 (Get), out of range `400 OutOfRangeQueryParameterValue` with the
  parameter name and allowed range. The message text is limited to 64 KiB (bytes of
  UTF-8, after XML decoding): more is `413 RequestBodyTooLarge` with `MaxLimit`.
- **XML**: a body that is not well-formed, or that holds a character XML 1.0 does not
  allow (`\u0001`), is `400 InvalidXmlDocument` with `LineNumber`, `LinePosition` and
  `Reason` as Azure sends them.
- **Service properties**: Logging, HourMetrics, MinuteMetrics and CORS are stored per
  account. A body replaces the sections it contains and keeps the others; `<Cors />`
  clears the rules. More than 5 CORS rules, a bad method, retention `Days` outside
  1 to 365 (also when retention is on and `Days` is missing) and an empty
  `<StorageServiceProperties />` are `400 InvalidXmlDocument`. A fresh account reports
  what a new real account reports (hour metrics on, 7 days).
- **Access policies**: up to 5 signed identifiers (more is `400 InvalidXmlDocument`);
  times come back with seven fraction digits.
- **List Queues**: ordered by name, `NextMarker` is `/{account}/{queue}`; `maxresults`
  is capped at 5000; `<Metadata />` appears for every queue with `include=metadata`.
  `ServiceEndpoint` is the URL the client used (host style or path style).

## Authentication

**Signature checks.** For the development account `devstoreaccount1` (the only account
whose key a client can know) the signature of a Shared Key / Shared Key Lite request and of a SAS is verified: `SharedKey` and `SharedKeyLite` headers, account SAS, queue SAS and user delegation SAS. A well-formed
signature (base64 of 32 bytes) that does not match is `403 AuthenticationFailed` with an
`AuthenticationErrorDetail` ("The MAC signature found in the HTTP request ... is not the same as
any computed signature. Server used following string to sign: ..." for Shared Key, "Signature did
not match. String to sign used was ..." for a SAS). Because the signed resource is part of the
signature, a SAS for one queue does not open another. Not checked, by design:

- any other account name (the key is invented by azemu, no client can know it);
- a signature that is plainly a placeholder: not base64 of 32 bytes (the replay tool sends
  `SharedKey devstoreaccount1:replay`, recordings carry `sig=Sanitized`);
- a SAS whose layout sources disagree on (user delegation SAS before 2020-02-10, signed request
  headers / query parameters `srh` / `srq`): the layouts are tried and, if none matches, the request
  is accepted and logged at debug level. A false rejection of a legitimate client is the worst
  outcome, so doubt means accept. (A SAS with an `sv` newer than `2026-12-06`, the Go SDK's, is
  checked with the newest layout and refused when it does not match; the error detail says that the
  version is newer than the layouts azemu knows and names `AZEMU_LOOSE_AUTH=1`, in case a newer SDK
  changed the layout.);
- `AZEMU_LOOSE_AUTH=1` in the environment (read once at start) turns every signature check off.

Expiry, start, permission, resource-type, protocol and stored-policy checks below still apply to
every account. SAS layouts implemented (`azemu_core::storage::sig`): queue SAS 2012-02-12, 2013-08-15, 2015-04-05 (and later unchanged); account SAS 2015-04-05, 2020-12-06; user delegation SAS 2018-11-09 to 2025-07-05. Path-style endpoints
(`http://127.0.0.1:10000/devstoreaccount1/...`) are signed with the account name twice in the Shared
Key resource, which is handled (`Endpoint::path_style`). The user delegation key that Get User
Delegation Key returns is an HMAC of its own fields under the account key, so a user delegation SAS
is verified without keeping state.

Bearer tokens are accepted without a check. What a SAS says about itself is honoured:

| Request | Answer |
|---|---|
| no credentials | `403 AuthenticationFailed` |
| expired, or not yet valid, SAS | `403 AuthenticationFailed` with `AuthenticationErrorDetail` "Signature not valid in the specified time frame: ..." |
| SAS with neither `se` nor `si` | `403 AuthenticationFailed` |
| `si` naming an identifier the queue does not have | `403 AuthenticationFailed` |
| `spr=https` over http | `403 AuthorizationProtocolMismatch` |
| account SAS whose `ss` has no `q` | `403 AuthorizationServiceMismatch` |
| account SAS whose `srt` lacks the needed type (`s` list/properties, `c` queue operations, `o` messages) | `403 AuthorizationResourceTypeMismatch` |
| missing permission | `403 AuthorizationPermissionMismatch` |

Permissions: queue SAS `r` (Peek, Get Metadata, Get ACL), `a` (Put), `u` (Update),
`p` (Get, Delete Message); a queue SAS can do nothing else. Account SAS: `l` List
Queues; `r`/`w` service properties and stats; `w` or `c` Create Queue; `d` Delete
Queue and Clear Messages; `w` Set Metadata and Set ACL; `r` Get Metadata and Get ACL
and Peek; `a`, `u`, `p` as above. With `si` the permissions, start and expiry the token
leaves out come from the queue's stored access policy.

## Known differences from Azure

- **Metadata names come back lower-case.** HTTP header names are folded to lower case
  before the service sees them; Azure keeps the case sent.
- **Get Queue Service Stats** answers on the primary host as well as on `-secondary`,
  and always reports `live`.
- **Get User Delegation Key** returns a key for the requested window with any kind of
  authentication. The key is derived from its own fields, so a SAS signed with it verifies.
- **No Entra checks**: bearer tokens are not decoded, so the `401
  InvalidAuthenticationInfo` "Audience validation failed" answer is never produced.
- **CORS** follows the rules set with Set Queue Service Properties (`azemu_core::storage::cors`, shared
  with Table): an `OPTIONS` request needs no credentials and is judged against the service, not the queue.
  Rules are tried in order; the first whose origin (case-sensitive, `*` or a pattern like
  `http://*.contoso.com`) and method match is the only one that counts, and every header in
  `Access-Control-Request-Headers` must be in its `AllowedHeaders` (names without case, a trailing `*` is a
  prefix). A match is `200` with `Access-Control-Allow-Origin` (the origin, or `*` when the rule says `*`),
  `-Allow-Methods`, `-Allow-Headers`, `-Max-Age`, and for an exact origin `-Allow-Credentials: true` and
  `Vary: Origin`. No match, or no rules, is `403 CorsPreflightFailure`; no `Origin` or no
  `Access-Control-Request-Method` is `400 MissingRequiredHeader`. An actual request with `Origin` gets
  `Access-Control-Allow-Origin` and `Access-Control-Expose-Headers` from the matching rule (also on
  errors); a GET/HEAD without a match gets `Vary: Origin`. Not copied from a recording of real Azure.
- **Non-ASCII header values** (for example metadata): Shared Key verification reads the bytes both as
  UTF-8 and as Latin-1 (the Python and JavaScript SDKs send Latin-1 bytes and sign the UTF-8 of the
  characters; Go sends UTF-8). A correct signature then gets the service's own answer: metadata values must be
  ASCII, so `400 InvalidMetadata`, not `403 AuthenticationFailed`.
- **`sip` (allowed IP range) in a SAS is not checked.** `sv` is not validated either,
  nor is `x-ms-version` (the request's value is echoed).
- **Error bodies copied from recordings**: `QueueNotFound`, `QueueAlreadyExists`,
  `InvalidResourceName`, `InvalidXmlDocument` (position and reason),
  `RequestBodyTooLarge`. The wording and extra elements of the other errors
  (`OutOfRange...`, `InvalidQueryParameterValue`, `MissingRequiredQueryParameter`,
  `PopReceiptMismatch`, `MessageNotFound`, the authorization family, name-length
  `OutOfRangeInput`) follow the Azure documentation, not a recording.
- Pop receipts are base64 records of the same size and shape as Azure's but are
  otherwise the emulator's own.
- Timestamps in state have sub-second precision, so `visibilitytimeout=0` makes a
  message visible at once; responses show whole seconds as Azure does.
- `timeout` is ignored. No quotas (number of queues, messages, bytes per queue).
- State is lost on restart unless `--data-dir` is set (below); `/_azemu/reset` always starts empty.

## Keeping data between runs (`--data-dir`)

`<data-dir>/queue/queue.state` (kind `queue`, version 1; `crates/services/queue/src/persist.rs`): for each account its queues (metadata, stored access
policies, every message with its text, insertion / expiry / next-visible times as absolute times, dequeue count, pop receipt) and the service properties
(logging, metrics, CORS). A message that was handed out stays hidden until the time it was hidden to, and its pop receipt still deletes or updates it; an
expired message is dropped as before, when something looks at the queue. Nothing else exists in this service to keep. `Get Messages` that returns a
message is a change; peeks, listings and a `Get Messages` that finds nothing are not. The file is written only when its bytes differ from the last save.

Version 1 of `queue.state` is checked in under `crates/services/queue/tests/state-v1/queue/` and loaded by `tests/state_v1.rs`. A 501 never happens in this service (an unknown verb is `405 UnsupportedHttpVerb`).
