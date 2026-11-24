# Python SDK tests for azemu

Tests that drive the emulator with the official Azure SDKs for Python. They are the largest suite and the quickest to run: one directory per service
(`blob`, `queue`, `table`, `datalake`, `keyvault`, `appconfig`, `cosmos`, `servicebus`, `eventhubs`, `kafka`, `eventgrid`, `storage_events`, `identity`, `arm`,
`persistence`, `seed`, `monitor`, `faults`, `schemaregistry`, `auth`, `file`). The tests were written from the SDKs and Microsoft's documentation, not from the emulator's code, and they double as examples of how to point each
Python SDK at it. What the Python SDKs need set is in [`docs/sdk-notes.md`](../../docs/sdk-notes.md#python).

## What you need

* Python 3.13 and the packages in `requirements.txt` (exact versions), for example:

  ```bash
  uv venv .venv --python 3.13 && uv pip install --python .venv/bin/python -r tests/sdk-python/requirements.txt
  # or: python3.13 -m venv .venv && .venv/bin/pip install -r tests/sdk-python/requirements.txt
  ```
* A built emulator binary: `cargo build -p azemu` gives `target/debug/azemu`; a release binary works as well.

## Run

```bash
.venv/bin/python tests/run_sdk_suites.py --bin target/debug/azemu                     # every suite, in parallel (--jobs 4 by default)
.venv/bin/python tests/run_sdk_suites.py --bin target/debug/azemu --only blob --only queue --jobs 2
AZEMU_BIN=target/debug/azemu .venv/bin/pytest tests/sdk-python/queue -q              # one suite with pytest
AZEMU_GATEWAY=http://127.0.0.1:4577 AZEMU_CA_FILE=~/.azemu/ca.pem .venv/bin/pytest tests/sdk-python/blob   # an emulator that is already running
```

Each pytest process starts its own emulator on a free port with a temporary certificate directory, so suites run in parallel and the tests of one suite share it
(they use unique names and do not reset). `AZEMU_KEEP_LOG` keeps the emulator's log. `tests/run_sdk_suites.py` prints one line per suite (passed, failed, skipped);
the Service Bus and Event Hubs suites are split per file because their tests wait on receives, and take about half an hour each on a small machine.

## What the skips mean

* The emulator does not create a queue, topic, event hub or Event Grid topic because an AMQP or HTTP client names it, as Azure does not (the Kafka endpoint does make an event hub for a producer, as Azure does, and a consumer group that was never created is accepted without being made), and blob versioning is off until
  someone turns it on. Tests about the opposite behaviour carry the markers `autocreate` and `versioning`; they are skipped in the first pass and the runner runs them in a
  second pass with the emulator's `AZEMU_*_AUTOCREATE`, `AZEMU_BLOB_VERSIONING` or `AZEMU_LENIENT` variables set (`autocreate_gate.py` has the rules). Tests that check
  what Azure answers for a missing entity (`no_autocreate`, `no_versioning`) are skipped in that second pass.
* `emulator_only` marks tests of behaviour that only the emulator has (its admin API, `/_azemu/...`), or that was not probed on Azure (`@pytest.mark.emulator_only("not probed on Azure")`: the reason is shown when it is skipped); `slow` marks tests that wait for a lock, a TTL or a schedule (a few seconds each).
* The suites can also run against another service with `AZEMU_GATEWAY` / `AZEMU_STORAGE_CONN_STR` (for example Azurite, to check an expectation against a second implementation). The
  tests skipped in that case say so in their reason ("Azurite does not implement ..."); against azemu itself they run.

## Running a suite against Azure

With `AZEMU_AZURE=1` a suite runs against a real Azure resource instead of an emulator: the `emu` fixture starts nothing, `emu.azure` is true and nothing is reset between tests.
Each service's helper module reads the address and the credential of its resource from `AZ_*` variables. Real resources cost money: run only the modules you need, and
keep the variables out of logs and out of the repository. Tests that check something only the emulator has carry `emulator_only`; tests where Azure was seen to answer
differently carry `azure_differs("reason")`. Both are skipped with `AZEMU_AZURE=1` (and run as usual without it).

### Cosmos DB (`cosmos/`)

* Variables: `AZ_COSMOS_ENDPOINT` (the account address, `https://<account>.documents.azure.com:443/`) and `AZ_COSMOS_KEY` (a primary or secondary key).
* Account: API for NoSQL, **serverless**. The suite never asks for throughput (no `offer_throughput`, no autoscale, no shared-throughput database); the tests about
  throughput are `emulator_only`, and the fixtures refuse such a request before it is sent. A provisioned-throughput account would bill every container by the hour.
* Needs nothing else. Features an account must enable first are not assumed: the tests of the "all versions and deletes" change feed mode are `emulator_only` because
  the account answers 400 until the feature is on.
* The messaging files (`test_servicebus.py`, `test_eventhubs.py`, `test_kafka.py`, `test_schemaregistry.py`) need more: `AZ_SERVICEBUS_NAMESPACE`, `AZ_SERVICEBUS_CONN_STR` (a rule with `Manage`), `AZ_SERVICEBUS_SEND_CONN_STR` and
  `AZ_SERVICEBUS_LISTEN_CONN_STR` (a send-only and a listen-only rule of the namespace), `AZ_SERVICEBUS_QUEUE_SEND_CONN_STR` (a send-only rule of the queue), `AZ_SERVICEBUS_AUTH_QUEUE` (the queue the tests use; they empty it), `AZ_EVENTHUBS_CONN_STR`,
  `AZ_EVENTHUBS_SEND_CONN_STR`, `AZ_EVENTHUBS_LISTEN_CONN_STR`, `AZ_EVENTHUBS_AUTH_HUB`, `AZ_SCHEMA_GROUP`. They use none of the helpers of the other Service Bus and Event Hubs suites (those make and delete entities on the management plane). The roles
  of the four principals for them: `main` Azure Service Bus Data Owner, Azure Event Hubs Data Owner, Schema Registry Contributor; `reader` Reader, Azure Service Bus Data Receiver, Azure Event Hubs Data Receiver; `scoped` Azure Service Bus Data Sender on the queue.
  Each successful send to an event hub leaves its event there until the retention ends (six events in all).
* What it leaves behind: nothing in the normal case. Every database a test makes is deleted after the test (by resource id, so names with `#` or `?` work too);
  a test that is killed half way can leave its database. The runs create and delete many containers (a few seconds each) and send a few hundred to a few thousand requests per module;
  the modules add up to about 25 minutes of test time (they can run in parallel, one pytest process per module; use `--timeout=300`, the default of 60 seconds is short for a real account).
* One run uses one account; do not run two runs against the same account at once if they use the same database names (names are random, so this is only a problem for
  a manual clean-up).
* `test_cosmos_query_azure_rules.py` replays about 440 queries from `cosmos_query_cases.json`, each with the rows or the error the account gave in November 2026 (about 450 queries
  and four containers of seven to twelve documents; a minute on the account). The same file runs against the emulator.

```bash
set -a; . path/to/your/env; set +a    # a value that holds a `;` (a connection string) needs the file read without a shell: export the variables from Python
AZEMU_AZURE=1 .venv/bin/pytest tests/sdk-python/cosmos/test_cosmos_query_basic.py -q --timeout=300
```

### Event Hubs and Kafka (`eventhubs/`, `kafka/`)

* Variables: `AZ_EVENTHUBS_CONN_STR` (the namespace connection string, a key that may manage), `AZ_EVENTHUBS_NAMESPACE` (the namespace name). For making and deleting event
  hubs and consumer groups inside that namespace (the Python SDK has no administration client) the suites call the Resource Manager REST API with a service principal:
  `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`, `AZ_SUBSCRIPTION_ID`, `AZ_RESOURCE_GROUP`. That is the only management call they make.
  `test_blob_checkpoint.py` also needs `AZ_STORAGE_CONN_STR` (a storage account).
* Tier: **Standard**, one throughput unit. The Kafka endpoint needs Standard or above. A namespace holds at most 10 event hubs and a hub 20 consumer groups, so the
  Event Hubs suite makes one hub for each of the partition counts 1, 2 and 4 (and one that nothing is ever sent to) and every test works in the shared hub: when a test
  starts it notes where each partition ends, and the helpers turn "read from the start" into "read after that point" (`ehutil.record_baseline`, `ehutil.start_kwargs`).
  A test that needs a hub to itself (another partition count, a time range) makes one and deletes it when it ends. The Kafka suite makes a topic (an event hub) for each
  test that needs one and deletes it again.
* What it leaves behind: nothing in the normal case. The shared hubs are deleted when the session ends (`AZEMU_AZURE_KEEP_HUBS=1` keeps them for the next run); the Kafka
  suite and the blob checkpoint tests delete what they made. A run that is killed can leave event hubs in the namespace and containers named `cp-*` in the storage account.
  Events stay in a hub for as long as it lives (retention is a day or more on Azure, so nothing can be told to expire quickly: those tests are `emulator_only`).
* Run one module at a time and not two runs at once against the same namespace (they share the hubs and the 10-hub limit); a module takes 2 to 10 minutes, and the waits
  in the helpers are longer on Azure (`ehutil.SCALE`). Use a longer timeout than the default 60 seconds:

```bash
set -a; . path/to/your/env; set +a
AZEMU_AZURE=1 .venv/bin/pytest tests/sdk-python/eventhubs/test_send.py -q --timeout=240
AZEMU_AZURE=1 .venv/bin/pytest tests/sdk-python/kafka -q --timeout=240
```

### Azure Files (`file/`)

* `azure-storage-file-share` 12.27.1 and raw REST requests signed with the account key (`fileutil.Raw`). Variables: `AZ_STORAGE_ACCOUNT` and `AZ_STORAGE_KEY` (the account name and its
  key; nothing else, and nothing calls the management plane, unlike the helpers of some other suites).
* Account: a general-purpose v2 account with SMB shares (a standard account: `Premium` and NFS are refused there, and the tests about that expect it).
* What it leaves behind: shares are deleted when a test ends, but **a deleted share stays on the account for the soft delete days (7 by default) and cannot be purged through the data
  plane**: a run leaves about 30 soft-deleted shares, all empty and gone from `list_shares`. The tests that only need directories and files share one share per run. Nothing is
  left in the live account: check with `list_shares` (the run ends with no share). The only setting the tests change is CORS (`test_file_service.py`), and they put it back.
* Azure answers `ShareBeingDeleted` for about half a minute after a delete: the one test that restores a share waits (`fileutil.eventually`), and a new share has a new name every time.
  A new CORS rule takes a few seconds to apply (the preflight test polls).
* The emulator-only tests are `emulator_only`; a run takes about 2 minutes.

```bash
set -a; . path/to/your/env; set +a
AZEMU_AZURE=1 .venv/bin/pytest tests/sdk-python/file -q --timeout=240
```

### Schema Registry (`schemaregistry/`)

* The registry of an Event Hubs namespace (`azure-schemaregistry` 1.3.0 and `azure-schemaregistry-avroencoder` 1.0.0, the REST API without an SDK, and a round trip of an encoded event
  through Event Hubs). Variables: `AZ_EVENTHUBS_NAMESPACE`, `AZ_SCHEMA_GROUP` (the namespace's schema group: Avro, compatibility Forward) and `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`,
  `AZURE_CLIENT_SECRET` of a principal with the roles Schema Registry Contributor and Azure Event Hubs Data Owner (or Sender and Receiver). The clients sign in with Microsoft Entra ID;
  a shared access key is not used, so a namespace with local authentication off works.
* Tier: **Standard**: one schema group and 25 schema versions (Microsoft Learn, "Event Hubs quotas and limits"). Every test makes schemas under a name of its own and deletes them when it
  ends (the REST API has a `DELETE` that the SDKs do not call; it takes away the latest version of a schema, so the cleanup repeats it). A run that is killed leaves schemas in the group:
  list them with `GET /$schemaGroups/{group}/schemas` and delete them, or the next run may hit the limit of 25.
* The round trip sends to the namespace's existing hub `AZ_EVENTHUBS_AUTH_HUB` (the suite calls no Resource Manager API): its events carry a marker property and are read from 30 seconds before
  the send. They stay in the hub for its retention.
* The emulator-only tests (a reset, any token passing) are `emulator_only`.

```bash
set -a; . path/to/your/env; set +a
AZEMU_AZURE=1 .venv/bin/pytest tests/sdk-python/schemaregistry -q --timeout=120
```

### Service Bus (`servicebus/`)

* Variable: `AZ_SERVICEBUS_CONN_STR` (the namespace connection string, with a key that may manage: the tests make and delete queues, topics, subscriptions and rules).
* Tier: **Standard** (topics, sessions, duplicate detection, forwarding and transactions need it). Large messages and the other Premium limits are not exercised.
* The suite owns the namespace while it runs. After every test it deletes every queue and topic of the namespace (raw requests, `sweep_namespace` in `conftest.py`), so the
  tests that list entities start from nothing, as they do on a freshly reset emulator. Do not point it at a namespace that holds anything you want to keep, and do not run two
  runs against one namespace at once. A run that is killed can leave entities behind (their names start with a short prefix and 10 hex digits).
* Tests about the emulator's own ports and transports (plain AMQP, AMQP in TLS on its port, WebSocket, auto-creation, a second namespace by name) are `emulator_only`; the
  namespace is the one of the connection string, and the SDK always uses TLS on 5671.
* Entity creation takes about a second and a test about a lock, a schedule or a time to live waits for real, so the modules take from 1 to 13 minutes each (the rules module is
  the longest, with 100 parametrised tests that each make a subscription); the whole suite takes about an hour. Use `--timeout=180`.

```bash
set -a; . path/to/your/env; set +a
AZEMU_AZURE=1 .venv/bin/pytest tests/sdk-python/servicebus/test_queue_locks.py -q --timeout=180
```

### Event Grid delivery properties (`storage_events/test_delivery_properties.py`)

* Four tests of this module (delivery properties from the event, a value over 4096 bytes and an empty `MessageId`, `SessionId` for a session queue, events without a usable session id) run against Azure; the
  rest is `emulator_only`. The custom topic, its three subscriptions and the queues are made beforehand by whoever owns the resources: the tests only publish and receive, and call no management API.
* Variables: `AZ_EVENTGRID_ENDPOINT` and `AZ_EVENTGRID_KEY` (a custom topic with `Event Grid` input schema), `AZ_EVENTGRID_PROPS_QUEUE` (a Service Bus queue that gets the events whose subject starts with `props/`, with the
  delivery properties `x-static` (static `abc`), `x-dyn` (dynamic `data.key1`), `MessageId` (dynamic `data.mid`) and `PartitionKey` (dynamic `data.pk`)), `AZ_EVENTGRID_SESSION_QUEUE` (a session-enabled queue that gets
  `session/…` with `SessionId` dynamic from `data.sid`) and `AZ_SERVICEBUS_CONN_STR` (the namespace connection string).
* What it leaves behind: nothing. The messages are received with delete; an event the queue refuses (an empty `MessageId`, no session id) is retried by Event Grid for a day and then dropped. The run takes about 30 seconds.
* The dead letter blob test is `emulator_only`: Azure writes dead letters about five minutes after the event, in one blob for the events of that time, and the test would have to delete the destination queue.

### Monitor (`monitor/`)

* Only `monitor/test_track_protocol.py` can run against Azure: it sends raw requests to the ingestion endpoint of an Application Insights resource and checks the status and
  the message of every answer (what the emulator copies is what these tests found). Variable: `AZ_APPINSIGHTS_CONN_STR`, a connection string with the instrumentation key
  (with no `IngestionEndpoint` in it the global endpoint, `dc.services.visualstudio.com`, is used, which answers `v2.1/track` with a redirect: the tests use `v2/track`).
  The SDK tests read telemetry back through the emulator's admin API and are `emulator_only`; Application Insights has no way to read it back that is quick.
* What it leaves behind: the telemetry it sent, named `azemu-probe`. Telemetry cannot be taken back; the run sends about 60 requests of one or two items.
* Tests about a difference that was left are `azure_differs` (the emulator takes every GUID as an instrumentation key; Azure refuses one it does not know).

```bash
set -a; . path/to/your/env; set +a
AZEMU_AZURE=1 .venv/bin/pytest tests/sdk-python/monitor/test_track_protocol.py -q
```

Every test of this suite also fails when the emulator refused a request of the monitor service while it ran (read from the emulator's own log), unless it carries the marker
`expects_refusals`.

### Strict authorisation (`auth/`)

* One suite that runs on **both sides** (`docs/strict-auth.md`). Against the emulator its fixture starts the unified binary with `AZEMU_STRICT_AUTH=1` (the runner sets it for this suite), makes the accounts through Resource Manager and
  gives four principals the roles that the Azure side has (`main`: every data role; `reader`: the read roles; `norole`; `scoped`: Blob Data Reader on one container and Secrets User on one secret), with Resource Manager requests, as Terraform does.
  Against Azure it uses the four service principals of the environment and creates **nothing** on the management plane.
* Variables for `AZEMU_AZURE=1`: `AZURE_TENANT_ID`, `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET` (main), `AZ_READER_CLIENT_ID` / `_SECRET`, `AZ_NOROLE_CLIENT_ID` / `_SECRET`, `AZ_SCOPED_CLIENT_ID` / `_SECRET`, `AZ_STORAGE_ACCOUNT`
  (a flat account), `AZ_HNS_ACCOUNT` (hierarchical namespace), `AZ_SCOPED_CONTAINER` (a container, in the flat account, where `scoped` has a role), `AZ_KEYVAULT_URL` (role-based), `AZ_KEYVAULT_POLICY_URL` (access policies),
  `AZ_SCOPED_SECRET` (a secret of the first vault where `scoped` has a role), `AZ_APPCONFIG_ENDPOINT`, `AZ_COSMOS_ENDPOINT` and `AZ_COSMOS_KEY` (for making the database: no token may), `AZ_EVENTGRID_ENDPOINT` (a custom topic).
  The roles are the ones in the table at the top of `docs/strict-auth.md`; the principals need them in force before the run.
* What it leaves behind: nothing in the normal case. Every container, blob, queue, table, file system, secret, key-value and database a test makes is deleted at the end of its module (by the principal `main`); the secret of `scoped` and the
  container of `scoped` are written and removed too. A run that is killed can leave a few of them. About 60 requests to each service and about 50 token requests; about a minute.
* Tests of what only the emulator has (the admin API, `/_azemu/auth/check`, the principal endpoint, an expired token, Resource Manager's answers) are `emulator_only`. The Rust tests in `crates/azemu/tests/strict_auth.rs` cover the mode off, persistence, the config file and Resource Manager.

```bash
set -a; . path/to/your/env; set +a
AZEMU_AZURE=1 .venv/bin/pytest tests/sdk-python/auth -q --timeout=300
.venv/bin/python tests/run_sdk_suites.py --bin target/debug/azemu --only auth      # the emulator side
```

### Faults (`faults/`)

* Fault rules ([`docs/faults.md`](../../docs/faults.md)) through the Blob, Queue, Table, Cosmos DB, Key Vault, App Configuration and Service Bus clients: they retry what is throttled
  or dropped and then succeed, they waited as long as the retry header said where the SDK honours it, and with retries off they raise the right error. Every test is
  `emulator_only` and needs the unified binary (the runner knows). About two minutes; the Service Bus file takes half of it.

## Helpers

`conftest.py` holds the `emu` fixture (start, wait for `/_azemu/health`, stop); each suite has a `conftest.py` that names the service and a `*util.py` with its helpers.
`servicebus/sdk_dial_probe.py` and `eventhubs/sdk_dial_probe.py` are not tests: each starts a bare AMQP listener and prints what the SDK sends first, which is how the
connection settings in `docs/sdk-notes.md` were found.
