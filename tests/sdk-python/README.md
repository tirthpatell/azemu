# Python SDK tests for azemu

Tests that drive the emulator with the official Azure SDKs for Python. They are the largest suite and the quickest to run: one directory per service
(`blob`, `queue`, `table`, `datalake`, `keyvault`, `appconfig`, `cosmos`, `servicebus`, `eventhubs`, `kafka`, `eventgrid`, `storage_events`, `identity`, `arm`,
`persistence`, `seed`). The tests were written from the SDKs and Microsoft's documentation, not from the emulator's code, and they double as examples of how to point each
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
* `emulator_only` marks tests of behaviour that only the emulator has (its admin API, `/_azemu/...`); `slow` marks tests that wait for a lock, a TTL or a schedule (a few seconds each).
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
* What it leaves behind: nothing in the normal case. Every database a test makes is deleted after the test (by resource id, so names with `#` or `?` work too);
  a test that is killed half way can leave its database. The runs create and delete many containers (a few seconds each) and send a few hundred to a few thousand requests per module;
  the modules add up to about 25 minutes of test time (they can run in parallel, one pytest process per module; use `--timeout=300`, the default of 60 seconds is short for a real account).
* One run uses one account; do not run two runs against the same account at once if they use the same database names (names are random, so this is only a problem for
  a manual clean-up).
* `test_cosmos_query_azure_rules.py` replays about 440 queries from `cosmos_query_cases.json`, each with the rows or the error the account gave in November 2026 (about 450 queries
  and four containers of seven to twelve documents; a minute on the account). The same file runs against the emulator.

```bash
set -a; . path/to/your/env; set +a
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

## Helpers

`conftest.py` holds the `emu` fixture (start, wait for `/_azemu/health`, stop); each suite has a `conftest.py` that names the service and a `*util.py` with its helpers.
`servicebus/sdk_dial_probe.py` and `eventhubs/sdk_dial_probe.py` are not tests: each starts a bare AMQP listener and prints what the SDK sends first, which is how the
connection settings in `docs/sdk-notes.md` were found.
