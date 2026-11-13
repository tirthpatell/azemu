# Settings

Every setting is an `AZEMU_*` environment variable, and most also have a command-line option (named in the table).
All of them are read the same way (`azemu_core::settings`):

- A switch is on for `1`, `true`, `yes`, `on` and off for `0`, `false`, `no`, `off`, in any case. An empty value counts as
  not set. Anything else is reported once on stderr and the default is used.
- A size is a number of bytes, or a number with `K`, `M` or `G` (powers of 1024). A length of time is seconds, or a
  number with `ms`, `s`, `m`, `h` or `d`.
- A command-line option wins over the variable. `--lenient` and `--allow-webhook-host` are read before the services are
  built, because services read their settings when they start.
- `AZEMU_LENIENT=1` (or `--lenient`) turns on every convenience Azure does not have: queues, topics, event hubs and
  consumer groups that a client names but nobody created come into being on first use, Event Grid topics too, and new
  storage accounts keep blob versions. Each convenience also has a variable of its own, which wins over
  `AZEMU_LENIENT` for that one convenience.
- `GET /_azemu/config` shows the settings in force (name, value, and whether it came from the default, the
  environment or an option), and the `AZEMU_*` variables that nothing has read.
- At start the emulator logs one warning for each `AZEMU_*` variable it does not know: a typing mistake. The names of
  the test tooling in the second table below are not settings of the emulator, and are not warned about.

The table below is produced by the code (`azemu __settings-table` prints it) and a test (`crates/azemu/tests/settings_doc.rs`)
fails when this file differs. After a change to a setting, update this file with:

```
AZEMU_UPDATE_SETTINGS_DOC=1 cargo test -p azemu --test settings_doc
```

A crate appears in the table by calling `azemu_core::settings::describe(name, default, meaning)` next to the code that
reads the setting, in the constructor of its service (so that `azemu __settings-table` sees it).

<!-- BEGIN TABLE -->
| Variable | Default | Meaning |
|---|---|---|
| `AZEMU_ALLOW_HOST` | none | Also serve requests whose Host is this name or a name under it (comma separated). Option: `--allow-host`. |
| `AZEMU_ALLOW_ORIGIN` | none | Let web pages from this origin, host or `*.suffix` use the emulator although they are on another site; `*` turns the cross-site check off (comma separated). Option: `--allow-origin`. |
| `AZEMU_ALLOW_WEBHOOK_HOST` | none | Event Grid webhooks may also go to this host name (comma separated; `*` allows any). Option: `--allow-webhook-host`. |
| `AZEMU_APPCONFIG_REVISIONS_PER_KEY` | 1000 | most revisions kept for one key and label, the current one included |
| `AZEMU_APPCONFIG_REVISION_DAYS` | 30 | days a revision of a setting is kept (counted from the revision's own time) |
| `AZEMU_APPCONFIG_SNAPSHOTS` | 10000 | most snapshots in one store |
| `AZEMU_APPCONFIG_SNAPSHOT_MB` | 50 | megabytes of snapshots (active and archived) in one store |
| `AZEMU_APPCONFIG_STORE_MB` | 100 | megabytes of settings and history in one store; a write over it gets 429 |
| `AZEMU_ARM_DEPLOYMENT_HISTORY_MB` | 64 | ARM: megabytes of deployment records one subscription keeps; the oldest are dropped first (Azure also keeps at most 800 per group). |
| `AZEMU_ARM_STRICT` | off | ARM: a write for a resource type that no provider emulates (Microsoft.Web/sites) answers 501 instead of being stored as plain JSON. |
| `AZEMU_BLOB_LOG_DELAY` | 3s | how long Storage Analytics log entries (Blob logging switched on with Set Blob Service Properties) wait before they are written to the `$logs` container; Azure takes up to an hour |
| `AZEMU_BLOB_LOG_MAX_BLOBS` | 2000 | log blobs one account keeps in `$logs`; the oldest are removed first |
| `AZEMU_BLOB_LOG_MAX_BYTES` | 64M | bytes of log blobs one account keeps in `$logs`; the oldest are removed first |
| `AZEMU_BLOB_QUERY_MAX_ANSWER_BYTES` | 128M | largest answer of one query; past it the query ends with a fatal error |
| `AZEMU_BLOB_QUERY_MAX_BLOB_BYTES` | 256M | largest blob a Query Blob Contents request will scan |
| `AZEMU_BLOB_QUERY_MAX_CONCURRENT` | 4 | queries that run at the same time; further ones wait, then get 503 ServerBusy |
| `AZEMU_BLOB_QUERY_MAX_ERRORS` | 1000 | non-fatal errors a query reports before it ends |
| `AZEMU_BLOB_QUERY_MAX_RECORD_BYTES` | 4M | largest single record (CSV row or JSON object) a query accepts |
| `AZEMU_BLOB_QUERY_SECONDS` | 30 s | time one query may scan before it ends with a fatal error |
| `AZEMU_BLOB_SOFT_DELETE_BYTES` | 256M | bytes of soft-deleted snapshots one account keeps (0 keeps none) |
| `AZEMU_BLOB_VERSIONING` | off (on with AZEMU_LENIENT) | blob versioning for storage accounts that appear on first use; an account made through ARM follows its own setting |
| `AZEMU_BLOB_VERSION_BYTES` | 256M | bytes of old blob versions one account keeps; older ones are dropped first (0 keeps none) |
| `AZEMU_COSMOS_SCRIPT_LOOP_ITERATIONS` | 5000000 | Cosmos: most loop iterations one script may run. |
| `AZEMU_COSMOS_SCRIPT_MAX_OPERATIONS` | 10000 | Cosmos: most collection calls (reads, writes, queries) one script may make. |
| `AZEMU_COSMOS_SCRIPT_MAX_RUNNING` | 4 | Cosmos: scripts that may run at the same time; more wait their turn, then answer 429. |
| `AZEMU_COSMOS_SCRIPT_MEMORY_MB` | 192 | Cosmos: megabytes of memory one script worker may use beyond its start. |
| `AZEMU_COSMOS_SCRIPT_TIMEOUT_MS` | 5000 | Cosmos: milliseconds a stored procedure, trigger or user-defined function may run before its worker is stopped. |
| `AZEMU_DATA_DIR` | none (state is kept in memory only) | Keep state between runs in this directory. Option: `--data-dir`. |
| `AZEMU_EVENTGRID_AUTOCREATE` | off (on with `AZEMU_LENIENT`) | Publishing to a custom topic or namespace topic that was never created, or receiving from a namespace subscription that was never created, makes it, instead of the not-found answer Azure gives. |
| `AZEMU_EVENTGRID_CONFIG` | none | Path of a JSON file with topics, domains, namespaces and subscriptions to create at start and after every reset. |
| `AZEMU_EVENTGRID_DELIVERY_TIMEOUT_MS` | 10000 | Milliseconds a webhook may take to answer before the delivery counts as failed. |
| `AZEMU_EVENTGRID_MAX_PENDING` | 1000 | Most deliveries waiting for a retry per subscription; the oldest is dead-lettered when more come. |
| `AZEMU_EVENTGRID_MAX_PENDING_MB` | 32 | Most megabytes of deliveries waiting for a retry per subscription. |
| `AZEMU_EVENTGRID_MAX_PENDING_TOTAL` | 5000 | Most deliveries waiting for a retry in all subscriptions together. |
| `AZEMU_EVENTGRID_MAX_PENDING_TOTAL_MB` | 128 | Most megabytes of deliveries waiting for a retry in all subscriptions together. |
| `AZEMU_EVENTGRID_RETRY_BASE_MS` | 500 | Milliseconds before the first retry of a failed webhook delivery; later retries wait twice as long each time. |
| `AZEMU_EVENTGRID_RETRY_MAX_MS` | 10000 | Longest wait in milliseconds between two retries of a webhook delivery. |
| `AZEMU_EVENTHUBS_AUTOCREATE` | off (on with `AZEMU_LENIENT`) | An event hub or consumer group that is sent to or read from but was never created comes into being (over AMQP and over the Kafka endpoint), instead of the not-found answer Azure gives. |
| `AZEMU_EVENTHUBS_CONFIG` | none | Path of a JSON file with event hubs and consumer groups to create at start and after every reset. |
| `AZEMU_KAFKA_ADVERTISED_HOST` | none (the host the client connected to) | Broker host name the Kafka endpoint puts in Metadata and FindCoordinator answers, for plain connections that do not name one. |
| `AZEMU_KAFKA_ADVERTISED_PORT` | none (the port the emulator listens on) | Broker port the Kafka endpoint advertises when clients reach the emulator through another port. |
| `AZEMU_KAFKA_INITIAL_REBALANCE_DELAY_MS` | 500 | Milliseconds the first round of a new Kafka consumer group waits for more members to join (at most 30000). |
| `AZEMU_LENIENT` | off | Turn on every convenience Azure does not have: things a client names but nobody created (queues, topics, event hubs, consumer groups, Event Grid topics) come into being on first use, and new storage accounts keep blob versions. Option: `--lenient`. A convenience's own variable overrides this for that convenience. |
| `AZEMU_LOOSE_AUTH` | off | Skip the signature checks of storage requests (Shared Key and SAS) for the one account whose key is known, `devstoreaccount1`. |
| `AZEMU_SAVE_INTERVAL` | 15 (seconds) | Seconds between saves of changed state when a data directory is set; 0 saves only on `POST /_azemu/save` and at shutdown. Option: `--save-interval`. |
| `AZEMU_SCRIPT_WORKER` | the emulator's own program | Cosmos: the program that runs a script worker (it must handle the worker's hidden first argument); only embedders and tests need it. |
| `AZEMU_SERVICEBUS_AUTOCREATE` | off (on with `AZEMU_LENIENT`) | A queue, topic or subscription that is sent to or read from but was never created comes into being, instead of the not-found answer Azure gives. |
| `AZEMU_SERVICEBUS_CONFIG` | none | Path of a JSON file with queues, topics, subscriptions and rules to create at start and after every reset. |
| `AZEMU_SERVICEBUS_TXN_CONNECTION_MAX_MB` | 256 | Most megabytes of messages the open transactions of one connection may hold. |
| `AZEMU_SERVICEBUS_TXN_MAX_MB` | 128 | Most megabytes of messages one open AMQP transaction may hold. |
| `AZEMU_SERVICEBUS_TXN_TIMEOUT_SECONDS` | 120 (seconds) | How long an AMQP transaction may stay open before it is rolled back (seconds, or a number with ms, s, m). |
| `AZEMU_SERVICEBUS_TXN_TOTAL_MAX_MB` | 1024 | Most megabytes of messages all open transactions together may hold. |
| `AZEMU_STANDARD_PORTS` | off | Open one fixed port per service next to the gateway port: 10000 Blob, 10001 Queue, 10002 Table, 10003 Data Lake, 10010 Key Vault, 10011 App Configuration, 10012 Event Grid, 10013 ARM, 10014 Identity, 8081 Cosmos, 5672 and 5671 AMQP (plain and TLS on both), 9093 Kafka, and 443 when the process may bind it (a warning when it may not). Option: `--standard-ports`. |
| `AZEMU_TLS_NAME` | none | Extra names the local CA may sign for, besides localhost and loopback addresses (comma separated). A different set makes a new CA. Option: `--tls-name`. |
<!-- END TABLE -->

## Names that belong to test tools

The test suites and scripts of this repository, and the test-support crate, read `AZEMU_*` variables of their own
(which emulator program to start, where a running one is, whether to keep a log). A test program passes its environment on
to the emulator it starts, so these names reach the emulator although it does not read them. It does not warn about them
at start and does not list them under `unreadVariables`. Any other `AZEMU_*` name it does not know is still reported as a
likely typing mistake. The list is in `azemu_core::settings::TEST_TOOLING`, and a test keeps this table in step with it.

<!-- BEGIN TEST TOOLING -->
| Variable | Used by |
|---|---|
| `AZEMU_BIN` | The `azemu` program a test harness starts (the SDK suites, `tests/run_all.sh`, `azemu-testkit`). |
| `AZEMU_FULL_BIN` | The path of the all-services program, for the Python suites (older name of `AZEMU_UNIFIED_BIN`). |
| `AZEMU_UNIFIED_BIN` | The path of the all-services program, for the Python suites. |
| `AZEMU_GATEWAY` | The address of an emulator that is already running, such as `http://127.0.0.1:4577`; a harness that finds it uses that emulator instead of starting one. |
| `AZEMU_CA_FILE` | The path of that running emulator's `ca.pem`. |
| `AZEMU_CERT_DIR` | The certificate directory of the emulator that the JavaScript suite started (set by its runner for the test files). |
| `AZEMU_EXTRA_TRUST_PEM` | A PEM file that the Java suite adds to its trust store, for a proxy in front of the emulator. |
| `AZEMU_STORAGE_CONN_STR` | A storage connection string that the Python suites use as it is, instead of building one for the emulator they start. |
| `AZEMU_COSMOS_ENDPOINT` | The Cosmos endpoint that the Python Cosmos suite uses instead of starting an emulator. |
| `AZEMU_COSMOS_VERIFY` | `0` turns off the certificate check of that endpoint in the Python Cosmos suite. |
| `AZEMU_EH_STACK` | The ports (Event Hubs, then Blob) of an emulator the Python Event Hubs checkpoint test uses instead of starting one. |
| `AZEMU_EVENTHUB_HOST` | The host name the container test's client uses to reach Event Hubs. |
| `AZEMU_HOST` | The host name of the emulator that the example checks and the container test's client connect to. |
| `AZEMU_PORT` | The gateway port of the emulator that the JavaScript suite's runner started (set for the test files). |
| `AZEMU_IMAGE` | The container image the examples and the container test use (default `azemu:latest`). |
| `AZEMU_CONTAINER_KEEP` | Keep the container test's containers and files after it ends. |
| `AZEMU_CONTAINER_SCRATCH` | Where the container test keeps its files. |
| `AZEMU_FUNC_SCRATCH` | Where the Azure Functions test keeps its files. |
| `AZEMU_KEEP_LOG` | Where the Python and Go suites copy the emulator's log when they end. |
| `AZEMU_LOG_PATH` | The file the JavaScript suite writes the emulator's log to (set by its runner for the test files). |
| `AZEMU_DUMP` | When set, the Go suite prints every request and response to stderr. |
| `AZEMU_FINDINGS` | `1` runs the .NET tests that show a known difference from Azure (they fail while it stands). |
| `AZEMU_RUN_FINDINGS` | `1` runs the Go tests that show a known difference from Azure (they fail while it stands). |
| `AZEMU_PERSIST_BENCH_DIR` | A directory on a real disk for the ignored persistence timing tests of the service crates (they write about 100 MB). |
| `AZEMU_COST_DIR` | A directory for the persistence cost test of Blob (default `~/.cache/azemu-cost`). |
| `AZEMU_UPDATE_SETTINGS_DOC` | Set to 1 when running `cargo test -p azemu --test settings_doc` to rewrite the table in `docs/SETTINGS.md`. |
| `AZEMU_TEST_*` | Any name that starts so: options of the test harnesses (see the README of the suite that reads it). |
| `AZEMU_TESTKIT_*` | Any name that starts so: options of the test harnesses (see the README of the suite that reads it). |
<!-- END TEST TOOLING -->

## Read by a crate that does not describe them yet

These names are in the source of a crate that has not been converted to `settings::describe`. They are real settings; their
meaning is in that service's document under `docs/services/`.

<!-- BEGIN UNDESCRIBED -->
<!-- END UNDESCRIBED -->
