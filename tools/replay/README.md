# Replay tool

Replays real-Azure recordings against the emulator and compares each answer with what
Azure returned. It is one of the ways the emulator is checked against real Azure behaviour (the SDK suites in `tests/` are the others). Python
standard library only (pytest for the tests).

Files in this directory:

| File | What it is |
|---|---|
| `replay.py` | the tool |
| `valuecheck.py` | the comparison of values (headers, body values, error texts) and the rules that leave a value out |
| `run_all.py` | runs every package whose service the emulator has mounted, writes `fidelity/reports/*.json` and `fidelity/SCORES.md` |
| `mockserver.py` | a fake emulator that answers with the recorded responses; used to test the tool |
| `test_replay.py` | pytest tests (`.venv/bin/pytest tools/replay -q -p no:cacheprovider`) |

## How to run it

One package, emulator already running:

    .venv/bin/python tools/replay/replay.py \
        --recordings refs/recordings/python/sdk/storage/azure-storage-queue \
        --gateway http://127.0.0.1:4577 --report /tmp/queue.json

Everything (starts the binary on a free port for each package and stops it afterwards):

    .venv/bin/python tools/replay/run_all.py --start-bin target/debug/azemu
    .venv/bin/python tools/replay/run_all.py --gateway http://127.0.0.1:4577   # emulator you started

`run_all.py` asks `GET /_azemu/health` which services are mounted and runs only those
packages (blob -> azure-storage-blob, queue -> azure-storage-queue, table ->
azure-data-tables with `--exclude cosmos`, keyvault -> the three keyvault packages with
`--exclude mhsm` for keys, appconfig -> azure-appconfiguration, eventgrid, servicebus,
datalake -> azure-storage-file-datalake, arm -> all the `azure-mgmt-*` packages). Packages whose
service is not mounted are listed as "not run". It prints one table and writes the same table to
`fidelity/SCORES.md` with the date and the binary path.

### Which settings each package needs

With `--start-bin` the emulator is started again for every package, with the settings that package
needs and with no `AZEMU_*` variable of your shell (a `AZEMU_LENIENT=1` in your shell would change
scores). `Package.env` in `run_all.py` is the list, `fidelity/SCORES.md` prints it, and a test
(`TestPackageSettings`) checks it. The emulator behaves as Azure does by default (nothing is created by being named, blob versioning is off); the recordings
were made in accounts that were set up before the recording, so these packages need the matching switch:

| Package | Setting | Why |
|---|---|---|
| blob | `AZEMU_BLOB_VERSIONING=1` | the recordings come from storage accounts with blob versioning turned on: `x-ms-version-id` answers, versions in listings, deletes that keep a version. Without it a new account has versioning off, as on Azure |
| eventgrid | `AZEMU_EVENTGRID_AUTOCREATE=1` | the recorded publishes go to topics that were made outside the recordings (in the Azure portal); without auto-creation they answer 404 and the score falls from 100% to 8% |
| every other package | none | queue, table, key vault, app configuration, Service Bus (management API), Data Lake and Resource Manager recordings create what they use. Data Lake was measured with and without `AZEMU_BLOB_VERSIONING=1`: the same numbers and the same values (94.79 / 97.28 then; 95.03 / 97.52 since the replay keeps the order of the seconds, see "Seconds" below) |

Not `AZEMU_LENIENT` for everything: it would also turn on Service Bus, Event Hubs and Event Grid
auto-creation for packages that must not have them, and a score would then hide which convenience it needs.
With `--gateway` the emulator is yours: start it with the setting of the package you replay, and the tool
prints a note.

Useful `replay.py` options: `--filter SUBSTR` / `--exclude SUBSTR` (file name), `--limit N`,
`--verbose` (print every failing entry), `--report FILE` (JSON), `--dry-run` (build every
request, send nothing), `--no-classify` (see below), `--no-time-shift`, `--no-reset`,
`--default-auth`, `--label`, `--strict` (non-zero exit on failure).
Use a free port for your emulator; do not use 4577, 10000-10002, 8081 or 5672 for tests.

## Hard, soft, unreplayable, ahead-async

Each recorded entry is sent to the emulator (the recorded `RequestUri` path and query are
sent byte for byte; the `Host` header is `<account>.<label>.localhost:<port>`) and gets
exactly one result class:

| Class | Meaning |
|---|---|
| `pass` | no hard failure |
| `fail` | a hard check failed |
| `unreplayable:auth-dependent` | not sent; see rule below |
| `unreplayable:sanitized-copy-source` | not sent; see rule below |
| `unreplayable:throttled` | not sent; see rule below |
| `unreplayable:azure-server-error` | not sent; see rule below |
| `unreplayable:generated-key` | not sent; see rule below |
| `unreplayable:leftover-state` | sent; the failure is explained by state the recording never created |
| `ahead-async` | sent; Azure finished later than the emulator |

The recordings replace the customer-provided key headers (`x-ms-encryption-key`, `-key-sha256`) with `Sanitized`; the emulator checks that the key, its hash and the
algorithm agree, so the tool sends one fixed real key and its hash instead. The `Host` it sends is always `.localhost`, which the emulator's host check serves,
so no `--allow-host` is needed.

Entries whose host is not one of ours (`login.microsoftonline.com`, `*.file.core.windows.net`, ...) are
skipped and counted as "skipped (host not ours)". `management.azure.com` is ours only in the `azure-mgmt-*`
recordings (see "Resource Manager recordings" below): the data-plane recordings hold a few requests to it (a test that
makes its storage account) that stay skipped, so the score of those packages does not change.

**Hard checks** (these make an entry fail):
1. the HTTP status code equals the recorded one;
2. the `x-ms-error-code` header equals the recorded one, when the recording has one;
3. the error code in the body equals the recorded one (JSON `error.code`, `odata.error.code`,
   problem+json `type`, XML `<Error><Code>`), when the recording has one (status >= 400 only).

**Soft checks** (reported, never fail an entry): response headers present in the recording
but missing in the answer (minus a list of noise headers, `NOISE_HEADERS` in `replay.py`),
body present/absent, and body shape: element paths for XML (namespaces dropped), key paths
for JSON (arrays as `[]`).

**Values** (a third number, never fails an entry): see "Values that differ" below.

### The rules of every classifier

All classifiers are narrow on purpose. `--no-classify` switches all of them off (every
entry is sent, nothing is relabelled), which gives the raw behaviour.

**unreplayable:auth-dependent** (not sent). The recorded status is >= 400 and either
(a) the recorded error code is one of `AuthenticationFailed`, `InvalidAuthenticationInfo`,
`NoAuthenticationInformation`, `InvalidToken`, `InvalidAuthenticationToken`,
`ExpiredAuthenticationToken`, `AuthorizationFailure`, `AuthorizationFailed`,
`AuthorizationPermissionMismatch`, `AuthorizationServiceMismatch`,
`AuthorizationResourceTypeMismatch`, `AuthorizationProtocolMismatch`, `invalid_token`; or
(b) `CannotVerifyCopySource` with status 401 (404/409/412 with that code are replayable:
missing source, public access off, failed source condition); or (c) the recorded body
contains the word "audience" (wrong token audience). Why: the request that caused it
carried a credential that the recording no longer contains. NOT covered: the Key Vault
`401 Unauthorized` challenge (AKV10000, "missing a Bearer token"); it is what a request
without a credential gets and is replayed as recorded (no credential added).

**unreplayable:sanitized-copy-source** (not sent). A blob request has
`x-ms-copy-source: Sanitized` (all 293 copy sources in the corpus are that literal) and no
candidate source exists. The candidate is the blob most recently written by an earlier
successful entry of the same recording (Put Blob, Copy Blob / Put Blob From URL, Put Block
List, Put Page, Append Block; recorded 2xx) that is not the destination of this request. If
there is one, the header is rewritten to its URL on the emulator
(`<scheme>://<account>.blob.localhost:<port>/<container>/<blob>`) and the entry is replayed
normally; if there is none, the entry is not sent. (Blocks staged with Put Block do not count
as writes, because they are not readable blobs.) The same placeholder in
`x-ms-copy-source-authorization` becomes `Bearer sanitized`.

**unreplayable:throttled** (not sent). The recorded status is 429. Azure throttled the
request and the SDK retried it; all 431 in the corpus are App Configuration
(`too-many-requests`) and each is followed by the same request again (checked: 431 of 431).
The emulator never throttles and would apply the request, so the recorded retry would then
see a changed state. Not sending it makes the retry pass on its own. Other 5xx statuses are
not treated this way (the corpus has none).

**unreplayable:azure-server-error** (not sent). The recorded status is 500, 502, 503 or 504: Azure itself failed
for a reason the recording does not show (all 8 in the corpus are `GET /providers/Microsoft.EventHub/operations`
answered 500 in the `azure-mgmt-eventhub` recordings). There is nothing to compare an answer with. 501 is a real
answer ("not implemented") and is sent.

**unreplayable:generated-key** (not sent). Key Vault `decrypt` or `unwrapkey`, recorded 2xx, on a key that an earlier
entry of the same recording made the vault generate (`POST /keys/{name}/create` or `.../rotate`, recorded 2xx, same vault), when the
`value` the tool is about to send is still Azure's own ciphertext. Azure's ciphertext was made for Azure's key and the emulator generates
another one, so it can never decrypt it to what Azure gave. The emulator answers 400 for most and 200 for some (measured: 3 to 4 of
these entries, all `RSA1_5`, changed between two runs, never the same ones; the RSA library does implicit rejection of bad PKCS#1 v1.5 padding, so the answer
depends on whether the ciphertext is smaller than the random modulus of the key the emulator generated that run: read in its source, not run), so the score used to move
by a few entries from run to run. Not covered, and sent as before: an `encrypt` or `wrapkey` answer just before gave the ciphertext (the value
mapping has put the emulator's own ciphertext into the request, so the emulator can decrypt it: 72 entries pass that way), a key that was imported (`PUT /keys/{name}`),
and `encrypt`, `wrapkey`, `sign`, `verify`, which work with any key. 336 entries of the 800 recorded decrypts and unwraps in the Key Vault keys recordings.

**unreplayable:leftover-state** (sent, relabelled afterwards). All of: the entry is a create
request (Create Container, Put Blob / Copy Blob, Create Queue, Create Table (`POST /Tables`),
Key Vault set/create/import of a secret, key or certificate), the recorded status is 409 with
an error code ending in `AlreadyExists` or equal to `Conflict`, the emulator answered 2xx,
and no earlier entry of the recording (other than a recorded 401) concerns the same
resource path or table. Why: the real account already held the resource. Nothing else about
leftover state is covered; those stay failures. (Key Vault `restore` is not a create request
here: its 409 is a different thing, see known limits.)

**ahead-async** (sent, relabelled afterwards). All of: the entry is a `GET` recorded 404 and
the emulator answered 2xx; the same method and the same recorded URL occurs LATER in the same
recording with a 2xx; no write (anything but GET/HEAD/OPTIONS) to the same resource lies
between the two; and an earlier 2xx `DELETE`, or `POST .../recover`, of the same resource
exists in the recording (Key Vault: `deletedsecrets/x` and `secrets/x` are the same
resource). That is the Key Vault delete/recover polling (`404, 404, ..., 200`). The later
2xx entry must be in the same file, so a real "should be 404" check that never turns 200 stays
a failure. For Resource Manager the rule is a second one, because its polls are 202: the entry is a `GET`
recorded 202 (the operation was still running), the emulator answered 2xx (it finishes every operation at
once), the same URL occurs later in the recording with a 2xx, and an earlier write (PUT, PATCH, POST, DELETE)
of the recording was answered 200, 201, 202 or 204 (it started the work). Not covered: certificate operation polls (`inProgress` then `completed`) and
snapshot provisioning polls: they are 200 on both sides, so no hard check can see them.

### The two sets of numbers

The summary prints both and labels them:

* **RAW**: every entry that is not a pass counts against; a recording passes only if every
  entry passed. Classified entries are not passes.
* **ADJUSTED**: `unreplayable:*` entries leave the denominator; `ahead-async` counts as
  passing. A recording is "fully passing (adjusted)" when it has no hard failure left after
  classification. A recording where nothing was verified at all (every entry unreplayable) is
  also counted, and the summary says how many such recordings there are
  (`[N of them have no verified entry at all]`).

The JSON report has, per entry, `class` and `class_reason`, and per recording `result` (raw)
and `result_adjusted`. The class counts are in `summary.classes`.

## Values that differ
The hard checks look at the status and the error code. That hides what the answers say: comparing the values of the answers with the recorded ones finds small real
differences (for example the spacing in Table's `Content-Type`, `x-ms-delete-type-permanent`, the wording of Key
Vault's 401). The comparison is part of the tool (`valuecheck.py`).

For every answer whose status matched the recorded one (and whose error codes matched), the tool compares:

* the value of every **response header** that is in both the recording and the answer (case and surrounding blanks do not
  count; anything else, such as a space after a semicolon, does);
* the value of every **scalar of the body** that is in both: a string, number or boolean of a JSON body (null is left out), the
  text of every XML element, by path;
* the **first line of every error message** (a path ending in `message`, and the `Detail` of a Service Bus error), so a changed
  wording is found and the request ids and times that follow it are not. Before the comparison the first line loses what differs
  by design: the host of an address, the name of the account (and the sanitizer's word for it), an id of 32 hex digits, a GUID
  (tracking ids, with their `_G4` gateway suffix), a `Timestamp:` and an upper-case host name (Event Grid names the topic's address in capitals); the namespace in front of `:Topic:` or `:Queue:` also goes.

A header or key that only one side has is the soft check's business, not this one's. An address is compared without its
scheme, host and query string (the emulator's host differs by design; what is left of the path is compared), and a path
segment of 32 hex digits or a GUID (a version or an id the server makes up, as in a Key Vault `id`) counts as the same id.

Three numbers per package come out: values compared, values that differ (with the percentage), and the entries that
have one or more. The differences are **grouped like the hard failures**: by operation pattern, status, and the header
name or the path of the value (array positions folded: `/value[]/name`), most frequent first, each group with one example of
both values. They are in the summary (`== Values that differ, grouped ==`), in `fidelity/SCORES.md` (the top six per package) and,
all of them, in the JSON report under `value_groups` (per entry under `value_diffs`).

**What the number is not.** Most differences are properties of Microsoft's test account in the recording (logging and
metrics settings, access tier, the list of regions of the subscription) or a state that Azure had and the emulator
finishes at once (`Creating` against `Succeeded`). The score is not a percentage to push to zero; it is a list to read.

### The rules that leave a value out

A value that the server makes up cannot be the same twice, so it is not compared. Every rule is listed here with its
reason, is kept narrow on purpose (a broad rule hides exactly what this check is for), and looks at the **recorded** value
only: a rule that also looked at the emulator's value would excuse a plain word that the emulator answers with an
id. A test (`test_the_readme_lists_every_rule_and_header_group`) fails when a rule or a header is in the code and not here.

**Body values** (`VALUE_EXCUSES` in `valuecheck.py`):

| Rule | Excuses a recorded value that is... |
|---|---|
| `sanitized` | the recording's sanitizer replaced the real value (`Sanitized`, `fake...`, a run of one character, the all-zero GUID, `MA==`: the base64 of `0`, which stands for a user delegation key) |
| `account name` | the recorded value contains the account name, which the sanitizer replaced and the replay chose again |
| `date` | an ISO 8601 date and time (`2024-05-01T...`) or an HTTP date: the server's clock |
| `guid` | starts like a GUID: an id the server makes up |
| `etag` | an ETag in the forms Azure writes (`0x8D...`, `W/"..."`) |
| `big number` | a whole number of nine or more digits: a time stamp, a sequence number or an id |
| `token` | 24 or more characters of letters, digits and `+/=_-` with a digit in them, no spaces: a key, a handle, a hash or base64 (a certificate thumbprint, `x5t`, needs no digit) |
| `version` | dotted digits (`1.2.3`, `1.2.3.4`): a build or service version |
| `request id text` | text that holds `RequestId:` (an Azure error text with the request id and a time) |
| `odata datetime` | an OData date literal (`datetime'...'`) |
| `epoch fraction` | seconds since 1970 with a fraction (ten digits, a dot, digits) |
| `answers with the host's account` | the emulator's value is the account (namespace, vault, store) of the request's host, which the sanitizer replaced in the recording: the recorded value is the real name |
| `connection string` | a connection string (`Endpoint=sb://...`, `DefaultEndpointsProtocol=...`, `...SharedAccessKey=...`): it holds the server's address and a key |
| `named like its member` | the value is the name of its own member (`"primaryKey": "primaryKey"`): the sanitizer wrote the member name over a secret |
| `long` | more than 300 characters: a certificate, a key, a backup or a template |
| `retired wording` | a recorded value that older recordings carry and newer recordings of the same call do not: the service stopped writing it, so the emulator (which answers as the service does now) cannot be asked to. Only the exact values of `RETIRED` in `valuecheck.py`: the Key Vault 401 text `... a Bearer or PoP token.` (every Key Vault recording from before March 2026; from then on every api-version says `... a Bearer, PoP, or MTLS_POP token.`), the RSA-HSM public exponent `AAEAAQ` (2022 and 2023; `AQAB` since 2024), `Pending certificate not found: ...` (small c until 2023) and `hsmPlatform` `1` of an HSM key (2024; `2` since 2025) |
| `vault retention` | the recorded vault keeps deleted things for another number of days than Azure's default of 90: `recoverableDays` other than 90 and the `recoveryLevel` that goes with it (`CustomizedRecoverable...`). Microsoft's test vault was set to 7 days (this was about 80% of the Key Vault differences before the rule); a new emulated vault has the default |
| `continuation marker` | the value of a `NextMarker` or `Marker` element: an opaque text the server makes up for the next page (Azure writes `2!68!MDAw...`), which no client reads |
| `nearly never` | a duration within a second below .NET's `TimeSpan.MaxValue` (`P10675199DT2H48M5.477539S`, what the Python SDK writes back for the maximum it read as `P10675199DT2H48M5.4775807S`; the maximum itself is compared): Azure keeps the exact tick count the client sent, the emulator keeps milliseconds and answers any such value as the maximum (known difference, Service Bus) |
| `work in progress` | a state that Azure was still working towards when it answered and that the emulator finishes at once, by design (the SDK pollers must not wait): `Running`, `inProgress`, `cancelled` in a `status`; `Creating`, `Created`, `RegisteringDns`, `Updating`, `Deleting`, `Activating` in a `provisioningState`; `Activating` in the `status` of a Service Bus namespace; and the Key Vault text `Deleted Secret not found: ...` (also Key, Certificate): Azure answers it while a delete or purge is still being finished and the long text `A secret with (name/id) ... was not found in this key vault` afterwards, which is what the emulator answers once the purge is done (the replay hits both: right after a purge the recordings say the short one first) |

**Where a value stands** (`STRUCTURE_RULES`):

| Rule | What is left out |
|---|---|
| `lists matched by id` | a list whose elements all have an `id` or `kid` of their own, different from each other, is compared element by element by that id and not by position; an element only one side has is left out. A Key Vault list holds the vault's other secrets, keys and certificates too, in Azure's own order, so the n-th element of the two answers was not the same object (before the rule this made about 12,000 of the 24,000 Key Vault differences). Lists whose ids are not all different (the versions of one secret, whose addresses end in a made-up id) are still matched by position |
| `lists of different length` | the values under a list whose two sides have a different number of elements are not compared: elements are matched by position, and two lists of another length hold other things (the recorded account had resources the replay never made), so the pairs are not the same thing |
| `settings the recording never set` | the parts of an answer that only report the account's configuration are not compared in a recording that did not set that part itself with an earlier Set Service Properties: Get Service Properties of Blob, Queue, Table and Data Lake part by part (`Logging`, `HourMetrics`, `MinuteMetrics`, `Cors`, `StaticWebsite`, `DeleteRetentionPolicy`, `DefaultServiceVersion`; Set Service Properties changes the parts it names and leaves the others), and Get Service Stats. They are the recorded account's, which the emulator does not have (this was about 360 values in four packages) |
| `tier the recording never set` | `x-ms-access-tier` and `AccessTier` are not compared in a recording in which no request names an access tier (`x-ms-access-tier` in a request): the tier of a blob then is the recorded account's default (`Cool`, or `Smart`), which the emulator does not model |
| `delete type of an account the recording did not set up` | `x-ms-delete-type-permanent` of a delete is not compared in a recording that did not turn soft delete on itself, unless the recording shows versioning (an `x-ms-version-id` in some answer) and the delete is a plain delete of the current blob (no `versionid`, `snapshot` or `x-ms-delete-snapshots`): whether anything stays recoverable is decided by the recorded account's soft delete and versioning, and the replay runs every blob recording with versioning on. The emulator's own rule is in the Blob crate's tests |

**Response headers**, by group (`HEADER_EXCUSES` and `HEADER_EXCUSE_PREFIXES`). The header is not compared at all:

| Group | Why | Headers |
|---|---|---|
| made up per request | the server makes it up for each request; no two answers have the same | `client-request-id`, `dataserviceid`, `date`, `sync-token`, `x-ms-client-request-id`, `x-ms-correlation-request-id`, `x-ms-date`, `x-ms-request-id`, `x-ms-routing-request-id` |
| identity of a stored thing | an ETag, a handle, a hash or an id the server makes up when it stores something | `content-md5`, `etag`, `last-modified`, `preference-applied`, `x-ms-blob-content-md5`, `x-ms-content-crc64`, `x-ms-continuation-nextpartitionkey`, `x-ms-continuation-nextrowkey`, `x-ms-continuation-nexttablename`, `x-ms-copy-id`, `x-ms-deletion-id`, `x-ms-encryption-key-sha256`, `x-ms-lease-id`, `x-ms-or-policy-id`, `x-ms-popreceipt`, `x-ms-snapshot`, `x-ms-version-id` |
| clock | a point in time or a wait that depends on when the request ran | `retry-after`, `x-ms-access-tier-change-time`, `x-ms-copy-completion-time`, `x-ms-creation-time`, `x-ms-expiry-time`, `x-ms-immutability-policy-until-date`, `x-ms-last-access-time`, `x-ms-time-next-visible` |
| address | it holds the address of the server, which differs by design | `link`, `location`, `operation-location`, `www-authenticate`, `x-ms-copy-source` |
| transport and edge | set by the HTTP stack or by Microsoft's front end, not by the service logic | `accept-ranges`, `access-control-*`, `access-control-allow-origin`, `cache-control`, `content-length`, `expires`, `pragma`, `server`, `strict-transport-security`, `transfer-encoding`, `vary`, `x-content-type-options`, `x-frame-options`, `x-ms-ests-*`, `x-ms-keyvault-*`, `x-ms-keyvault-network-info`, `x-ms-keyvault-region`, `x-ms-keyvault-service-version`, `x-ms-ratelimit-*`, `x-ms-ratelimit-remaining-subscription-reads`, `x-ms-ratelimit-remaining-subscription-writes`, `x-ms-version`, `x-xss-protection` |
| settings of Microsoft's test accounts | the recording comes from an account whose encryption settings the emulator does not model | `x-ms-default-encryption-scope`, `x-ms-deny-encryption-scope-override`, `x-ms-encryption-scope`, `x-ms-namespace-enabled`, `x-ms-request-server-encrypted`, `x-ms-server-encrypted`, `x-ms-sku-name` |
| contents of Microsoft's test accounts | it counts what the recorded account holds, including what other tests left in it | `x-ms-item-count` |
| replay limits | depends on the size of a blob, and blobs replayed from sanitized block ids are smaller than the recorded ones (README, known limits) | `content-range`, `x-ms-blob-sequence-number`, `x-ms-content-length`, `x-ms-range` |

One thing about a header is normalised before the comparison: the random `boundary=` of a `multipart/` `Content-Type`
(`multipart/mixed; boundary=batchresponse_<guid>`) is left out; everything else of the value is compared.

Not excused on purpose, so they show up when they differ: `content-type`, `x-ms-blob-type`, `x-ms-lease-*` states, the
`x-ms-meta-*` values, `x-ms-delete-type-permanent`, access tiers, the Data Lake `x-ms-acl`, `x-ms-owner`,
`x-ms-group`, `x-ms-permissions` and `x-ms-resource-type`, and error texts.

## Resource Manager recordings (the `azure-mgmt-*` packages)

The 34 `azure-mgmt-*` recordings (Python management SDKs: Service Bus, Event Hubs, Event Grid, Key Vault, Cosmos DB, Storage,
Resources) are replayed against the emulator's ARM service as the package `arm`. How they differ from the data-plane ones:

* The host is `management.azure.com` (or, in newer recordings, the sanitizer's `Sanitized.azure.com` with a path that starts with
  `/subscriptions`, `/providers` or `/tenants`). There is no account: the replay sends `Host: management.localhost:<port>` and a
  bearer token. The sign-in requests (`login.microsoftonline.com`) are skipped as before.
* **Prerequisite groups.** The tests get their resource group (`rgname`, the sanitizer's word) from a test fixture whose requests are
  not in the recording. Without the group every answer would be `ResourceGroupNotFound` and nothing about the service could be
  compared. So before the first request the tool creates (`PUT .../resourcegroups/<name>`, location `eastus`, not scored, not in the
  report) every resource group that the recording works in and never creates with a successful `PUT` of its own. With `--no-classify`
  this is off.
* Operation patterns keep the resource types and drop the names (`PUT /subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.ServiceBus/namespaces/{name}/queues/{name}`).
* Polls: see `ahead-async` for Resource Manager above. 500 answers: see `unreplayable:azure-server-error`.
* The corpus is small: 129 entries, 16 of them sign-in requests that are skipped and 113 replayed. Most recordings hold one to
  four requests (a list of operations, a check of a name); the data comes from a few Service Bus, Key Vault, Event Grid and Cosmos DB
  flows.

## What the tool does to a recorded request

* Body: see "Recording format facts". Request headers are sent as recorded, except
  `Host`, `Content-Length`, `Connection`, `Accept-Encoding` and similar transport headers.
* Authorization: if the recording has none, a default is added for storage (`SharedKey
  <account>:replay`), Key Vault (`Bearer replay`) and App Configuration (an HMAC header),
  except for recorded 401 answers and SAS requests (`sig=` in the query). A recorded
  `Authorization` that is only a sanitizer placeholder (`Sanitized`, `Bearer Sanitized`,
  `SharedKey x:Sanitized`, ...) means "the real request had a valid credential of unknown
  kind": Key Vault, App Configuration, Event Grid and Service Bus get `Bearer sanitized`,
  storage gets the default storage authorization.
* Value mapping: values that the server generates (etags, pop receipts, snapshot and version
  ids, continuation tokens, Key Vault version ids and backup blobs...) differ between Azure
  and the emulator. For every response the tool pairs the recorded value with the emulator's
  value (same header, same body position) and rewrites the recorded value wherever it
  appears in later requests (URL, headers, body), longest value first, one pass, percent
  encoded and XML-escaped variants too. Placeholders (`Sanitized`, all-zero GUIDs, `fake...`)
  are never mapped, and a value that already appeared in an earlier request (a name the client
  chose) is not mapped. Values of 6 to 4,096 characters are mapped from headers and URLs;
  body values up to 4 MiB are mapped (Key Vault backup blobs are 5 KB to 45 KB).
* Seconds: dates on the wire have one-second resolution and a replay takes milliseconds where the recording took seconds (a test that
  sleeps 10 seconds, then writes a blob again and asks for it with `If-Unmodified-Since` = the time of the first write). Without help the
  second write lands in the same second as the first, or in the next one by chance, and the answer changes from run to run (the Data Lake
  score moved this way). `SecondGate` waits for the next second before an entry that was answered in a later second than the one before it, when
  the clock has not moved on by itself; it does so only up to the last entry of the recording that carries such a date header, and
  waits at most a second and a bit. It added about 75 seconds to the Blob replay (79 s) and about 1 second to Data Lake; nothing else waits.
* Dates: `If-Modified-Since`, `If-Unmodified-Since` and every `x-ms-*-if-modified-since` /
  `x-ms-*-if-unmodified-since` variant (`source`, `blob`, `access-tier`), plus
  `x-ms-immutability-policy-until-date` and `x-ms-expiry-time`, are moved by the difference
  between the emulator's clock and the recorded clock (taken from the first response with a
  `Date`). `x-ms-date` and `Date` are left alone. `accept-datetime` (ISO format) is not moved.
* Copy sources: see the classifier above.
* `POST /_azemu/reset` is sent before each recording (unless `--no-reset`).

## Recording format facts

Found by reading all 5,840 recordings (57,780 entries). A file is `{"Entries": [...]}`; an
entry has `RequestUri`, `RequestMethod`, `RequestHeaders`, `RequestBody`, `StatusCode`,
`ResponseHeaders`, `ResponseBody`.

* Body encodings (`RequestBody` / `ResponseBody`): `null` (no body); a JSON object, array or
  number (embedded JSON; one recording has a 5,125-digit integer, so the tool lifts Python's
  integer-string limit); a string (plain text for json, xml, `text/*` and form content types;
  base64 of the raw bytes for everything else, including an absent content type: octet-stream,
  png, avro, arrow, multipart/mixed); a list of strings (the text split into pieces; pieces
  are joined). In a list, a piece that is `b64:<base64>` is base64 of raw bytes: it is
  decoded (this is how Table `$batch` sub-requests are stored; 138 request and 132 response
  bodies). Decoded sizes were checked against the recorded `Content-Length` for every list
  body that has one: they match.
* Recorded `Content-Length` of a JSON-object body is not reliable (the recorder re-serialised
  the JSON), so it is never used.
* Sanitizer placeholders: account and vault names (`Sanitized`, `storagename`, `vaultname`,
  `fake_app_config`, `faketableaccount`, `fakeendpoint`), `Authorization: Sanitized`
  (121 table entries, 2 secrets entries), `x-ms-copy-source: Sanitized` (293, all blob),
  `x-ms-copy-source-authorization: Sanitized` (20), `x-ms-rename-source: Sanitized` (31,
  datalake, not rewritten), `x-ms-encryption-key(-sha256): Sanitized`, `x-ms-content-sha256`
  (appconfig), SAS values (`se=Sanitized`), the all-zero GUID, and block ids
  (`blockid=00000000-0000-0000-0000-000000000000`: see known limits).
* Hosts per package: blob `<acct>.blob.core.windows.net` (plus `management.azure.com`,
  `*.file.core.windows.net`, `login.microsoftonline.com`, skipped); queue `.queue.core.windows.net`;
  tables `.table.core.windows.net` and `.table.cosmos.azure.com` (the cosmos files have
  `cosmos` in their name; excluded); keyvault `.vault.azure.net`, keys also
  `.managedhsm.azure.net` (files with `mhsm` in the name; excluded); appconfig
  `fake_app_config.azconfig-test.io`; datalake `.dfs.core.windows.net` and `.blob...`;
  eventgrid `.eastus-1.eventgrid.azure.net`; servicebus; Resource Manager `management.azure.com`.
* Key Vault recordings start with a `401` challenge entry that has no `Authorization`
  header; the replay sends it without one, and sends `Bearer replay` on the rest.

## Reading the output: a worked example

    == Summary: RAW (everything that is not a pass counts against) ==
    recordings: 200 total | 192 fully passing (96.0%) | 8 with failures | 0 errored
    entries:    729 total | 723 replayed | 715 passing (98.89%) | 8 not passing | 6 skipped (host not ours)
    == Summary: ADJUSTED ... ==
    recordings fully passing (adjusted, no hard failure left): 196/200 (98.0%)
    entries passing (adjusted): 715/719 (99.44%)
    == Result classes (replayed entries) ==
        715  pass
          4  fail   e.g. tests/recordings/test_queue.pyTestStorageQueuetest_sas_signed_identifier.json#3
          4  unreplayable:auth-dependent   e.g. ...test_bad_audience_queue_client.json#2
    == Hard failure groups, after classification (2 groups) ==
         2  GET /{queue}/messages?peekonly  200->403 (->AuthenticationFailed)   [first failure in 2 recordings]
         2  PUT /{queue}  204->201   [first failure in 2 recordings]

Reading it: 8 entries are not passes. 4 were never sent (their recorded answer was an
authentication error, so nothing can be said about them) and 4 are real hard failures. The
adjusted line removes the 4 unsent entries from the denominator (715/719). Each group line is
`count  <operation pattern>  <recorded status>-><emulator status> (<recorded error code>-><emulator error code>)`.
`first failure in N recordings` counts recordings where this was the first failing entry:
the best place to start, since later failures in the same recording are often consequences.
The first group means: `GET messages?peekonly` was answered 200 by Azure and 403
AuthenticationFailed by the emulator (example file names follow the line). Then read the
`Soft` tables: they show missing headers and body-shape differences by operation.

## Tests

`test_replay.py` covers body decoding (including the `b64:` list form), byte-exact URLs on
the wire, host and label derivation, value mapping, shape comparison, grouping keys, auth
placeholders, date shifting, copy-source rewriting, every classifier (positive and negative
cases) and the aggregate/report numbers. End-to-end tests run real recordings from the
queue, blob, tables and keyvault-secrets packages against `mockserver.py` (port 0) in
perfect, mutate (server-generated values are changed on the way out and the tool must carry
the new values into later requests) and wrong-status modes.

The mock cannot tell apart recordings that begin with identical requests and carry no
distinguishing header (most App Configuration recordings), so it is not a valid perfect-mode
check for appconfig (about 43 entries mismatch); that is a limit of the mock, not the tool.

## Known limits

* Leftover state is only covered for "already exists" on a first create. Other cases stay
  failures: a first `PUT` with `If-None-Match: *` answered 412 (App Configuration), a `DELETE`
  of something another test created, queue `PUT` answered 204 because the queue existed.
* Key Vault `restore` recordings: Azure answers the first restore 409 (the purge has not
  finished) and the second 200; the emulator purges at once, so the pair shows as
  `409->200` and `200->409`. Not classified (the rule is only for GET polls).
* Sanitized block ids: Put Block requests carry the same `blockid=0000...` for every block,
  so on replay the blocks overwrite each other and the blob is smaller than on Azure
  (later `Range` reads give 416, Put Block List gives Md5Mismatch / Crc64Mismatch).
  These recordings cannot be replayed faithfully without inventing block ids.
* Responses that embed an id inside a binary body (Apache Arrow blob listings carry the
  continuation marker) cannot be mapped.
* Time-dependent recordings (lease expiry, `sleep`) replay in milliseconds, except that the order of the seconds is kept where a request
  carries a date condition (see "Seconds" above). A lease that expires after 15 seconds is still not waited for.
* Secondary-endpoint / location-mode and retry-with-failover recordings assume several hosts.
* Entries on hosts that are not ours (Azure Files, token endpoints) are skipped, so recordings that create
  resources through them lose state. (The Resource Manager host is ours.)
* Only `x-ms-copy-source` is rewritten; `x-ms-rename-source` (datalake) is not.
* "Fully passing (adjusted)" can include recordings where almost nothing was verified; the
  summary counts those.
* Shape checks compare key/element paths, never values; header checks only look for
  missing headers. Values are compared by `valuecheck.py` (see "Values that differ"), but only in answers whose
  status matched, only where both sides have the value, array elements by position (a list in another order shows
  many differences), and never the ones a rule excuses.
