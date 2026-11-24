# Azure Files (`file`)

**This is the REST API of file shares, and only that.** SMB and NFS are protocols of the operating system: a share cannot be mounted from this emulator (no `mount -t cifs`,
no drive letter), and nothing listens on port 445 or 2049. What works is what the storage SDKs and tools call over HTTP: `azure-storage-file-share` (Python),
`Azure.Storage.Files.Shares` (.NET), `azfile` (Go), the Java and JavaScript clients, and `az storage file` / `azcopy` where they use the REST API.

**What it emulates.** Shares, directories and files: create, properties, metadata, delete, listing, ranges (put, clear, from a URL, list, and the difference to a snapshot),
downloads by range, SMB properties (attributes, three times, permission keys), leases, snapshots, soft delete and restore of shares, copy and rename, stored access
policies and SAS, service properties (metrics, CORS, the SMB setting). Crate `azemu-file`, standalone binary `azemu-file`.

**How to reach it.** `http(s)://{account}.file.core.localhost:4577/{share}/{path}` (any account name works without being created first). With `--standard-ports`
(file is on **10004**): `http://azemu:10004/{account}/{share}/{path}`, the Azurite style. A share must be created before it is used.

**Not built.** Symbolic links, hard links and the other operations of NFS shares (they answer `501`; an NFS share, `x-ms-enabled-protocols: NFS`, is refused as a standard
account refuses it), copying from a blob into a file, a file's structured-body upload (`x-ms-structured-body`, the checksums of the newest SDKs), the paging of List Ranges,
provisioned and paid-bursting settings of premium shares, handles (there never are any), Entra ID roles (a bearer token is taken as it is, see below), events.
Shares made through Resource Manager (`fileServices/default/shares`) are shares here, and the other way round: see [Resource Manager](#resource-manager).

| | |
|---|---|
| Service name | `file` |
| Gateway URL | `http://{account}.file.core.localhost:{port}/{share}/{path}` (also `https://`, same port) |
| Standard port | 10004 (`--standard-ports`) |
| Pinned URL (Azurite style) | `http://127.0.0.1:{port}/{account}/{share}/{path}` (`--pin file=10004`) |
| Default account on a pinned port | `devstoreaccount1` |
| Accounts | any name works without being created first; state is kept per account |
| State | in memory, and on disk too with `--data-dir` (see below); `POST /_azemu/reset` drops everything, `POST /_azemu/reset?account=<name>` one storage account |
| Setting | `AZEMU_FILE_SOFT_DELETE_DAYS` (default 7; 0 turns soft delete off) |

## Pointing a client at it

```python
from azure.storage.fileshare import ShareServiceClient

conn = ("DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;"
        "FileEndpoint=http://devstoreaccount1.file.core.localhost:4577")
service = ShareServiceClient.from_connection_string(conn)
share = service.create_share("docs")
share.get_file_client("a.txt").upload_file(b"hello")
```

The host has `.core.` in it on purpose: the Python storage SDK reads any host with `localhost` and no `.core.` as an Azurite address with the account in the path. With
`--standard-ports` use `FileEndpoint=http://azemu:10004/devstoreaccount1`. Go: `service.NewClientWithSharedKeyCredential("http://devstoreaccount1.file.core.localhost:4577", cred, nil)`.
What each SDK needs set is in [`docs/sdk-notes.md`](../sdk-notes.md).

## Operations

Everything here was tried against a real storage account in November 2026 (a standard account, SMB shares) unless the section "Behaviour that could not be confirmed" says
otherwise.

| Group | Built |
|---|---|
| Service | Get and Set Service Properties (metrics, CORS, `ProtocolSettings/SMB/Multichannel`; `Logging` is refused as Azure refuses it), List Shares (`prefix`, `marker`, `maxresults`, `include=metadata,snapshots,deleted`), Get User Delegation Key (answered like Queue does) |
| Shares | Create (quota, access tier, metadata), Get Properties (also of a snapshot), Set Properties, Get and Set Metadata, Get and Set Access Policy (up to 5), Get Statistics, Snapshot (create, read through `?sharesnapshot=`, delete), Delete (`x-ms-delete-snapshots: include` and `include-leased`), Lease (acquire, renew, change, release, break; also on a snapshot), Restore, Create and Get Permission |
| Directories | Create, Get Properties, Set Properties, Get and Set Metadata, Delete (only when empty), List Files and Directories (`prefix`, `marker`, `maxresults`, `include=Timestamps,ETag,Attributes,PermissionKey`, `x-ms-file-extended-info`), Rename, List Handles, Force Close Handles. The root of a share is a directory with an empty path |
| Files | Create, Get (ranges, `x-ms-range-get-content-md5`), Get Properties, Set Properties (also resize), Get and Set Metadata, Delete, Put Range (update and clear, `Content-MD5`), Put Range From URL (from a file in this emulator), List Ranges (and the difference to a snapshot with `prevsharesnapshot`), Copy File (from a file of this emulator, from a snapshot), Abort Copy, Rename, Lease (acquire, change, release, break), List Handles, Force Close Handles |
| Credentials | Shared Key, Shared Key Lite, account SAS, service SAS (`sr=s` and `sr=f`) with stored access policies, bearer tokens (taken as they are) |
| Limits and names | share names (3 to 63 characters), the reserved characters and device names of directories and files, 255 characters a name, 2,048 a path and 250 levels deep, 4 MiB a Put Range (refused before the body is read), 4 TiB a file, 8 KiB of metadata, quota 1 to 102,400 GiB |

### Behaviour that is modelled

- **Case.** Names are case-insensitive and keep the case they were created with. A trailing dot is dropped unless the request has `x-ms-allow-trailing-dot: true`, a
  backslash separates names like a slash.
- **Pages.** Content is kept in pages of 512 bytes, as the range lists show: a range that holds data is whole pages, cut at the size of the file. A clear that zeroes a whole page
  removes it; a clear of part of a page leaves the page. Only the pages that exist are looked at, so a file declared as 4 TiB with nothing in it costs nothing to read, to clear or to list.
- **Reads are sent in parts.** Azure documents no cap on the length of one Get File range, so the emulator has none either (the size of the file, 4 TiB at most, is the only one). A read
  of 1 MiB or more is sent in parts of 512 KiB as the client takes them: stored pages are joined into a part, a hole in a sparse file is sent as zeros without being made, and
  the memory used is one part plus a list with one entry for each stored page in the range (nothing for a hole). A client that goes away stops the sending. The data is the file as it
  was when the request was served. A read with `x-ms-range-get-content-md5` is at most 4 MiB, as Azure's is, and is not streamed. A range written with the largest number
  (`bytes=0-18446744073709551615`) is read up to the end of the file, and a write or a clear with it is refused as a range past the end (`416 InvalidRange`) or a range over 4 MiB
  (`400 InvalidHeaderValue`).
- **Listings.** Files come first, then directories, each in name order without regard to case; the marker is the name of the next entry (`1!8!Yy50eHQ-` is `c.txt`), also accepted
  as a plain name. A share's marker is `/{account}/{name}`.
- **SMB properties.** Attributes (`ReadOnly | Hidden | Archive | None | ...`, in any case, joined by `|`), creation, last-write and change times (ISO 8601 with seven digits), a
  permission key. `preserve` keeps, `now` takes the time of the request (Create, Copy and Rename only: Set Properties refuses it), `inherit` takes the parent's permission.
  A file made with attributes `None` gets `Archive`; Set Properties with `None` stores `None`. A directory's attributes must hold `Directory` (`None` means `Directory`).
- **Permissions.** Create Permission and the `x-ms-file-permission` header store a security descriptor under a key; Get Permission returns it. Nothing reads the descriptor:
  there is no access control by owner, group or ACL. The keys of the default permission are the ones in Microsoft's recordings.
- **Leases.** A share lease can be taken for 15 to 60 seconds or without end, and breaks over the break period. A file lease has no renew and breaks at once. A lease
  protects what changes the share or the file (a read of a file needs no lease ID; a share's reads do check an ID that is given). A fixed lease that ran out answers `LeaseLost` when its ID is used.
- **Snapshots.** A snapshot keeps the tree, the content and the properties of the moment, and is read through `?sharesnapshot=`. Everything that changes a snapshot (except
  its lease) is `400 InvalidQueryParameterValue`. The time of a snapshot is whole seconds.
- **Soft delete.** A deleted share is listed with `include=deleted` for 7 days (`AZEMU_FILE_SOFT_DELETE_DAYS`), with `DeletedTime` and `RemainingRetentionDays`, and comes
  back with Restore (`x-ms-deleted-share-name`, `x-ms-deleted-share-version`).
- **Copy.** A copy is done when it answers (`x-ms-copy-status: success`), so there is never one to abort (`NoPendingCopyOperation`). The copy gets the attributes asked for plus
  `Archive`, times of now, the permission of the source, and the metadata of the source unless the request sends some.
- **Rename.** `restype` is ignored: what the source is decides. The source is the URL of the file or directory (`x-ms-file-rename-source`).
- **Faults.** [`docs/faults.md`](../faults.md) rules answer in the shape of Storage (`503 ServerBusy` and the rest).

## Authentication

The same checks as Blob, Queue and Table (the README's "Credentials are checked loosely"): the form of a Shared Key signature and of a SAS is checked for every account,
the signature itself only for `devstoreaccount1`, `AZEMU_LOOSE_AUTH=1` turns the signature checks off. A SAS is judged by what it says for every account: start, expiry, protocol,
address, the services and resource types of an account SAS, permissions, and (through `si`) the stored access policy of the share.

- **Service SAS.** `sr=s` covers the share, `sr=f` one file. The string to sign has no `sr`: `sp, st, se, /file/{account}/{share}/{path}, si, sip, spr, sv, rscc, rscd, rsce, rscl, rsct`
  (the Python and .NET SDKs sign it so). `sr=s` permissions are `rcwdl`, `sr=f` `rcwd`. Operations on the share itself (properties, ACL, snapshot, lease) and on the
  service need an account SAS (`ss=f`, `srt=s` for the service, `c` for shares, `o` for directories and files) or a key.
- **User delegation SAS.** The key is given, a SAS made with it is accepted without comparing its signature (the layout for Azure Files is not known here).
- **Bearer tokens** are not checked and need no `x-ms-file-request-intent`: Azure answered the same with and without it for the main service principal, the reader and the
  one without a role (only the role decided). In `--strict-auth` this service is **not checked**; the start-up log says so.
- **No credentials:** `401 NoAuthenticationInformation` with the challenge from `x-ms-version` 2019-12-12 on, `404 ResourceNotFound` before it.

## Known differences from Azure

- **Nothing mounts.** See the first lines.
- **A deleted share's name can be used at once.** Azure answers `409 ShareBeingDeleted` for about half a minute after a delete (creating the same name, restoring it).
- **Tier changes are not finished.** After Set Properties with a new tier the share reports `AccessTierTransitionState: pending-from-<old>` for good; Azure moves the data in the
  background and then drops the field.
- **Usage.** Get Statistics counts the sizes of the files (not the allocated pages, and not rounded up to whole GiB).
- **Quota is not enforced.** A share can hold more than its quota.
- **No handles.** List Handles answers an empty list and Force Close Handles closes none, as for a share nobody has mounted.
- **Names of metadata are lower-cased.** The HTTP stack hands header names over in lower case, so `x-ms-meta-Foo` comes back as `x-ms-meta-foo` (Azure keeps the case).
- **Error bodies** write `'` as `&apos;`; Azure leaves it as it is. Same XML, different bytes.
- **A copy from a blob** (and a file that Blob holds) is `501`: it would need the Blob service to hand over its content.
- **Rename without the source header** is `400 MissingRequiredHeader` here; Azure answers `500 InternalError`, as it does for a source that is not a URL (here `400 InvalidHeaderValue`).
- **`x-ms-enabled-protocols`** with a word other than `NFS` makes an SMB share, as it did on Azure (`FTP` gave `201`).
- **The default permission keys** and the SDDL that Get Permission returns for them are fixed values; Azure's are made from the account.
- **File ids** are numbers made from a counter; Azure's look the same but are not repeatable.

## Behaviour that could not be confirmed against Azure

- A directory made with a permission key answers with the key of the descriptor as it applies to the directory, a key that Azure derives from the SID of the account. Here the key
  is the one that was given.
- What Get Permission returns for a descriptor: Azure added `S:NO_ACCESS_CONTROL` to the one tried (copied when there is no `S:` section); other changes it may make are not known.
- The `Binary` permission format is accepted when the value is base64 of more than 20 bytes; it is stored as given and not read.
- `x-ms-file-last-write-time: now` on Put Range, and `x-ms-file-change-time` on Create: taken as the REST reference words them.
- Shares of a premium or provisioned account: the provisioned-IOPS and bandwidth headers are ignored, `Premium` is refused as a standard account refuses it.
- The exact error of a path that is not valid when it also names a missing parent: names are checked first (as Azure did for the cases tried).
- User delegation SAS for Azure Files (see above).
- Put Range with `x-ms-file-last-write-time: preserve` on a file whose last-write time was never set.

## What Azure answered where it surprised

All in November 2026, on a general-purpose v2 account.

- `x-ms-file-attributes: None` on Create File gives `Archive`; `Hidden|None` is refused; `Hidden` alone is refused for a directory.
- Set Properties refuses `now` for the creation and last-write times; Create takes it.
- Rename: `x-ms-file-rename-source` must be the URL of the source (a path is `InvalidHeaderValue` for `x-ms-copy-source`); `restype=directory` is ignored; a directory can be moved over
  a file (with `x-ms-file-rename-replace-if-exists: true`) but never over a directory (`400 RenameDirectoryOperationNotSupported`); into itself it is `409 RenameCycle` (names are compared as the tree keeps them, with Unicode case folding: `Ä` and `ä` are one name, and a failed rename leaves the node where it was); another share
  is `400 RenameAcrossAccountsAndSharesNotSupported`.
- The Python SDK's `include=permission-key` for a listing is `400`; the name Azure takes is `PermissionKey`.
- A Put Range over 4 MiB is `413 RequestBodyTooLarge` with `MaxLimit`; a clear of more than 4 MiB is fine.
- A file lease: `renew` is `InvalidHeaderValue`, `break` ignores the break period and answers `x-ms-lease-time: 0`.
- `Premium` as an access tier and `x-ms-enabled-protocols: NFS` are `InvalidHeaderValue` on a standard account.
- A copy takes `Archive` and loses the other attributes of its source.
- The share listing puts a snapshot before its share. A deleted share keeps its lease state in the listing.
- The path of the URL is checked for characters, length and depth before anything is looked up, on every operation, also reads.
- A share name that is not valid is `OutOfRangeInput` when its length is wrong and `InvalidResourceName` for its characters.
- Set Properties with no header at all is a change: the ETag changes.
- `GET /{share}` with no `restype` is `400 InvalidUri` with `UriPath`; for a directory or file path an unknown `comp` is reported as an unknown `restype` with the value of `comp`.
- A symbolic link request on an SMB share: Azure drops the connection (here `501`).

## Resource Manager

A share made with `Microsoft.Storage/storageAccounts/fileServices/shares` is a share of this service: ARM creates it here (`PUT /{share}?restype=share` with the quota, the access tier and the metadata), sets them
again when it already exists, and deletes it (the delete of the account or the group removes the account's shares, the soft-deleted ones too). A share made with the file API is listed and read by ARM, and its
quota, tier, metadata and times in ARM are the ones of this service. Details and the shapes are in [`arm.md`](arm.md#file-shares); `azurerm_storage_share` works in both forms (`storage_account_id` and
`storage_account_name`), also with `azurerm_storage_share_directory` and `azurerm_storage_share_file` (`tests/terraform/cases/shares`).

The retention of deleted shares in `fileServices/default` (`shareDeleteRetentionPolicy`) is stored and returned by ARM, and does not reach this service: soft delete here is one setting for every account,
`AZEMU_FILE_SOFT_DELETE_DAYS`. A change of it through ARM changes nothing here. Not tried: NFS shares and the premium settings (`rootSquash`, provisioned sizes) through ARM.

## Keeping data between runs (`--data-dir`)

State is kept in `file/file.state` (kind `file`, version 1): per account the service properties, the shares with their properties, metadata, policies, leases, snapshots and
the whole tree with the content of every file, the deleted shares and the stored permissions. A save writes the whole file (file content is not split into pieces as Blob's is),
so a big share makes a big write. Times are absolute: a fixed lease and a soft-deleted share run out on schedule across a restart. See [`persistence.md`](persistence.md).

## What was tried

- `tests/sdk-python/file/`: 152 tests with `azure-storage-file-share` 12.27.1 and raw REST requests, run on the emulator and on a real account (`AZEMU_AZURE=1`, see
  `tests/sdk-python/README.md`).
- `tests/sdk-go/file/`: the main paths with `azfile` 1.7.2.
- `crates/services/file/tests/`: every operation and error, the state file and a version-1 fixture.
- [`tools/replay`](../../tools/replay/README.md): Microsoft's recordings of the Python SDK (679 recordings), score in [`fidelity/SCORES.md`](../../fidelity/SCORES.md).
- Rust: Azure's crate for file shares, `azure_storage_files_shares`, is a name that holds a placeholder (0.0.1, no library); there is nothing to test with.
