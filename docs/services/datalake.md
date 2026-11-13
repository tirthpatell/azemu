# Data Lake Storage Gen2 (`dfs`)

**What it emulates.** The Azure Data Lake Storage Gen2 REST API (`dfs` endpoint): filesystems, directories and files, rename, ACLs (access and default,
recursive changes), POSIX permissions, append and flush, leases, soft delete, expiry. It is served by the Blob crate over the same data: a filesystem is
a blob container and a file is a block blob, so what is written through one endpoint is visible through the other. Azurite does not have it.

**How to reach it.** `http(s)://{account}.dfs.core.localhost:4577/{filesystem}/{path}`. With `--standard-ports`: `http://azemu:10003/{account}/{filesystem}/{path}`.

**Not built.** Container sessions, Rename Container, Parquet and Arrow in quick query; `Content-MD5` and CRC64 of a flushed file are not computed; owner and
group are always `$superuser` (no identity is checked); the hierarchical switch is per container, not per account. Signatures follow Blob: checked only for
`devstoreaccount1`.

| | |
|---|---|
| Service name | `dfs` (`BlobService::datalake()` returns it; it shares the Blob service's data) |
| Gateway URL | `http://{account}.dfs.core.localhost:{port}/{filesystem}/{path}` (also `https://`, same port) |
| Pinned URL (Azurite style) | `http://127.0.0.1:{port}/{account}/{filesystem}/{path}`, `--pin dfs=10003` (or `--standard-ports`) |
| Blob URL of the same data | `http://{account}.blob.core.localhost:{port}/{container}/{blob}` |
| Default account on a pinned port | `devstoreaccount1` |
| Errors | JSON `{"error":{"code":"…","message":"…"}}` and `x-ms-error-code`, `Server: Windows-Azure-HDFS/1.0 Microsoft-HTTPAPI/2.0` |
| State | in memory (and on disk too with `--data-dir`), `POST /_azemu/reset` drops everything (both endpoints) |

## Pointing a client at it

`azemu-blob` and the `azemu` binary register both services (`blob` and `dfs`); see "How it is registered" at the end of this file.

```python
from azure.storage.filedatalake import DataLakeServiceClient
KEY = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="
svc = DataLakeServiceClient(account_url="http://devstoreaccount1.dfs.core.localhost:4577", credential=KEY)
fs = svc.create_file_system("demo")
fs.create_directory("a/b").create_file("c.txt").upload_data(b"hello", overwrite=True)
print([p.name for p in fs.get_paths(recursive=True)])      # ['a', 'a/b', 'a/b/c.txt']
```

Any account name works (host style); `devstoreaccount1` with its well-known key also has its
signatures checked. Use `*.dfs.core.localhost`: the SDK derives the blob URL by replacing `.dfs.`
with `.blob.` in the host and treats hosts with `.core.` as normal (not Azurite path style)
addresses. The SDK sends some operations to the blob endpoint (filesystem create / delete /
properties / list, leases, path properties and downloads, metadata, expiry, undelete, deleted
path listing, user delegation keys), so both endpoints must be reachable on the same port.

**The JavaScript client needs another host form.** `@azure/storage-file-datalake` turns a `dfs` URL into its blob URL only
for host names it knows (`dfs.core.windows.net` and the other Azure clouds). With `http://{acct}.dfs.core.localhost:{port}` it
sends the calls that need the blob endpoint (create / delete / exists / list / properties of a file system, leases, metadata,
user delegation keys) to the `dfs` host, and the emulator answers 501 for them. Use
`http://{acct}.dfs.core.windows.net.localhost:{port}`: the SDK maps it to `{acct}.blob.core.windows.net.localhost`, and the
emulator routes both hosts (the service is picked by the second label, anything after it is ignored). The Python and Go clients
do not have this limit.

Why a blob-only operation on a `dfs` host is 501 and not served: Microsoft says that on a hierarchical namespace account "services,
tools, and applications can use either endpoint", but does not list which Blob operations the `dfs` host accepts, and no recording
shows one. The Data Lake SDKs all send blob-only operations to the blob host, which is why they derive that host. So the emulator
keeps its own answer (`501 NotImplemented`, message naming the request) instead of guessing what Azure does.

## The hierarchical model

* A filesystem is a container; a path is a blob. A file is a block blob. A directory is a
  zero-length block blob with the metadata `hdi_isfolder=true` (and `x-ms-resource-type:
  directory` on its properties), named without a trailing slash (`a/b`).
* **When is a container hierarchical?** From the first time it is touched through the `dfs`
  endpoint: created there, or any request that names it (a read counts). A container used only
  through the blob endpoint stays flat and behaves exactly as before (the Blob test suites do
  not see any difference). When a container turns hierarchical, every blob already in it gets
  its attributes and the directories its name implies (`a/b/c.txt` creates `a` and `a/b`); later
  blob-endpoint writes are adopted the same way on the next request. A blob with the metadata
  `hdi_isfolder=true` and no content is a directory. `x-ms-is-hns-enabled` of Get Account
  Information is `true` on the `dfs` endpoint and on the blob endpoint once any container of the
  account is hierarchical.
* A path is a file or a directory, never both (`409 PathConflict`). Creating `a/b/c.txt` creates
  `a` and `a/b`. A directory with children is deleted only with `recursive=true`
  (`409 DirectoryNotEmpty`). Names are case sensitive.
* The extra attributes of a path (owner, group, ACL, sticky bit, expiry, appended but not flushed
  data) are stored on the blob (`DfsAttrs`) and appear as `x-ms-owner`, `x-ms-group`,
  `x-ms-permissions`, `x-ms-acl`, `x-ms-resource-type`, `x-ms-expiry-time` on blob-endpoint
  responses and as `Expiry-Time` / `ResourceType` (and, with `include=permissions`, `Owner`,
  `Group`, `Permissions`, `Acl`) in blob listings.
* Data Lake writes do not create blob versions (an HNS account has no versioning); a blob written
  over a Data Lake file through the blob endpoint keeps its owner / group / ACL.
* Append writes into a pending area by offset; Flush commits it. See Append / Flush below.
* Listings: `List Paths` walks the names in order; a recursive listing is the sorted names under
  the directory, directories and files together. `beginFrom` is relative to the listed directory.
  Blob listing with a delimiter shows a directory as a blob and as a prefix.

## Operations

| Operation | REST | Status |
|---|---|---|
| List FileSystems | `GET /?resource=account` | built (`prefix`, `maxResults`, `continuation`) |
| Create / Get properties / Set properties / Delete FileSystem | `PUT`/`HEAD`/`PATCH`/`DELETE /{fs}` (`?resource=filesystem`) | built (`x-ms-properties`, conditions, `x-ms-namespace-enabled`) |
| List Paths | `GET /{fs}?resource=filesystem&recursive&directory&maxResults&continuation&upn&beginFrom` | built |
| List Blobs (hierarchy) on the dfs endpoint | `GET /{fs}?restype=container&comp=list` | built (same as Blob) |
| Create file / directory | `PUT /{fs}/{path}?resource=file\|directory` | built (conditions, `x-ms-properties`, permissions, umask, ACL, owner, group, lease (`x-ms-proposed-lease-id`), expiry, content headers, CPK, `x-ms-encryption-context`) |
| Rename (`legacy`, `posix`) | `PUT /{fs}/{dest}` + `x-ms-rename-source` | built (file or directory tree in one step, across filesystems; source and destination conditions, source lease) |
| Delete | `DELETE /{fs}/{path}?recursive&continuation&paginated` | built (soft delete when the account's delete retention is on) |
| Read | `GET /{fs}/{path}` | built (ranges, MD5 / CRC64 of a range, conditions) |
| Get properties / status / access control / check access | `HEAD /{fs}/{path}[?action=getStatus\|getAccessControl\|checkAccess]` | built (`checkAccess` always answers 200 for an existing path) |
| Append Data | `PATCH ?action=append&position&flush` | built (out-of-order positions, MD5 / CRC64 of the chunk, lease actions, CPK) |
| Flush Data | `PATCH ?action=flush&position&retainUncommittedData&close` | built (contiguity check, content headers, conditions, lease actions) |
| Set properties | `PATCH ?action=setProperties` | built |
| Set access control | `PATCH ?action=setAccessControl` | built (owner, group, permissions, ACL; the root directory is `/{fs}//`) |
| Set access control recursive | `PATCH ?action=setAccessControlRecursive&mode=set\|modify\|remove&maxRecords&continuation` | built (batches, JSON counters; `failedEntries` is always empty, `forceFlag` accepted) |
| Lease | `POST /{fs}/{path}` + `x-ms-lease-action` | built |
| Set Expiry | `PUT ?comp=expiry` (both endpoints) | built (hierarchical containers; on a flat container the blob endpoint still answers 501) |
| Undelete path | `PUT ?comp=undelete` + `x-ms-undelete-source` (both endpoints) | built |
| List deleted paths | `GET /{fs}?restype=container&comp=list&showonly=deleted` (blob endpoint) | built |
| Get Account Information | `?restype=account&comp=properties` | built (`x-ms-is-hns-enabled`) |
| Query (quick query) | `POST /{fs}/{path}?comp=query` (both endpoints; the SDKs send it to the blob endpoint) | built: CSV and JSON in, CSV and JSON out, the Avro answer; Parquet input and Arrow output answer 501 (`docs/services/blob-query.md`). On the dfs endpoint errors are JSON; the answer carries `x-ms-resource-type: file` |
| Create Container Session | `POST ?comp=session` | 501 (as in Blob) |
| Anything else | | 501 `NotImplemented` in the dfs JSON error form, the message names the request |

### POSIX permissions and ACLs

Stored and returned faithfully, inherited on create, **not enforced** against the caller (a Shared
Key caller is the superuser, auth is loose).

* Owner and group default to `$superuser`; `x-ms-owner` / `x-ms-group` on create or set. The `upn`
  flag is accepted and has no effect.
* Permissions: symbolic (`rwxr-x---`, sticky `t` / `T`) or octal (`0750`, `1777`). Default `rwxr-x---`
  for a directory (`0777 & ~0027`) and `rw-r-----` for a file (`0666 & ~0027`). On create the umask
  (`x-ms-umask`, default `0027`) is applied to `x-ms-permissions`.
* ACL: `user::rwx,user:<id>:r-x,group::r-x,mask::rwx,other::---` plus `default:` entries (octal digits
  and `rwx` accepted). Missing base entries are filled in, a mask is calculated when there are named
  entries, default entries are completed, entries are written in the canonical order. Permissions are
  a view of the access entries: the group bits are the mask when there is one.
* Inheritance: a new path whose parent has default entries takes them as its access ACL, limited by
  the requested permissions (`0666` for a file, `0777` for a directory, or `x-ms-permissions`); a
  directory also gets the default entries; the umask is not used then. An explicit `x-ms-acl` on
  create wins over inheritance.
* Recursive changes visit the directory itself and everything below it in name order; `maxRecords`
  counts entries; the continuation token resumes after the last entry. `set` replaces the ACL (files
  take no default entries), `modify` adds or replaces the given entries, `remove` drops the named
  entries, the mask and default entries listed (the three access base entries stay).
* Setting the ACL does not change the ETag or Last-Modified (as recorded).
* **Limits.** An access ACL and a default ACL each hold at most 32 entries (base entries, mask and named
  entries all count; "effectively 28 named"), from the Microsoft Learn access control article. More is `400 InvalidHeaderValue`
  with `x-ms-acl` as the header and a message that says so (the real error code is not documented anywhere in Microsoft's documentation; this
  one is a guess; the header value is cut to 200 characters in the answer). In a recursive change a path that would end above the
  limit is left as it is and listed in `failedEntries` (`name`, `type`, `errorMessage`). A list of more than 64 entries is refused
  without being parsed to the end. A directory's default ACL is not copied into every child: `DfsAttrs::acl` is an
  `Arc<Vec<Entry>>` and children made from the same default ACL with the same requested permissions get the same `Arc`
  (the directory keeps what it produced, until its own ACL changes); a path that changes its ACL gets a list of its own.
  Making the intermediate directories of a path costs time in proportion to its depth.

### Names

One function (`crates/services/blob/src/names.rs`) judges names for both endpoints: 1 to 1,024 characters, no control characters
(below U+0020) and no U+FFFD (what a name that is not valid UTF-8 turns into, so two bad names cannot become one; U+FFFF is
accepted, as the recording `test_list_encoded_blobs` shows), at most 254 path segments in a flat container and 61 in a
hierarchical one (the REST naming page: 63 including account and container), and no `.` or `..` segment in a hierarchical
container. A bad name is `400 InvalidResourceName`. On the Blob endpoint a write into a hierarchical container (Put Blob, Put
Block, Put Block List, Copy Blob, Put Blob From URL) is `409 PathConflict` when a parent is a file or when the name is a
directory. The Data Lake error table describes `PathConflict` as "the specified path, or an element of the path, exists and its
resource type is invalid for this operation"; what a real account answers on the Blob endpoint for this case is not recorded (a guess).

### Replacing and moving paths

Path - Create over an existing path and Rename onto one apply the checks the Blob endpoint makes for an overwrite
the destination's lease needs `x-ms-lease-id` (`412 LeaseIdMissing`, and so on), a legal hold or an
immutability policy on the destination is `409 BlobImmutableDueToLegalHold` / `BlobImmutableDueToPolicy`, and the source of a rename
(and everything below a source directory) must not be under a hold or policy either, because the rename takes it from its old
name (as Delete Blob would). Conditional headers apply to both sides (`If-*` on the destination, `x-ms-source-if-*` on the
source, `x-ms-source-lease-id` for the source lease). Undelete puts a path back under the name it was deleted with and refuses
(`409 PathAlreadyExists`) when that name, or the name of anything that was below it, is live again. Soft-deleted entries older than the retention are dropped at the next delete.

### Append / Flush

`PATCH ?action=append&position=N` stores the body at offset `N` in a pending area (any order, the SDK
uploads chunks in parallel) and answers 202. `?action=flush&position=L` commits it: the pending data
from the file's current length up to `L` must be contiguous (else `400 InvalidFlushPosition`), the new
length is `L`; with `retainUncommittedData=true` data beyond `L` stays pending, otherwise it is dropped.
Flush always gives the file a new ETag and Last-Modified (a flush at the current length with nothing
pending only does that). Data appended below the current length overwrites. `flush=true` on an append
flushes right after it.

### Auth

Like Blob (`docs/services/blob.md`): signatures are checked for `devstoreaccount1` through
`azemu_core::storage::sig` (Shared Key, account / service / user delegation SAS; for a directory SAS
`sr=d` the signed resource is the first `sdd` path segments), SAS content is honoured for every account
(expiry, start, protocol, IP, stored policy, resource type, permissions). Data Lake letters:
`m` move (rename accepts `m`, `c` or `w`), `e` execute (read or get status accept `r` or `e`), `o`
(change owner or group), `p` (change permissions or ACL; a service SAS without them gets
`AuthorizationPermissionMismatch`). With no credentials the answer is `401 NoAuthenticationInformation`
as JSON (reads of paths in a container with public access are allowed).

## Known differences from Azure

* The hierarchical switch is per container and is flipped by the first Data Lake request (see above);
  a real account is hierarchical as a whole.
* Create over an existing path overwrites it (a directory keeps its children). Rename needs the
  destination's parent to exist (`404 RenameDestinationParentPathNotFound`), unlike a create. The codes
  for a few unrecorded cases come from the REST documentation, not from recordings: `PathConflict`,
  `DirectoryNotEmpty`, `InvalidFlushPosition`, `SourcePathNotFound`, `SourceConditionNotMet`,
  `RenameDestinationParentPathNotFound`.
* `x-ms-lease-renewed` of `auto-renew` is always `false`.
* A listing page that is full and ends on a directory also carries a continuation token (as recorded);
  a recursive ACL change continues only while entries are left.
* Soft delete follows the account's delete retention policy (service properties on the blob
  endpoint), as in Blob. With it off a delete is permanent (`x-ms-delete-type-permanent: true`); the
  real accounts in the recordings had it on, so the replay does not compare that header there (rule `delete type of an account the recording did not set up` in `tools/replay/README.md`).
* An append with `flush=true` takes the request's `Content-Type` as the file's content type when the file has none of its own (two recordings).
* Owner and group are `$superuser` whoever calls (no Azure AD identity is available).
* Expired paths (`x-ms-expiry-time`) disappear lazily on the next request that looks at them.
* `Content-MD5` / CRC64 of a flushed file are not computed.
* Container sessions are not built; neither is Blob's Rename Container. Quick query is built for CSV and JSON (`docs/services/blob-query.md`); a `dfs` POST with `comp=query` is answered like the blob one, which Azure documents only for the blob endpoint.
* Append Data hashes its body (`Content-MD5`, `x-ms-content-crc64`) before the state lock is taken, and
  a read is joined after it is released, like the Blob endpoint.
  One exception is left: Flush Data with appended pieces that overlap what is already committed (an
  out-of-order or rewriting append) copies the whole file while the lock is held.
* A `position` whose end (`position` plus the body) does not fit in 64 bits is `400 InvalidQueryParameterValue`,
  and an `x-ms-expiry-time` that is too far away to be a date is `400 InvalidHeaderValue`.

## Keeping data between runs

Data Lake shares its state with Blob: the files are written by the `blob` service (see "Keeping data between runs" in `blob.md`); the `dfs` service
has none of its own. Directories, owner, group, ACLs (access and default), permissions, sticky bit, expiry, soft-deleted paths and the data of files are
in the Blob files. An append that was not flushed is kept too.

## Other notes

* **Delete File System** is Delete Container: it is refused with `409 ContainerHasLegalHold` / `ContainerImmutabilityPolicyLocked` /
  `BlobImmutableDueToPolicy` while a path (blob) in it is protected; see `blob.md`, "A container with protected blobs cannot be deleted".
* **Versioning**: an account that appears on first use has none (`AZEMU_BLOB_VERSIONING`, off by default); Data Lake writes never made versions.
* `POST /_azemu/reset?account=<name>` empties the storage account for Blob and Data Lake together.
* The Data Lake service asks core to drop a handler when the client goes away only for `GET`.
* A 501 is named `Blob_Query (Parquet input)` / `DataLake_Unknown (PATCH action=...)` in `GET /_azemu/unsupported`.
