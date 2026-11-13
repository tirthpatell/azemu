# Python SDK tests for azemu

Tests that drive the emulator with the official Azure SDKs for Python. They are the largest suite and the quickest to run: one directory per service
(`blob`, `queue`, `table`, `datalake`, `keyvault`, `appconfig`, `cosmos`, `servicebus`, `eventhubs`, `kafka`, `eventgrid`, `storage_events`, `identity`, `arm`,
`persistence`). The tests were written from the SDKs and Microsoft's documentation, not from the emulator's code, and they double as examples of how to point each
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

* The emulator does not create a queue, topic, event hub, consumer group or Event Grid topic because a client names it, as Azure does not, and blob versioning is off until
  someone turns it on. Tests about the opposite behaviour carry the markers `autocreate` and `versioning`; they are skipped in the first pass and the runner runs them in a
  second pass with the emulator's `AZEMU_*_AUTOCREATE`, `AZEMU_BLOB_VERSIONING` or `AZEMU_LENIENT` variables set (`autocreate_gate.py` has the rules). Tests that check
  what Azure answers for a missing entity (`no_autocreate`, `no_versioning`) are skipped in that second pass.
* `emulator_only` marks tests of behaviour that only the emulator has (its admin API, `/_azemu/...`); `slow` marks tests that wait for a lock, a TTL or a schedule (a few seconds each).
* The suites can also run against another service with `AZEMU_GATEWAY` / `AZEMU_STORAGE_CONN_STR` (for example Azurite, to check an expectation against a second implementation). The
  tests skipped in that case say so in their reason ("Azurite does not implement ..."); against azemu itself they run.

## Helpers

`conftest.py` holds the `emu` fixture (start, wait for `/_azemu/health`, stop); each suite has a `conftest.py` that names the service and a `*util.py` with its helpers.
`servicebus/sdk_dial_probe.py` and `eventhubs/sdk_dial_probe.py` are not tests: each starts a bare AMQP listener and prints what the SDK sends first, which is how the
connection settings in `docs/sdk-notes.md` were found.
