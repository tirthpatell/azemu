# Blob Storage

**What it emulates.** The Azure Blob Storage REST API: containers, block / append / page blobs, leases, snapshots, versions (when turned on), soft delete,
tags, copy, batch, SAS and anonymous access, immutability policies and legal holds, Storage Analytics logging (`$logs`), and quick query (`Query Blob
Contents`). The Data Lake Gen2 `dfs` endpoint is served by the same crate over the same data (`docs/services/datalake.md`).

**How to reach it.** `http(s)://{account}.blob.core.localhost:4577/{container}/{blob}` (any account name works without being created first). With
`--standard-ports`: `http://azemu:10000/{account}/{container}/{blob}`, the Azurite address, and `UseDevelopmentStorage=true` works. From a container or
from a Windows or macOS machine, use the standard ports (`docs/reaching-the-emulator.md`).

**Not built.** Rename Container, Create Container Session, Incremental Copy (501); quick query with Parquet input or Arrow output (501); Arrow listings; Set Blob
Expiry on a flat container; encryption (a customer-provided key is checked and its hash stored, nothing is encrypted); object replication, container-level
immutability, account-level soft delete; a static website endpoint; throttling. Signatures are verified only for `devstoreaccount1`. Blob versioning is off
until it is turned on (see "Known differences").

Crate `azemu-blob`, standalone binary `azemu-blob`.

| | |
|---|---|
| Service name | `blob` |
| Gateway URL | `http://{account}.blob.core.localhost:{port}/{container}/{blob}` (also `https://`, same port) |
| Pinned URL (Azurite style) | `http://127.0.0.1:{port}/{account}/{container}/{blob}` |
| Default account on a pinned port | `devstoreaccount1` |
| Accounts | any name works without being created first; state is kept per account |
| State | in memory, and on disk too with `--data-dir` (`bytes::Bytes`, no copy for single-block blobs); `POST /_azemu/reset` drops everything, `POST /_azemu/reset?account=<name>` one storage account (Blob and Data Lake share it; its versioning switch goes back to the default) |
| Settings (environment, read at start through `azemu_core::settings`; the list with defaults is `docs/SETTINGS.md`) | `AZEMU_BLOB_LOG_DELAY`, `AZEMU_BLOB_LOG_MAX_BLOBS`, `AZEMU_BLOB_LOG_MAX_BYTES` (Storage Analytics logging, see below), `AZEMU_BLOB_VERSIONING` (off by default as on Azure; on with `AZEMU_LENIENT=1` or when set on its own), `AZEMU_BLOB_VERSION_BYTES` (default 268435456), `AZEMU_BLOB_SOFT_DELETE_BYTES` (default 268435456), `AZEMU_BLOB_QUERY_*` (quick query limits, see `docs/services/blob-query.md`) |

## Pointing a client at it

```
azemu --pin blob=10000                         # all services, blob also on 10000
azemu-blob --port 4580 --pin blob=10000        # only the blob service
```

Azurite-style connection string (the well-known development account and key):

```
DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;BlobEndpoint=http://127.0.0.1:10000/devstoreaccount1;
```

Gateway (host style), any account name and any base64 key:

```
DefaultEndpointsProtocol=http;AccountName=myaccount;AccountKey=<any base64>;BlobEndpoint=http://myaccount.blob.core.localhost:4577;
```

```python
from azure.storage.blob import BlobServiceClient
svc = BlobServiceClient.from_connection_string(conn_str)
c = svc.create_container("demo")
c.upload_blob("hello.txt", b"hello")
print(c.download_blob("hello.txt").readall())
```

`az` CLI: `az storage blob upload --connection-string "<one of the above>" -c demo -n hello.txt -f hello.txt`
(the CLI also accepts `--blob-endpoint http://myaccount.blob.core.localhost:4577 --account-name myaccount --account-key <key>`).

`UseDevelopmentStorage=true` points the SDKs at `127.0.0.1:10000`, so start with `--standard-ports` or pin the service to
`10000` for that to work unchanged. HTTPS works on the same port; trust the CA from
`/_azemu/ca.pem` (`SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS`).

SDK note: the Python SDK treats any host that contains `localhost` as an Azurite-style
path-style address when it parses a *blob URL* (`BlobClient.from_blob_url`), and puts the
first path segment in the URL twice. Build such clients from parts
(`BlobClient(account_url, container_name, blob_name, credential=...)`) or use a pinned
port.

## Operations

| Operation | REST | Status |
|---|---|---|
| List Containers (`prefix`, `marker`, `maxresults`, `include=metadata,deleted,system`) | `GET /?comp=list` | built |
| Get / Set Blob Service Properties (logging, metrics, CORS, delete retention, static website, default version) | `GET`/`PUT /?restype=service&comp=properties` | built |
| Get Blob Service Stats | `GET /?restype=service&comp=stats` | built (always `live`) |
| Get User Delegation Key | `POST /?restype=service&comp=userdelegationkey` | built (key is random, never verified) |
| Get Account Information | `GET`/`HEAD ...?restype=account&comp=properties` | built (fixed `StorageV2` / `Standard_RAGRS`) |
| Find Blobs by Tags (account, container) with the tag filter grammar | `GET /?comp=blobs`, `GET /{c}?restype=container&comp=blobs` | built |
| Blob Batch (Delete Blob, Set Blob Tier sub-requests) | `POST /?comp=batch`, `POST /{c}?restype=container&comp=batch` | built; the answer is laid out as Azure writes it: a part without a body is the headers and one blank line, then the next delimiter; a part with a body has the body and a line break; no line break after the closing delimiter (the Java batch client fails every part otherwise) |
| CORS preflight | `OPTIONS` | built on the `blob` and `dfs` endpoints with the shared code `azemu_core::storage::cors` (as Queue and Table): rules from the service properties, origin compared as written (case sensitive) with `*` and `http://*.contoso.com` patterns, no credentials needed, `403 CorsPreflightFailure` / `400 MissingRequiredHeader`; the same code adds the `Access-Control-*` headers to an actual request with a matching `Origin` |
| Create / Delete Container | `PUT`/`DELETE /{c}?restype=container` | built |
| Get Container Properties / Metadata, Set Container Metadata | `GET`/`HEAD`/`PUT /{c}?restype=container[&comp=metadata]` | built |
| Get / Set Container ACL (public access, up to 5 stored access policies) | `GET`/`PUT /{c}?restype=container&comp=acl` | built |
| Lease Container (acquire, renew, change, release, break) | `PUT /{c}?restype=container&comp=lease` | built |
| Restore Container | `PUT /{c}?restype=container&comp=undelete` | built (needs the delete retention policy on) |
| List Blobs (flat, hierarchy; `prefix`, `delimiter`, `marker`, `maxresults`, `startFrom`, `include=` snapshots, versions, deleted, metadata, tags, copy, uncommittedblobs, legalhold, immutabilitypolicy) | `GET /{c}?restype=container&comp=list` | built |
| Rename Container, Create Container Session | `PUT ...comp=rename`, `POST ...comp=session` | 501 |
| Put Blob (block, append, page) | `PUT /{c}/{b}` | built |
| Put Blob From URL | `PUT /{c}/{b}` + `x-ms-copy-source` + `x-ms-blob-type` | built |
| Get Blob, Get Blob Properties, Get Blob Metadata (ranges, `x-ms-range-get-content-md5/-crc64`, SAS response header overrides, snapshot, versionid) | `GET`/`HEAD /{c}/{b}` | built |
| Delete Blob (`x-ms-delete-snapshots`, snapshot, versionid, `deletetype=permanent`) | `DELETE /{c}/{b}` | built |
| Undelete Blob | `PUT ...?comp=undelete` | built |
| Set Blob Properties (incl. page blob resize and sequence number) | `PUT ...?comp=properties` | built |
| Set Blob Metadata | `PUT ...?comp=metadata` | built |
| Snapshot Blob | `PUT ...?comp=snapshot` | built |
| Lease Blob (acquire, renew, change, release, break) | `PUT ...?comp=lease` | built |
| Set Blob Tier (Hot, Cool, Cold, Archive with pending rehydration, Smart, Premium page tiers) | `PUT ...?comp=tier` | built |
| Get / Set Blob Tags, `x-ms-tags`, `x-ms-if-tags` | `GET`/`PUT ...?comp=tags` | built |
| Copy Blob (synchronous), Abort Copy | `PUT /{c}/{b}` + `x-ms-copy-source`, `PUT ...?comp=copy` | built (copies are complete when the call returns, so Abort always answers `NoPendingCopyOperation`) |
| Incremental Copy Blob | `PUT ...?comp=incrementalcopy` | 501 |
| Put Block, Put Block From URL, Put Block List, Get Block List | `PUT ...?comp=block`, `comp=blocklist`, `GET ...?comp=blocklist` | built |
| Append Block, Append Block From URL, Append Blob Seal | `PUT ...?comp=appendblock`, `comp=seal` | built |
| Put Page (update, clear), Put Page From URL, Get Page Ranges (and diff against a snapshot, paging) | `PUT ...?comp=page`, `GET ...?comp=pagelist` | built |
| Set / Delete Immutability Policy, Set Legal Hold | `PUT`/`DELETE ...?comp=immutabilityPolicies`, `PUT ...?comp=legalhold` | built (enforced on delete and overwrite) |
| Query Blob Contents (quick query: a SQL `SELECT` over a block blob, CSV and JSON in and out, answered as an Avro stream of data, progress and error records; conditions, lease, snapshot, version, customer-provided key) | `POST ...?comp=query` | built, except Parquet input and Arrow output (`501`, naming them); bounded by blob size, record size, answer size, time and four queries at once. See `docs/services/blob-query.md` |
| Set Blob Expiry | `PUT ...?comp=expiry` | built for hierarchical (Data Lake) containers, 501 on flat ones |
| Anything else | | 501 `NotImplemented`, the message names the operation |

Also built across operations: conditional headers (`If-Match`, `If-None-Match`,
`If-Modified-Since`, `If-Unmodified-Since`, `x-ms-if-tags`, `x-ms-blob-if-*` for tags,
`x-ms-source-if-*` for copies, append position / max size, page sequence numbers, access
tier change time), lease ids on everything that takes one, content MD5 and CRC64
(CRC-64/NVME) validation and return, structured messages (`x-ms-structured-body: XSM/1.0; properties=crc64`,
see below), customer-provided key and encryption scope headers
(nothing is encrypted; the key hash is stored and checked, see "Customer-provided keys" below), per-version `x-ms-version-id`.

## Auth

**Signature checks.** For the development account `devstoreaccount1` (the only account
whose key a client can know) the signature of a Shared Key / Shared Key Lite request and of a SAS is verified: `SharedKey` and `SharedKeyLite` headers, account SAS, service SAS (resources `c`, `b`, `bs`, `bv`, `d`) and user delegation SAS. A well-formed
signature (base64 of 32 bytes) that does not match is `403 AuthenticationFailed` with an
`AuthenticationErrorDetail` ("The MAC signature found in the HTTP request ... is not the same as
any computed signature. Server used following string to sign: ..." for Shared Key, "Signature did
not match. String to sign used was ..." for a SAS). Because the signed resource is part of the
signature, a SAS for one container or blob does not open another. Not checked, by design:

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
- batch sub-requests (the outer request carries the signature);
- `AZEMU_LOOSE_AUTH=1` in the environment (read once at start) turns every signature check off.

Expiry, start, permission, resource-type, protocol and stored-policy checks below still apply to
every account. SAS layouts implemented (`azemu_core::storage::sig`): service SAS 2012-02-12, 2013-08-15, 2015-02-21, 2015-04-05, 2018-11-09, 2020-12-06 (and later unchanged, up to the Go SDK's 2026-12-06); account SAS 2015-04-05, 2020-12-06; user delegation SAS 2018-11-09, 2020-02-10, 2020-12-06, 2025-07-05, 2026-04-06. Path-style endpoints
(`http://127.0.0.1:10000/devstoreaccount1/...`) are signed with the account name twice in the Shared
Key resource, which is handled (`Endpoint::path_style`). The user delegation key that Get User
Delegation Key returns is an HMAC of its own fields under the account key, so a user delegation SAS
is verified without keeping state.

Bearer tokens are accepted without a check. A SAS is also honoured for what it says:

- expiry `se`, start `st` (both also from a stored access policy `si`; an unknown policy fails)
  -> `403 AuthenticationFailed` with `AuthenticationErrorDetail`
- `spr=https` over HTTP -> `AuthorizationProtocolMismatch`
- `sip` -> `AuthorizationSourceIPMismatch`; the client address is taken from
  `X-Forwarded-For`, else assumed to be `127.0.0.1` (core does not hand the peer address to services)
- account SAS `ss`, `srt` -> `AuthorizationServiceMismatch`, `AuthorizationResourceTypeMismatch`
- permissions `sp` per operation -> `AuthorizationPermissionMismatch` (creating a new blob
  needs `c` or `w`, overwriting needs `w`, append needs `a` or `w`, delete `d`, delete version `x`,
  tags `t`, list `l`, find `f`)

A request with no credentials is anonymous: reads of blobs on containers with public access
`blob` or `container`, list / properties on `container` access, nothing else; everything
else is `401 NoAuthenticationInformation` with a `WWW-Authenticate` challenge. Creating, uploading,
deleting and listing containers are always 401, whether or not the container exists. Three cases
have their own answer:

- In an account that does not allow public access, an anonymous read is `409 PublicAccessNotPermitted`,
  a container cannot be made public (409), and a copy whose source is in such an account gets 409.
  An account that appears on first use allows public access; an account made through Resource Manager
  follows its `allowBlobPublicAccess` property, which is false unless set, as on Azure.
- An anonymous read of a container that does not exist is `404 ResourceNotFound`. No recording
  shows what Azure answers here, so this one is a guess.
- An `Authorization` header that cannot be read (an unknown scheme, or `Bearer` with no token) is
  `400 InvalidAuthenticationInfo`.

## Structured messages

A client can send the body of Put Blob, Put Block, Put Page, Append Block (and the Data Lake append) as a
structured message, a header, segments each with a CRC-64, and a trailer with the CRC-64 of all the data, and
say so with `x-ms-structured-body: XSM/1.0; properties=crc64` and `x-ms-structured-content-length` (the Go SDK
with `TransferValidationTypeComputeStructuredMessageCRC64`, the Python SDK with `validate_content="crc64"`).
The service takes the message apart, checks every CRC, stores only the data, and puts `x-ms-structured-body`
in the answer. A bad CRC is `400 Crc64Mismatch`; a message with the wrong layout is `400 InvalidInput`
(its message names `x-ms-structured-body`); an unknown schema, or a missing or wrong
`x-ms-structured-content-length`, is a 400 about that header. A Get Blob with `x-ms-structured-body` gets the data
(or the range) wrapped in one message per 4 MiB, with `x-ms-structured-body` and `x-ms-structured-content-length`
in the answer; `Content-MD5` and `x-ms-content-crc64` are left out because they describe the data, not the message.
The code is `crates/services/blob/src/structured.rs`; the layout comes from the Go SDK
(`azblob/internal/shared/structured_message.go`) and 22 recordings of the Python SDK. Error names for a wrong
layout are not documented by Microsoft, so `InvalidInput` is a guess. The message is taken apart before the
signature is checked, so a request with a broken message and no credentials gets 400 instead of 403.

## Data Lake

The same crate serves the Data Lake Gen2 `dfs` endpoint over the same data (`BlobService::datalake()`;
see `docs/services/datalake.md`). A container the `dfs` endpoint has touched is hierarchical: its blobs
carry owner / group / ACL / resource-type headers, directories are zero-length blobs with
`hdi_isfolder=true`, listings add `ResourceType` / `Expiry-Time`, `showonly=deleted` lists deleted paths,
`comp=undelete` with `x-ms-undelete-source` restores one. Containers used only through the blob endpoint
are unchanged.

## Known differences from Azure

- `x-ms-delete-type-permanent` is `true` when nothing of the deleted blob stays recoverable (no soft delete, and no version kept by versioning) and `false` otherwise. The XML errors of a bad Set Service Properties body carry `LineNumber`/`LinePosition` 1/1 where Azure points into the document (the Queue service has the positions).

- Put Page, Put Page From URL and Clear Pages take the range from `x-ms-range` or `Range` (`x-ms-range` wins when both are sent), as the REST docs say.
- In a query string `+` is a space and a plus is `%2B` (core `Query`); in a blob name inside the path `+` is a plus. A SAS `sig` with an unencoded `+` is
  refused for `devstoreaccount1`.

- **Metadata that is not ASCII** (`x-ms-meta-*` with a byte above 0x7F, for example `café`) is `400 InvalidMetadata`, as on
  Azure and in Queue. The Shared Key check lets such requests through, so the request
  reaches Blob, which refuses it on Put Blob, Set Blob Metadata and Create Container
  (`crates/services/blob/tests/cors_metadata.rs`).
- **Signatures are verified only for `devstoreaccount1`** and only when well formed (see Auth).
  For other accounts a SAS for another container or blob, a tampered signature and a wrong
  SharedKey signature are accepted. The mismatch text is the one Azure documents; the
  recordings hold no signature mismatch to copy it from.
- **Versioning is off by default, as on Azure, and old copies are limited.** A storage account that appears on first use keeps
  no versions until someone turns versioning on: `isVersioningEnabled` on `blobServices/default` through ARM (per account), or the emulator
  started with `AZEMU_BLOB_VERSIONING=1` (every account that ARM did not make) or `AZEMU_LENIENT=1`. An account made through ARM has it off. Microsoft's
  recordings were made on accounts with versioning on, so the replay run needs `AZEMU_BLOB_VERSIONING=1`. With versioning on, every Put Blob,
  Put Block List, Copy, Set Metadata, Set Properties and Snapshot keeps the previous state as a version
  (`x-ms-version-id`, listed with `include=versions`). Because everything is held in memory:
  - the previous versions of one account may hold at most `AZEMU_BLOB_VERSION_BYTES` bytes (default
    256 MiB; `0` keeps no previous versions, new writes still get version ids). When it is passed the
    oldest versions of the account go first. A buffer that several versions share (Set Metadata makes a
    version that shares its data with the current blob) is counted once. At most 500 previous versions
    are kept per blob name as well.
  - `AZEMU_BLOB_VERSIONING` (read at start with `settings::lenient_flag`: off unless `AZEMU_LENIENT` or the variable itself says on) is the default for
    accounts that appear on first use.
    `BlobService::set_versioning_enabled(account, bool)` does it for one account (for the ARM crate:
    `isVersioningEnabled`); `versioning_enabled(account)` reads it. `BlobService::with_versioning(on,
    bytes)` builds a service with these settings without the environment (used by tests). A `reset()`
    drops the per-account switches with the rest of the data.
  - With versioning off the service behaves like an account without versioning: no `x-ms-version-id`,
    no `x-ms-is-current-version`, no `IsCurrentVersion` in listings, writes overwrite in place, deleting
    keeps nothing, and a `versionid` that does not exist is `404 BlobNotFound` (read from Microsoft
    Learn, which says versions can still be read after versioning is turned off; neither the spec nor
    the recordings show a request with `versionid` on an account without versioning, so the status for
    that case is an assumption). Versions that exist when versioning is turned off stay readable and
    deletable; a blob that still has a version id keeps its old state as a previous version when it is
    next changed, and then stops being a version.
  - Blob soft delete (delete retention policy in the service properties) works on top of that the way
    it does on an account without versioning: a deleted blob stays visible with
    `include=deleted`, overwriting keeps a soft-deleted snapshot of the old content. These
    soft-deleted snapshots have their own limit, `AZEMU_BLOB_SOFT_DELETE_BYTES` (default 256 MiB per
    account, `0` keeps none), oldest first, at most 500 per blob name. Soft-deleted items are hidden
    after the retention period but not removed from memory (not built).
- **A container with protected blobs cannot be deleted.** Delete Container (and the Data Lake file system delete, and the container delete
  ARM makes through the Blob endpoint, also when it drops an account) answers `409 ContainerHasLegalHold` while a blob, a snapshot or a version
  in it is under a legal hold, and `409 ContainerImmutabilityPolicyLocked` while one is under a *locked* immutability policy that has not run
  out. Azure's pages on immutable storage ("Container-level WORM policies", "Version-level WORM policies") say a delete of a container with
  protected blobs fails and list those two codes; they do not say which code a *time-based policy that is unlocked* gets, so the emulator
  answers with the blob's own code, `409 BlobImmutableDueToPolicy` (a guess; the pages also say "unlocked policies don't provide delete
  protection" for the account). A blob that is already deleted, a policy whose date is past, and a container-level policy (the emulator has none;
  `x-ms-has-legal-hold` and `x-ms-has-immutability-policy` of the container are always `false`) do not count.
- **Cancel when the client goes away**: `abort_on_disconnect` is true for `GET` and for a `POST` on a blob path (Query Blob
  Contents), false for everything else. The Data Lake service asks for `GET` only (its `POST` is a query or a lease, and the two cannot be told
  apart from the path). A streamed answer that nobody reads is stopped by core in any case.
- **Container soft delete** follows the same delete retention policy (Azure has a separate
  account setting that is not reachable over the data plane).
- **Copy** is synchronous and the source URL must point at this emulator (resolved from
  state, never fetched). A source in another account needs a SAS or public access;
  other hosts answer `404 CannotVerifyCopySource`.
- A failed `x-ms-source-if-modified-since` / `-if-none-match` on Put Blob / Put Block / Append Block /
  Put Page **from URL** is `304 CannotVerifyCopySource` (as recorded); `-if-match` / `-if-unmodified-since`
  are 412. `x-ms-source-if-tags` is checked on Copy Blob and the from-URL operations. Set Blob Tier
  ignores `x-ms-if-tags` on page blobs (as recorded).
- Block ids that are not valid Base64 are accepted as opaque strings (up to 88 characters).
- `x-ms-access-tier-inferred: false` is sent for an explicit tier (Azure omits it).
- `x-ms-blob-content-md5` on Put Blob is validated against the body (as Azure does);
  on Set Blob Properties any Base64 value is stored.
- Metadata names come back lower-cased (HTTP header names do not keep their case).
- Get Page Ranges diff compares content, so rewriting identical bytes is not reported
  as a change.
- Lease durations, break periods and expiry use the request time; a fixed lease expires
  lazily on the next request that looks at it.
- `Put Block List` / `Put Block` do not enforce the 4000 MiB block limit; one request is
  limited by core to 1 GiB. A page blob read is limited to 1 GiB per request.
- Listings are never in Arrow format; account kind, SKU and stats are fixed values.
- **Query Blob Contents** answers with a `Content-Length` (Azure streams it chunked), builds the whole answer in memory (at most
  128 MiB), and has no Parquet input or Arrow output; error names and several formatting rules are guesses, listed in
  [`blob-query.md`](blob-query.md) ("Behaviour that could not be confirmed against Azure").
- Static website content (`$web`) is stored like any container but not served on a web
  endpoint; the `$web` container is made when static website is switched on (as on Azure), and is listed and deletable like any container. The ARM-only account settings (container-level immutable storage with
  versioning, object replication, account-level soft delete) are not reachable.
- All state still sits behind one lock, but nothing that grows with a body is done while it is held:
  hashes of a request body, parsing a block list, joining the pieces of a download and hashing a
  range, and joining and hashing the source of a "from URL" operation happen before the lock is taken
  or after it is released.
- Limits that Azure has and that are kept here against unbounded growth: request XML at most 16 MiB
  and 64 levels deep (`400 InvalidXmlDocument`), 100,000 staged blocks per blob
  (`409 BlockCountExceedsLimit`), 256 soft-deleted containers per account (older ones, and those past
  the retention period, are dropped when another container is deleted).

## Storage Analytics logging (`$logs`)

Built for the Azure Functions host: the host, when its storage
endpoints are host-name style, finds new blobs for a polling blob trigger in the Storage Analytics log blobs. Code: `src/analytics.rs`. Sources:
Microsoft Learn, "Azure Storage Analytics logging", "Storage Analytics log format", "Storage Analytics logged operations and status messages",
"Enable and manage Azure Storage Analytics logs (classic)", "List Containers" (`include=system`), and the extension's code
(`Microsoft.Azure.WebJobs.Extensions.Storage.Blobs`: `BlobLogListener`, `StorageAnalyticsLogParser`; public source, MIT, in `Azure/azure-sdk-for-net`).

**The container.** `$logs` is made by Set Blob Service Properties when `Read`, `Write` or `Delete` is true, and also by an update that carries CORS, static
website, the default service version or the delete retention policy (the page says these make it too; those do not stop it being deletable). It is an ordinary
container of the account (so it is saved and loaded with the rest), with these differences: List Containers does not return it unless the request has
`include=system`; the Data Lake filesystem list does not return it; Delete Container on it is refused once logging has been switched on (the page: "cannot be
deleted once Storage Analytics has been enabled, though its contents can be deleted"; the page gives no error code, the answer here is `400 InvalidOperation`,
a guess); its blobs can be deleted. No blob events are raised for it (as on Azure). Switching logging off keeps the container.
Other special names, as the same pages say: `$web` is made with the static website setting and is a normal listed container; `$root` is the root container
(a path with one segment is a blob in `$root`) and a normal one; the `$metrics*` names are Table service tables, not containers (not a Blob concern);
`$blobchangefeed` belongs to the change feed, which is not built (nothing makes the container; it would also be left out of a plain List Containers).

**What is logged.** A request is logged when its class is switched on in `Logging`: read (GET, HEAD, OPTIONS, Query Blob Contents, Get User Delegation Key),
write (every other PUT and POST), delete (DELETE). Authorized requests (`Authorization` header) are logged whatever the outcome; anonymous and SAS requests only
when they worked, failed with a server error, or were a `304` on a GET (a SAS that fails its check counts as anonymous: not logged). Copy Blob writes three lines
(`CopyBlob`, `CopyBlobSource`, `CopyBlobDestination`, operation counts 0, 1, 2, one request id; the source line's key is the source URL, as in the page's example).
Operation names are the ones of the "logged operations" page (lease operations by action: `AcquireBlobLease`, ...; a page clear is `ClearPage`).

**Lines.** Version 1.0, 30 fields separated by `;`, in the order of the format page: version, start time (7 fractional digits), operation, status, HTTP status,
end-to-end and server latency (both the time the request took here), authentication type (`authenticated`, `sas`, `bearer`, `anonymous`), requester account (empty for
anonymous and SAS), owner account, `blob`, request URL (quoted), requested object key (quoted, always `/account/container/blob`), request id (the
`x-ms-request-id` the client got), operation count, requester IP, `x-ms-version`, header, packet and content sizes, request MD5, server MD5, ETag
(quoted, HTML encoded: `"&quot;0x8D...&quot;"`), last modified (`Monday, 03-Jan-22 20:34:54 GMT`), conditions used (`If-Match=...`), user agent, referrer, client
request id. A field that can hold a quote, a semicolon or a line break is quoted and HTML encoded (`&`, `"`, `<`, `>`, line breaks); an empty one is empty. A client
supplied field is cut at 1 KiB, the URL at 4 KiB. With `Logging.Version` 2.0 the eight extra fields of format 2.0 are appended, empty. The request status is `Success`,
`SASSuccess`, `OAuthSuccess` or `AnonymousSuccess`; for a failure it is the error code (the page says versions 2017-04-17 and later write the code), or
`ClientOtherError` / `ServerOtherError` / `ThrottlingError` (with `SAS` / `Anonymous` in front) when there is none. **No credential is written:** the `Authorization`
header never; the SAS signature is `sig=XXXXX` in the URL (also in a copy source), as Azure writes it.

**Log blobs.** Named `blob/YYYY/MM/DD/hh00/NNNNNN.log` (hour of the end of the request; the minutes are always `00` on Azure too; the counter is per account and
hour, starts at `000000`, and a number that was used is not used again even when its blob was deleted), block blobs with the metadata `LogType` (`read`, `write`,
`delete`, the ones present, comma separated), `StartTime`, `EndTime` (`YYYY-MM-DDThh:mm:ssZ`) and `LogVersion`. A log blob is written once and never changed
(the Functions host reads each name once). A flush makes one or more blobs per hour, at most 4 MiB each.

**When.** The request path only builds the finished lines and puts them in a queue (50,000 lines or 16 MiB, whatever does not fit is dropped and counted); no
work happens under the state lock. One tokio task per batch sleeps `AZEMU_BLOB_LOG_DELAY` (default 3 seconds; Azure takes up to an hour), then writes the batch under
one short hold of the lock. A save (`--data-dir`, also the final one) writes what is queued first, so a stop does not lose lines.

**Limits.** Retention: log blobs older than `Logging.RetentionPolicy.Days` (when enabled) are removed when a batch is written, when the retention is changed, and when
`$logs` is listed. Hard limits per account, oldest first: `AZEMU_BLOB_LOG_MAX_BLOBS` (default 2000) and `AZEMU_BLOB_LOG_MAX_BYTES` (default 64M); the newest blob is
always kept. Only blobs this writer made (a name under `blob/` with a `LogType` metadata) are removed; a blob the user put in `$logs` is not touched.

**Left out, and different from Azure:** Queue and Table logging (other services); Data Lake (`dfs`) requests and the sub-requests of a Blob Batch are not logged
(the batch request itself is, as `BlobBatch`, which is not on the page's list); requests addressed to `$logs` are not logged (Azure logs reads of analytics data; here, a
client that follows the logs every two seconds would make a new log blob every time); the requester IP is empty (core does not pass the peer address on) and the request
URL carries the host the client used (`http://acct.blob.core.localhost:4577/...`, or `http://127.0.0.1:10000/acct/...`) instead of `*.blob.core.windows.net`; header and packet
sizes are approximate; the latencies are the time the request took here (end-to-end and server are equal); "duplicate records" and the hourly merge of Azure do not exist;
the `$metrics` tables (Storage Analytics metrics) are not written. The `2.0` extra fields are empty (the emulator has no identities to put in them).


**`maxresults`**: List Blobs, List Containers, Find Blobs by Tags, Get Page Ranges and the deleted-path listing treat a value above 5000 as 5000 (the
`MaxResults` element of the answer says 5000), and zero or less, or text that is not a number, as `400` (`OutOfRangeQueryParameterValue` / `InvalidQueryParameterValue`).
Data Lake path listing (`maxResults`), Queue list (`maxresults`, 5000), Table `$top` (1000) were already like that; Queue Get / Peek Messages keep the real limit of 32.

## Streaming and cancellation

The state lock is never held while a body is sent. A **download** of a blob made of several pieces (blocks, page ranges with holes)
and more than 1 MiB, without a hash to send, is streamed piece by piece (`pre.rs`, `Download`): no joined copy of the blob is built
(a 1 GiB blob used 1 GiB more), `Content-Length` is set from the blob's size, and the blob can be overwritten while it is sent (the
pieces are shared handles). **Quick query** answers are streamed too (`docs/services/blob-query.md`). In both, the request is cancelled when the client goes away only for the requests Blob asks about (a `GET`, or a `POST` on a blob path, which is a quick query), and a
client that takes no data for the body idle time (60 s) loses its connection. Names follow one set of rules on both endpoints
(`docs/services/datalake.md`, "Names").

## Customer-provided keys

Nothing is encrypted. What is checked (`src/cpk.rs`, `src/store.rs`):

* Every request that carries `x-ms-encryption-key`, `-key-sha256` and `-algorithm` (or the `x-ms-source-encryption-*` set of a copy from a URL) must
  have them agree: a key without its hash or algorithm is `400 MissingRequiredHeader`; an algorithm other than `AES256`, a key that is not base64 of 32 bytes, or
  a hash that is not the base64 SHA-256 of the key is `400 InvalidHeaderValue` (header `x-ms-encryption-key` for a hash mismatch; the key is never echoed). A hash alone
  is accepted on reads (property requests); a write with a hash and no key is `400 MissingRequiredHeader`.
* A blob written with a key stores the hash. Reading or writing it with no key, or with a key whose hash differs, is `409 BlobUsesCustomerSpecifiedEncryption`.
  The missing-key case is recorded from Azure; the different-key case is a choice (same code and status: it is the only code the SDKs know for this; the
  real answer is not in the specification or the recordings).
* Not checked: a key sent for a blob that was not written with one is accepted (the real service most likely refuses it). Encryption scopes are only recorded, as before.

## What `GET /_azemu/unsupported` shows

A 501 of the Blob service names its operation: `Blob_Query (Parquet input)`, `Blob_Query (Arrow output)`, `Blob_RenameContainer`,
`Blob_CreateSession`, `Blob_IncrementalCopy`, `Blob_SetExpiry` (account without hierarchical namespace), `Blob_Unknown (GET comp=...)` for a
request nothing matches, and `DataLake_Unknown (PATCH action=...)` on the Data Lake endpoint. The names carry no account, container or blob names.

## Keeping data between runs (`--data-dir`)

The `blob` service owns the files for both Blob and Data Lake (they share one state); the `dfs` service keeps nothing of its own. Files in `<data-dir>/blob/`
(`crates/services/blob/src/disk.rs`):

* `blob.state` (kind `blob`, version 1): every account, container (properties, metadata, stored access policies, public access, lease, soft-deleted
  containers with their deletion version and time, the hierarchical-namespace flag and its pending bookkeeping, deleted Data Lake paths), every blob
  (current, snapshots, previous versions, soft-deleted ones with their deletion times: properties, metadata, tags, tier, lease with its absolute expiry,
  copy state, legal hold, immutability policy, encryption scope, the SHA-256 of the customer-provided key, Data Lake owner / group / ACL / expiry /
  appended-but-not-flushed data), committed blocks and staged (uncommitted) blocks, page ranges, service properties (logging, metrics, CORS, soft
  delete, static website, default version), the per-account versioning switch and the version / soft-snapshot byte pools. It does not hold the data.
* `pieces/<32 hex>`: the data, written once and never changed. A buffer of at least 1 MiB is a file of its own; smaller ones are packed into files of
  up to 8 MiB. `blob.state` says where each buffer is (file, offset, length, CRC-64). A buffer that is already in a file is not written again (two blobs that
  share a buffer, a snapshot and its blob, point to the same place), so a save after a small change writes `blob.state` and the new buffers only.
* A save copies the metadata under the lock (data is shared, not copied), encodes it and writes new pieces without the lock, replaces `blob.state`, and
  only then deletes the data files nothing refers to. A crash at any point leaves the old `blob.state` with all its files. A pack that is mostly dead (less
  than a quarter alive and at least 1 MiB wasted) has its live buffers moved to a new file.
* Loading reads every referenced data file into memory (the emulator keeps blob data in memory anyway; the data type does not allow reading on first
  use), checks length and CRC-64 of every buffer, rebuilds shared Data Lake ACL lists, and refuses a file that contradicts itself (a block blob whose size is
  not the sum of its blocks, page ranges outside the blob). Unknown files are left alone; unreferenced data files are deleted by the next save.
* Not kept: the settings of the run (`AZEMU_BLOB_VERSIONING` and the other `AZEMU_*` variables; an account keeps the versioning switch and byte limits it had; version 1 of the file is checked in under `tests/state-v1/blob/` and loaded by `tests/state_v1.rs`), account keys
  (derived from the account name), events waiting for Event Grid.
* Reads are not changes. A request changes the state unless it is `GET`/`HEAD`/`OPTIONS`, except that a read of a Data Lake filesystem that first makes
  it hierarchical or takes in blob-endpoint writes counts as a change. Lazily applied time (an expired lease, an expired Data Lake path, a soft-deleted item past
  retention) is not a change until something acts on it; the saved absolute times mean the same after a restart.
