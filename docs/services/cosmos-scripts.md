# Cosmos DB server-side scripts: stored procedures, triggers, user-defined functions

**What it emulates.** Stored procedures (`POST .../sprocs/{id}`), pre- and post-triggers named on item operations, and user-defined functions called from
queries. The JavaScript runs on `boa_engine` (a pure-Rust engine, ES2015 and later, no `Intl`, no `eval`) in a **separate worker process** that the
emulator limits and kills: the script never runs in the emulator's own process. Scripts are atomic and hold no lock while they run. The service they
belong to is `docs/services/cosmos.md`; they are reached the same way (gateway `https://{account}.documents.localhost:4577/`, or port 8081 with
`--standard-ports`).

**Not built.** Attachments, `enableScan` and the other query options of the server-side API (accepted, ignored), RU accounting and throttling, stack
traces with line numbers, triggers on patch, transactional batch and bulk calls. On macOS and Windows a script's memory is not limited (the time limit
and the kill still hold); a build made without the `cosmos-scripts` feature answers 501 for any script run ([`../releasing.md`](../releasing.md)). See "Bounds" for the rest.

## What it is

A request that runs a script has three steps. The script never runs while the service lock is held, and the JavaScript never runs in the emulator's process.

1. **Under the lock** (`script_run.rs`, `prepare_sproc` / `prepare_doc_op`): find the script, copy the logical partition the request is scoped to
   (`Container::snapshot` in `state.rs`; documents are shared `Arc`s, not copied), set aside `max_operations + 8` document counters for the script, and
   hand back a `ScriptJob` instead of an answer (`Body::Script`).
2. **On a blocking thread** (`ScriptJob::run`, started with `spawn_blocking` after the request got a script slot): start a worker process (below), send it the script and
   its inputs, and answer its collection calls (create, read, query, replace, upsert, delete, feeds, log lines) from the copy. The copy is the only thing the worker's
   calls reach. When the worker reports the result, kill it.
3. **Under the lock again** (`Container::apply_snapshot`, called from `CosmosService::run_script` in `lib.rs`): if the partition is exactly as it was
   in step 1 (same documents, same LSNs), apply the script's writes in one go (with change feed entries, LSNs, TTL bookkeeping, as for any write). If not,
   throw the result away and start again from step 1, up to 5 times, then answer 449 `RetryWith` ("the request conflicted with the current state, retry in a
   new transaction": the code the server-side API reference lists for exactly this). A script has no effect outside the database, so running it again is safe.

What that gives, and the answer to "what does the script hold while it runs": **nothing**. No lock, no flag on the container, and no thread of the emulator's async runtime.

* Atomic: all the writes of a script (and of a request with triggers, the operation included) or none. An uncaught exception, `abort`, a time-out, a
  failed operation, a crashed or killed worker: the copy is dropped.
* Isolated from other requests on the same partition: the script sees the partition as of its start, and commits only if nothing changed it
  (whole-partition check, so it is serialisable: no lost update, no write skew). A writer that never stops can starve a script (449 after 5 tries).
  Tests: `scripts_on_the_same_partition_are_atomic...` (4 concurrent increments that each think 150 ms: the counter ends at 4) and
  `a_script_that_loses_the_race...` (449, the script's write is not applied).
* Point reads, writes and queries on **other containers, and on the same container**, are never held up by a running script (`a_running_script_does_not_hold_up...`:
  a 1.5 s script, 60 reads and writes in under a second).
* Concurrency of scripts: `ScriptSettings::max_running` (4) at a time (so at most 4 stored procedure or trigger workers, plus one worker per running query that calls a function; queries have their own limit of 4 running at a time); the others wait up to the time limit for a slot, then get 429 with `x-ms-retry-after-ms: 100`. A slot is given back when the
  worker process has ended, which the emulator makes happen at the time limit (plus a short grace) at the latest.

## The worker process

**How it starts.** `std::process::Command` runs the executable again with a hidden first argument: `<exe> __cosmos-script-worker <settings as JSON>`.
`azemu` (`crates/azemu/src/main.rs`) and `azemu-cosmos` (`crates/services/cosmos/src/main.rs`) call `azemu_cosmos::script_worker_main_if_requested()` as the first
thing in `main`, before a runtime or the command line exists; in a worker it never returns. Which program: `ScriptSettings::worker` (embedders and tests), then the environment
setting `AZEMU_SCRIPT_WORKER` (read once, by `ScriptSettings::from_env`), then `std::env::current_exe()`. The crate has a second small program, `azemu-cosmos-script-worker` (`src/worker_bin.rs`), that only runs the worker: the
tests of this crate use it through `CARGO_BIN_EXE_azemu-cosmos-script-worker`, because in a test the current executable is the test binary. If the program is missing or cannot be run,
the script request answers 500 `The script worker could not be started (<path>): <reason>. Scripts cannot run without it.` Nothing falls back to running a script in the emulator.
A worker that starts and ends at once (for instance a program that does not know the hidden argument) answers 500 `The script worker ended unexpectedly (...)`.
(If the emulator's executable is replaced on disk while it runs, Linux reports the old path as `... (deleted)` and the start fails: set `AZEMU_SCRIPT_WORKER` to the new file.)

**What the worker gets.** An empty environment (only `MALLOC_ARENA_MAX=1`), pipes for standard input, output and error, and nothing else: every other descriptor of the emulator is
close-on-exec (Rust opens them so), and a test (`a_worker_has_only_its_pipes_an_empty_environment_and_its_limits`) lists `/proc/<pid>/fd` of a running worker: `0 1 2`. The worker opens no file and no socket.
Its working directory is the emulator's.

**Messages** (`js/proto.rs`): length (4 bytes, big endian) then JSON, over the worker's standard input and output. Standard error is only for the Rust runtime's last words
(a panic, `memory allocation of N bytes failed`, `has overflowed its stack`), which the emulator reads after the worker has ended, to say why. From the emulator: `Run` (stored procedure or
trigger: text, settings, arguments, time left), `Udf` (one function call), `Reply` (the answer to a collection call). From the worker: `Host` (one collection call or log line),
`Done`, `UdfDone`, `Failed`. The worker asks for collection calls and for nothing else; the emulator answers from the partition copy it keeps for the script. **Everything the worker sends is untrusted:**

| What | Bound |
|---|---|
| Size of one message from the worker | 16 MB, checked on the length before anything is read (memory grows with what arrives, not with the length). Larger: the worker is killed, 400 `The script produced a message of N MB, more than the 16 MB ...` |
| Not JSON, or not a message of the language | the worker is killed, 500 `The script worker sent something that is not a message` |
| A message of the wrong kind (a function result for a stored procedure) | the worker is killed, 500 |
| Collection calls | `max_operations` (10,000) accepted; after that every call answers "not accepted". A ceiling of `20 * max_operations + 1000` calls of any kind ends the run (400) |
| Size of an answer to the worker | 48 MB; a bigger feed or query result is answered with a 413 error to the script |
| Result (response body) | 2 MB, checked by the emulator on the text as before |
| Time | the worker is killed at the deadline plus 300 ms (`kill_grace`), whatever it is doing |
| The emulator's own state | the worker cannot reach it. The calls are served on a blocking thread from a private copy; no service lock is held while waiting for the worker |

**UDFs in queries.** One worker per query, not per row: the query evaluator asks for one value at a time, so each call is one message to the worker and one back over the pipe
(the function text is sent with the first call of that function only). The query's calls share the 5 s limit and the same kill. A query with a function that is
never reached starts no worker. Measured: 300 calls of a function in one query (debug build) in the test `user_defined_functions_use_one_worker_per_query...` (printed with `--nocapture`).
Triggers are stored-procedure-style runs: each pre- or post-trigger of a request is one `Run` in a fresh worker (a request with a pre- and a post-trigger starts two, one after the other).

**Pool or not.** A hello-world stored procedure takes about 27 ms end to end in a debug build, 23 to 24 ms before this change (see "Numbers"), so no pool of idle workers was built:
the common case stays under the 50 ms the task allowed. If a release build or a slower machine ever makes the start the main cost, a pool is a local change in `WorkerProc::start`
(workers are used for one job and then replaced, the protocol does not change).

## Engine: `boa_engine` 0.22.0

| | |
|---|---|
| Crate | `boa_engine` 0.22.0 (2026-08-28), licence `Unlicense OR MIT`, pure Rust, no C toolchain, no V8 |
| Where | `crates/services/cosmos/Cargo.toml` only: `boa_engine = { version = "0.22.0", default-features = false }` |
| Features | defaults off: no `temporal`, no `float16`, no `xsum`; no `intl` (no ICU data). `Intl` and `Temporal` do not exist in scripts. About 90 more crates join the tree (most small: `icu_*` pieces and proc macros that the parser needs) |
| Rust version | the crate needs Rust 1.91, which is what the workspace declares (`rust-version`). Anyone building with an older compiler gets a Cargo error |
| JavaScript | what Boa implements: ES2015 and later (classes, arrow functions, template literals, destructuring, spread, generators, Promises, `async`/`await`, `Map`/`Set`, `Proxy`, `Symbol`, BigInt, optional chaining, named regex groups, `Date`, `JSON`, `Math`). Run in the tests (`current_javascript_features_run`, and the async sample). Not Azure's engine: no source positions in errors, other error texts, `toLocaleString` without locale data |
| Refused | `eval` and the `Function` constructor (also through `(function(){}).constructor`): a host hook raises `EvalError` (the compiler recurses on the nesting of text, and text a script builds cannot be checked in advance) |

Boa was chosen because it is the one pure-Rust engine that is complete enough: it ran every documented sample unchanged (they are the tests). It has no memory limit and no
time limit that reaches native calls, callbacks and promise jobs, only a loop limit and a recursion limit per call, and it overflows its stack on deeply nested text: so it runs
in a worker process that the emulator limits and kills (see "The worker process" and "Bounds").

## Bounds

**What keeps the emulator safe is the worker process, not the checks in the engine.** Nothing a script does can end the emulator, hold a slot longer than the time limit
plus 300 ms, or grow the emulator's memory: the worst a script can do is end its own worker, and that is an error for its own request. The checks inside the engine (loops,
recursion, the source check, the one-step builders) are kept because they give friendly errors early; nothing depends on them for safety.

### Enforced from outside (the guarantees)

| Bound | Limit (setting) | What enforces it, and the answer | Test |
|---|---|---|---|
| Wall-clock time | 5 s (`timeout`, Azure's number), 408 | the emulator kills the worker (SIGKILL; `TerminateProcess` on Windows) at deadline + 300 ms (`kill_grace`) from a task on the async runtime, whatever the worker is doing (a native call, a callback, a promise job). The engine normally reports the time-out itself a few ms after the deadline (same 408) | `c2_catastrophic_regex...`, `c3_callbacks...`, `a_native_call_that_does_not_look_at_the_clock...` |
| Slot | `max_running` (4) | the permit is held until the worker is dead and reaped, so a slot is free within the time limit plus 300 ms; `/_azemu/reset` has nothing to free | same tests; `no_more_scripts_run_at_once_than_the_limit...` |
| Memory | the worker may add 192 MiB (`memory_growth`), 400 | Linux: the worker limits its own address space (`RLIMIT_AS`) to what it uses once the engine is loaded + `memory_growth` + 64 MiB. An allocation past that fails, the Rust runtime aborts the worker with `memory allocation of N bytes failed` on standard error, and the emulator answers 400 `The script used more than N MB of memory ...`. The emulator's own memory is not involved (the C4 script measured: emulator peak 34 MB, unchanged) | `c4_memory_used_in_a_promise_job...`, `memory_that_a_script_may_use_is_still_available`, js `a_string_doubled_forty_times...` (REST) |
| Processor time | `timeout` + 3 s | `RLIMIT_CPU` set by the worker on itself (SIGXCPU; a second later SIGKILL). A second line for the case that the emulator cannot kill the worker (it is shutting down, or is itself stuck): the worker ends anyway. Answer: 408 | limits shown in `a_worker_has_only_its_pipes...` |
| Crash of the engine | 400 or 500 | a stack overflow, abort or panic of the engine ends or fails the worker only. Stack overflow (`has overflowed its stack` on standard error): 400 `... the script engine ran out of stack`. Anything else that ends the worker without a reason the emulator caused: 500 `The script worker ended unexpectedly (signal: 9 (SIGKILL))`. A panic caught in the worker: 500 `The script engine panicked: ...` | `c1_text_that_still_gets_past_the_check...`, `a_worker_killed_from_outside...`, `a_panic_in_the_engine_is_a_500...` |
| Cancelled request | the worker is killed at once | the async side of the request holds a guard (`js::proc::CancelOnDrop`); dropping the request's future (or the runtime shutting down) kills the request's workers from the dropping thread. **Over HTTP, `crates/core` runs the service call in a task of its own and stops it when the client goes away only for requests that the service asks about** (`Service::abort_on_disconnect`: Cosmos asks for a stored procedure run, `POST /dbs/{db}/colls/{coll}/sprocs/{id}`, so its worker is killed at once; a query with a function and a write with a trigger cannot be told from a plain one by path, so their workers end at the time limit) | `a_dropped_request_kills_its_worker_at_once` (the service), `a_client_that_goes_away...` (HTTP, bounded by the time limit) |
| The emulator goes away | no orphan workers | the worker's input thread ends the process when its standard input closes (the emulator exited, crashed or was killed), even while the engine is busy in a native call; the processor-time limit is the second line. No `prctl` or parent-death signal is needed (that would need `unsafe` or another crate). On SIGTERM the emulator ends its workers first (dropping the request futures kills them) | `shutting_the_emulator_down_while_scripts_run...` (SIGKILL and SIGTERM) |
| Messages from the worker | 16 MB each, 20x `max_operations` calls | see "The worker process" | `a_worker_that_writes_garbage_or_a_message_that_is_too_large...` |
| Response body | 2 MB (`max_response`), 400 | checked by the emulator on the text | `the_response_body_is_limited_to_2_mb` |
| Collection calls | 10,000 accepted per run (`max_operations`) | after that, or when less than a tenth of the time is left, each call answers `false` ("not accepted"): the documented way to write a bulk import that resumes | `bulk_import_stops_when_a_call_is_not_accepted_and_resumes` |

### Per platform: what is and is not guaranteed

| | Linux (developed and tested here) | macOS | Windows |
|---|---|---|---|
| Kill at the time limit, kill on cancel | yes (SIGKILL) | yes, same code (not run here) | yes (`TerminateProcess` through `Child::kill`; not run here) |
| Crash isolation (stack overflow, abort) | yes | yes | yes |
| No orphan after the emulator ends | yes (input pipe closes) | yes, same mechanism | yes, same mechanism; a worker stuck in a native call that was started by an emulator that crashed is not ended (no processor-time limit, see below) |
| Address-space limit (`RLIMIT_AS`) | **yes** | **no**: the setting is accepted but not enforced for ordinary allocation, and the current size is not cheaply readable, so it is not set. A script can use memory until the time limit, bounded only by the engine's one-step checks | **no** (no limit on a plain process; a Job Object would be needed) |
| Processor-time limit (`RLIMIT_CPU`) | yes | yes (not run here) | no |
| Emulator watches the worker's memory | not needed (the limit above holds) | no | no |
| Empty environment, only the three standard streams | yes (checked in a test) | environment yes; descriptors: close-on-exec as on Linux (not checked) | environment cleared except `SystemRoot`; handles are not inherited by default (not checked) |

Where an operating system limit does not exist, the emulator's kill at the time limit still holds (so time and slots are bounded everywhere), but memory is not. The code for macOS
and Windows compiles on those targets only in principle: `rlimit` is a Unix-only dependency in `Cargo.toml` and the calls to it are behind `cfg(unix)`; neither platform was built or run.

### First-line limits inside the engine (friendly errors; nothing depends on them)

| Limit | Value | How | Test |
|---|---|---|---|
| Clock inside the engine | the same 5 s | the engine's "run with a budget" mode gives control back every `slice` (96) instruction costs; the driver looks at the clock each time. The callbacks of `map`/`filter`/`forEach`/`reduce`/`some`/`every`/`find` are re-written in `prelude.js` as ordinary script code so that this reaches them | js `work_without_loops_...` |
| Loop iterations | 5,000,000 per function call (`loop_iterations`), 400 | engine setting; counts per call frame | `an_infinite_loop_is_stopped` |
| Recursion | 1000 nested calls (`recursion_depth`), 400 | engine setting (counts nested native-to-script calls too) | `deep_recursion_is_an_error_not_a_crash`, js `recursion_is_limited_whatever_the_shape` |
| One-step builders | strings 32M characters, arrays 5M elements | `repeat`, `padStart`, `padEnd`, `concat`, `join`, `fill`, `Array.from`, `Array.prototype.concat`, `flat`, `appendBody` raise `RangeError` (a script can catch it) | `calls_that_build_a_big_value_in_one_step_are_refused` |
| Value handed to the host | depth 150, 3M nodes, arrays 5M | `plain()` in `prelude.js` | `deeply_nested_values_do_not_reach_native_recursion` |
| Script text | 32 KB, 400 brackets deep (brackets inside the `${...}` of a template literal count, nested templates too), 64 prefix operators in a row | `js::check_source` before the script is sent to a worker | js `script_text_that_could_exhaust_the_parser_stack_is_refused`, `c1_nesting_inside_a_template_literal...` |
| Engine stack in the worker | 128 MiB (`worker::ENGINE_STACK`) | the engine runs on a thread of the worker with that stack (reserved, not touched). Measured in a debug build: the deepest text `check_source` lets through overflows 32 MiB and fits 64 MiB | js `the_deepest_allowed_text_fits_the_workers_stack` |

### What the bounds do not cover

* **Time and memory on macOS and Windows**: see the platform table. Memory is not limited there.
* **The first moments of a runaway**: a script can use the 192 MiB of memory, or all of one processor core, for up to the time limit plus 300 ms before it is stopped. Four workers can do that at once.
* **Text that fools the source check** (a quote inside a regular expression literal hides the rest of the line from the bracket count) still overflows the worker's stack; the result is a 400 for
  that request and a worker that touched up to 128 MiB of stack, nothing more. The check is not the safety any more.
* **A query with a function or a write with a trigger that the client abandoned over HTTP** keeps its worker until the time limit (a stored procedure run is cancelled at once).
* The emulator itself does not limit the worker's number of threads or open its own descriptors beyond the three streams; the worker starts none.
* A worker is a copy of the whole emulator executable (110 MB debug, 20 to 30 MB resident when it runs a script). Four at once is about 100 MB of memory; idle there are none.

## The server-side API

| Built | Notes |
|---|---|
| `getContext()`, `getCollection()`, `getRequest()`, `getResponse()`, `abort(err)` | `abort` rolls back and answers 400 with the reason; it throws too (inside a promise callback the throw is swallowed, the flag stays) |
| `createDocument`, `upsertDocument`, `readDocument`, `readDocuments`, `queryDocuments`, `replaceDocument`, `deleteDocument` | links: `getSelfLink()` (`dbs/{rid}/colls/{rid}/`), `getAltLink()` (`dbs/{db}/colls/{c}`), a document's `_self`, or `{alt link}/docs/{id}`. Another collection's link is 404. A string or a `{query, parameters}` object for queries; `options`: `etag` (as `If-Match`), `ifNoneMatch`, `continuation`, `pageSize` (default 100), `disableAutomaticIdGeneration`; the id is generated when missing. All of them go through the ordinary document handlers, so 409, 412, 413, 400 and unique key violations are as in the REST API |
| Callbacks `(error, resource, options)` | queued and run one at a time after the calling code returns (as Azure does; no deep recursion in a long `tryCreate` chain). `error` is an `Error` with `message` `{"Errors":["..."]}`, `number` (the status), `body`. Without a callback an error is thrown later. Feed callbacks get `options.continuation` (`azemu-script:N`, a position in a result kept for the run) |
| Return value | `true`: accepted (the call is already done, the callback is queued); `false`: not accepted (time nearly used up, `max_operations`, after an abort): no callback will come |
| `getResponse()`: `setBody` (any JSON value), `getBody`, `appendBody`, `getValue`, `setValue`, quota getters | the body is what `setBody` was given, as JSON; none set: 200 with an empty body |
| `getRequest()`: `getBody`, `setBody` (pre-trigger only), `getOperationType`, `getValue`, `setValue` | in a stored procedure `getBody()` is the parameter array |
| `__` and the chained API: `filter`, `map`, `pluck`, `flatten([isShallow])`, `sortBy`, `sortByDescending`, `unwind(selector[, resultSelector])`, `chain()...value()` | written in JavaScript over a read of the partition (`prelude.js`), run as script code. Without callback the result is the response body (first page); with one it gets `(err, feed, {continuation})`, `pageSize` default 100. `unwind` without a result selector gives `[document, item]` tuples (Learn: "[person, pet] tuples"). The result is `{isAccepted}`. Not "optimized to run on the index": it reads every document of the partition |
| `console.log/info/warn/error/debug` | returned in `x-ms-documentdb-script-log-results` when `x-ms-documentdb-script-enable-logging: true`; lines joined with `\n`, at most 1 KB, percent-encoded (the .NET SDK's `ScriptLog` and the Python tests decode it) |
| Request charge, session token, owner headers | charge = 1 + what the operations of the script cost by the usual size formulas; `x-ms-session-token`, `lsn` after the writes are applied |

### Left out

* Attachments (`createAttachment`, `readAttachments`, ...): not asked for; the emulator has no attachments.
* `enableScan`, `enableLowPrecisionOrderBy`, `indexAction`: accepted, ignored. `currentCollectionSizeInMB` options in callbacks: not given.
* The JavaScript query API is not run "on the index": no `__.filter` limits on what is allowed in the predicate.
* Stored procedure RU pre-charging and throttling (no 429 for RUs).
* Errors have no stack trace with line numbers (only the message): the text after `Stack trace:` repeats it.
* Several scripts named in `x-ms-documentdb-pre-trigger-include` run in order (Azure documents one per operation).
* Triggers on patch, transactional batch and bulk calls are not run (Azure has no triggers on those either; not checked).
* Scripts cannot call other scripts, import modules (Azure cannot either), or use `setTimeout`.
* Error status of a thrown collection error: always 400 with `Exception = Error: {"Errors":[...]}` (the shape in Microsoft's data factory troubleshooting page), not the status of the inner error.

## Errors

| | Status | Text |
|---|---|---|
| Script threw / syntax error / `abort` / loop or recursion limit / the engine ran out of stack | 400 `BadRequest` | `Encountered exception while executing function. Exception = Error: boom` (a thrown string: the string) |
| Memory (the worker could not allocate) | 400 | `The script used more than N MB of memory ...` |
| Script too large or too nested, `eval` | 400 | says which |
| Time limit (the engine's clock, or the emulator's kill, or the worker's processor-time limit) | 408 `RequestTimeout` | `The script ran longer than 5 seconds ...` |
| Partition changed five times | 449 `RetryWith` | |
| No partition key on a stored procedure call | 400 | `PartitionKey value must be supplied for this operation.` |
| Engine failure, panic in the worker | 500 | `The script engine failed: ...` |
| The worker cannot be started, ended without a reason the emulator caused (killed from outside, crashed), or sent something that is not a message | 500 | `The script worker could not be started (path): ...`, `The script worker ended unexpectedly (signal: 9 (SIGKILL))`, `... sent something that is not a message` |
| A message from the worker larger than 16 MB, more than `20 * max_operations + 1000` collection calls | 400 | says which |
| Too many scripts at once | 429, `x-ms-retry-after-ms` | |
| Trigger named but missing, of the wrong type (`triggerType`), or registered for another operation (`triggerOperation`) | 400 | names the trigger |

## Triggers

`x-ms-documentdb-pre-trigger-include` / `x-ms-documentdb-post-trigger-include` (comma separated, what every SDK sends) on create, upsert, replace and delete. The
trigger must exist, have the matching `triggerType`, and `triggerOperation` of `All`, or the operation (`Create`; `Replace` or `Update`; `Delete`; an upsert
accepts `Create`, `Replace`, `Update`, `All`). Order inside the copy of the partition:

1. each pre-trigger runs with `getRequest().getBody()` the document (a JavaScript object, not text) and may `setBody`; the new body is what the operation gets
   (the partition key must still match the header);
2. the operation runs with the ordinary handler on the copy (`documents::create` / `replace` / `delete`: so 409, 412 and the rest apply);
3. each post-trigger runs with `getResponse().getBody()` the stored document, may read and write other documents of the partition (the documented metadata sample) and
   may `setBody` to change what the client gets; the stored document is not changed by it;
4. a throw anywhere, or a failed operation: nothing is applied, 400 (or the operation's own error).

`getResponse()` in a pre-trigger raises an error (the reference says it is not available there). `getOperationType()` is `Create`, `Upsert`, `Replace` or `Delete`.

## UDFs

`udf.name(args)` in SQL (SELECT, WHERE, ORDER BY, GROUP BY, subqueries; `crates/cosmos-sql`, see `cosmos-sql.md`). The query is parsed as before, the names it calls are
looked up in the container (an unknown one is a 400 before anything runs; a plan request only parses the query and does not look), and the query runs on a blocking thread. One worker
process serves a whole query (started with the first call): each function is compiled once in it and called once per evaluation as a small pre-compiled script, all calls of the query
sharing the 5 s limit and the kill. A function is pure: its context answers "not accepted" to every collection call. Arguments arrive as JSON (an undefined property as `undefined`, the
`income == undefined` check of the tax sample works); a result that is `undefined`, a function or not finite is undefined in SQL. A throw is a 400 `Encountered exception while
executing function. Exception = no input`. Query plan flags: none are needed (the `QueryInfo` of the SDKs has no UDF field; a plan request with a UDF is parsed like any other, and the
rewritten ORDER BY / aggregate queries keep the call as written), checked with the Python SDK (an aggregate over a UDF filter, which goes through the plan) and the .NET SDK (which always asks for a plan). Kept page results of a query
with a UDF are the same as for any query: if the function is replaced between two pages, the later pages still come from the first run.

## Settings

`ScriptSettings` (public: `azemu_cosmos::ScriptSettings`; `CosmosService::with_script_settings`; `CosmosService::new()` reads the environment):

| Field | Default | Environment variable |
|---|---|---|
| `timeout` | 5 s | `AZEMU_COSMOS_SCRIPT_TIMEOUT_MS` |
| `max_operations` | 10,000 | `AZEMU_COSMOS_SCRIPT_MAX_OPERATIONS` |
| `loop_iterations` | 5,000,000 | `AZEMU_COSMOS_SCRIPT_LOOP_ITERATIONS` |
| `recursion_depth` | 1000 | |
| `memory_growth` | 192 MiB | `AZEMU_COSMOS_SCRIPT_MEMORY_MB` |
| `max_response` | 2 MiB | |
| `max_running` | 4 | `AZEMU_COSMOS_SCRIPT_MAX_RUNNING` |
| `slice` | 96 | |
| `worker` | none: `AZEMU_SCRIPT_WORKER`, else the running executable | `AZEMU_SCRIPT_WORKER` (a path) |
| `kill_grace` | 300 ms | |

`memory_growth` is what a script may add to the worker's address space (Linux). `CosmosService::new()` and the standalone programs read the variables; an embedder that mounts the service in its own
program must call `azemu_cosmos::script_worker_main_if_requested()` first thing in its `main`, or point `ScriptSettings::worker` / `AZEMU_SCRIPT_WORKER` at a program that does (the
`azemu-cosmos-script-worker` program of this crate is one).
