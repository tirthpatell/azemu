# Faults: make the emulator fail on purpose

Azure throttles, is slow, and drops connections now and then. A program has retry and back-off code for that, and without help the code is never run: the emulator
answers every request at once and never says no. A **fault rule** tells the emulator to misbehave in a way you chose, so a test can see what the program does.

Nothing changes until you add a rule. With no rule the emulator does what it always did, and the emulator never throttles by itself.

- [Three examples](#three-examples)
- [The admin API](#the-admin-api)
- [A rule](#a-rule)
- [The kinds of fault](#the-kinds-of-fault)
- [What each service answers](#what-each-service-answers)
- [Service Bus, Event Hubs and Kafka](#service-bus-event-hubs-and-kafka)
- [A file of rules at start](#a-file-of-rules-at-start)
- [What the SDKs did](#what-the-sdks-did)
- [What is not possible](#what-is-not-possible)
- [Behaviour that could not be confirmed against Azure](#behaviour-that-could-not-be-confirmed-against-azure)

## Three examples

**1. Blob: the first two downloads are throttled.**

```bash
curl -X POST localhost:4577/_azemu/faults -H 'Content-Type: application/json' -d '{
  "service": "blob", "method": "GET", "path": "/reports/*", "times": 2,
  "fault": {"kind": "throttle", "retryAfterSeconds": 1}
}'
# {"id":1,"service":"blob","method":"GET","path":"/reports/*","times":2,"fault":{...},"matched":0,"fired":0}
```

```python
from azure.storage.blob import BlobServiceClient

client = BlobServiceClient.from_connection_string(conn_str, retry_total=4, initial_backoff=1)
client.get_blob_client("reports", "march.csv").download_blob().readall()   # two 503 ServerBusy, then the data
# with retry_total=0 the same call raises HttpResponseError: status_code 503, error_code "ServerBusy"
```

`GET /_azemu/faults` now shows `"fired": 2` for rule 1, and the request log on standard error says `fault=throttle rule=1` for the two requests.

**2. Cosmos DB: a budget of 50 request units a second.**

```bash
curl -X POST localhost:4577/_azemu/faults -H 'Content-Type: application/json' -d '{
  "service": "cosmos", "account": "myacct",
  "fault": {"kind": "ruBudget", "ruPerSecond": 50}
}'
```

A loop of reads is let through until the request charges of the current second add up to 50. The next request gets `429` with `x-ms-substatus: 3200` and
`x-ms-retry-after-ms` set to the time left in the second. The Cosmos SDKs wait that long and retry, so a loop that is slowed down by it still ends well; a program with
its retries turned off sees the `429`.

**3. Service Bus: every send to one queue is refused as "server busy", twice.**

```bash
curl -X POST localhost:4577/_azemu/faults -H 'Content-Type: application/json' -d '{
  "service": "servicebus", "operation": "send", "path": "orders", "times": 2,
  "fault": {"kind": "throttle"}
}'
```

```python
with ServiceBusClient.from_connection_string(conn_str, retry_total=5) as client:
    with client.get_queue_sender("orders") as sender:
        sender.send_messages(ServiceBusMessage("hello"))     # refused twice, then stored once
```

With `retry_total=0` the call raises `ServiceBusServerBusyError`. Nothing was stored by the sends that were refused.

## The admin API

All paths are under `/_azemu/`, on the gateway or on any service host. The fault endpoints are listed by `GET /_azemu/`. **Admin paths are never faulted**, so a test can
always remove its rules.

| Request | What it does |
|---|---|
| `GET /_azemu/faults` | `{"rules": [...]}`: every rule with its `id`, what `matched` (requests the rule matched, including those it let through) and how often it `fired` |
| `POST /_azemu/faults` | adds one rule (the body is the rule); answers `201` with the rule and its `id` |
| `PUT /_azemu/faults` | replaces the whole list with `{"rules": [...]}`. Every rule is checked first; one bad rule changes nothing. Ids start again at 1 |
| `DELETE /_azemu/faults` | removes every rule; answers `{"removed": n}` |
| `DELETE /_azemu/faults/{id}` | removes one rule; `404 FaultRuleNotFound` when there is none |
| `POST /_azemu/reset` | with no `service` or `account` parameter, also removes the rules that came from the API. Rules from a file ([below](#a-file-of-rules-at-start)) come back. `?service=` and `?account=` leave rules alone |

`GET /_azemu/config` shows `faultRules` (how many rules there are) and `faultsFile`.

A rule is checked strictly. A field that does not exist, a service that is not mounted, a status that is not 4xx or 5xx, a probability outside 0 to 1, and the
mixes that cannot work (below) are all `400 InvalidFaultRule`, and the message says which rule and which field. A typing mistake in a rule must not make a test pass
by never firing.

At most 1000 rules are held, and a rule is small: `path`, `query` and `account` are at most 1024 bytes, `fault.code` 256, `fault.message` 2048, a header name 128 and its value 2048, and a rule has at most 8
headers (a `400 InvalidFaultRule` that names the limit says so; these are limits of the emulator). A `*` in a path matches any run of characters, and a pattern costs one pass over the path however it is written.

**The body of `POST` and `PUT` is JSON.** Send it with `Content-Type: application/json`, as the examples do. A request **from a browser** (its `User-Agent` holds `Mozilla/`) with a body of
another type, or of none, is refused with `415 UnsupportedMediaType`, here and on every other admin path: a web page can send a form or `text/plain` request to `localhost` without asking
first, and the admin API would read it as JSON; `application/json` makes the browser ask first, and the answer to that is a refusal ([`../SECURITY.md`](../SECURITY.md)). A program that is not
a browser is not held to it, so `curl -d` (which sends a form type unless told otherwise) works.

## A rule

Every matcher is optional. A rule with none matches every request of every service.

```json
{
  "service": "blob",
  "account": "myacct",
  "method": "PUT",
  "path": "/orders/*",
  "query": "comp=block",
  "operation": "send",
  "probability": 1.0,
  "skip": 0,
  "times": 3,
  "fault": { "kind": "throttle", "retryAfterSeconds": 2 }
}
```

| Field | Meaning |
|---|---|
| `service` | the name in the ready line: `blob`, `dfs`, `queue`, `table`, `file`, `keyvault`, `appconfig`, `cosmos`, `eventgrid`, `servicebus`, `eventhubs`, `kafka`, `arm`, `identity` (and any other service the build mounts). A name that is not mounted is refused. `blob` does not cover `dfs`: they are two services |
| `account` | the storage account, vault, store, Cosmos account, namespace, or subscription, as the service sees it. Not case sensitive |
| `method` | HTTP only. Not case sensitive |
| `path` | HTTP: a pattern on the path **as the service sees it**. For a host-style storage URL (`acct.blob.core.localhost`) that is `/container/blob`, without the account; on a pinned port in the path style (`127.0.0.1:10000/devstoreaccount1/container`) the account is taken off as well. `*` matches any run of characters, also `/` and nothing. The pattern is matched against the path as sent and with the percent escapes undone. It starts with `/` (or with `*`). Messaging: the entity, [below](#service-bus-event-hubs-and-kafka) |
| `query` | HTTP only. The query string must hold every pair given: `comp=block`, or `comp=block&blockid=A*`. A bare name (`comp`) accepts any value. Escapes are undone; `*` works in a value |
| `operation` | messaging only: `connect`, `send`, `receive` or `management` |
| `probability` | 0 to 1, default 1. Drawn for each request that is past `skip` and not yet spent |
| `skip` | let this many matching requests through first. "The third call fails" is `"skip": 2, "times": 1` |
| `times` | then fire this many times and stop matching. Absent: no limit. A draw that does not fire (probability) does not use one up |
| `fault` | what happens |

The rules are looked at in the order they were added, and **the first rule of any kind but `latency` that fires decides the answer**. A `latency` rule that fires adds its
wait, and the next rules are still looked at (waits add up). A rule that is spent (`times`) is passed over.

Mixes that are refused: `operation` with `method` or `query`; `operation` on an HTTP-only service; `method` or `query` on `kafka`; a path that starts with `/` on a messaging rule
(an entity is not a path) and a path with no leading `/` on an HTTP-only service; `throttle` and `error` on `kafka`; `ruBudget` on anything but `cosmos`, or with `skip`, `times`
or `probability`; `times` of 0.

The rule is applied after the request has been read and before the service sees it. So a throttled request is refused **before its credential is looked at**: a request
with no credential gets the throttle, not a 401. Nothing is stored by a request that a rule answered or dropped.

`probability` and `jitterMs` use a random generator seeded from the clock. A program that starts the emulator in process can fix the seed
(`EmulatorBuilder::faults_seed`) for a repeatable sequence.

## The kinds of fault

| `kind` | Fields | What happens |
|---|---|---|
| `throttle` | `retryAfterSeconds` or `retryAfterMs` (one of them, both optional) | the answer that service gives when it throttles ([table](#what-each-service-answers)), with the service's own retry header set from the wait |
| `error` | `status` (400 to 599, required), `code`, `message`, `retryAfterSeconds` or `retryAfterMs`, `headers` (name to value) | an answer the rule spells out, in the service's own error shape. With no `code` the code is the one that service uses for that status (`ServerBusy` for 503, `InternalError` for 500 on storage). `headers` are added to the answer and replace the service's own. `x-azemu-fault` cannot be replaced |
| `latency` | `ms` (required), `jitterMs` | wait `ms` plus a random 0 to `jitterMs`, then handle the request as usual. At most an hour |
| `drop` | none | close the connection with no answer: the client sees an unexpected end of the stream or a reset. On HTTP/2 the whole connection ends, not one stream |
| `timeout` | `ms` | read the request and never answer: hold it for `ms`, then close the connection. With no `ms` the request is held until the client gives up, at most an hour |
| `ruBudget` | `ruPerSecond` | Cosmos DB only, [below](#cosmos-db-ru-budget) |

An answer that a rule made carries `x-azemu-fault: throttle` (or `error`, or `ruBudget`) and the request log line (standard error, one per request) says what fired:

```
INFO request service=blob account=acct method=GET path=/reports/march.csv status=503 ms=1 fault=throttle rule=1
INFO request service=blob ... status=200 ms=502 fault=latency rule=2
INFO request service=blob ... status=503 ms=31 fault=latency,throttle rule=2,3
```

A request that was dropped or held logs `status=0`. A request that only got a `latency` has no `x-azemu-fault` header, because its answer is the service's own.

### Cosmos DB: RU budget

`{"kind": "ruBudget", "ruPerSecond": 400}` on a Cosmos rule (`"service": "cosmos"`; `account` and `path` narrow what is counted). The emulator adds up the request charge it
already reports (`x-ms-request-charge`) of the requests the rule matched, per second. While the sum of the current second is below the budget a request goes through and its
charge is added after the answer; once it has reached the budget the request is answered `429`, with `x-ms-retry-after-ms` pointing at the next second. The count starts again
every second, as Cosmos's does. A request may overdraw the budget (the one that reaches it is not refused), as a real container allows.

The request charges are the emulator's own numbers (the README says they are made up), so this tests how a program behaves when it is throttled, not how many request units it
will really use.

## What each service answers

For `throttle`. `error` uses the same shape with the status, code and message of the rule. The sources are what the answers were written from; where a part has none it is
a guess, and it is listed [at the end](#behaviour-that-could-not-be-confirmed-against-azure).

| Service | Answer | Retry header | Source |
|---|---|---|---|
| Blob, Queue | `503`, code `ServerBusy` ("The server is currently unable to receive requests. Please retry your request."), the XML error body, `x-ms-error-code`, the usual `x-ms-*` headers | `Retry-After` in seconds, only when the rule gives a wait | [Common REST API error codes](https://learn.microsoft.com/en-us/rest/api/storageservices/common-rest-api-error-codes): `ServerBusy`, 503, that text. The page names no retry header; the Azure SDKs read `Retry-After` on a 503 |
| Data Lake (`dfs`) | the same code and status as Blob, in the JSON form of the `dfs` endpoint | same | same, and the shape the `dfs` endpoint uses for its other errors |
| Table | `503` `ServerBusy` as OData JSON (`odata.error`) | same | same page (Table Storage shares the common codes) |
| Cosmos DB | `429`, code `429`, body `Message: {"Errors":["Request rate is large. More Request Units may be needed, so no changes were made. Please retry this request later. Learn more: http://aka.ms/cosmosdb-error-429"]}`, `x-ms-substatus: 3200` | `x-ms-retry-after-ms` in milliseconds. Azure always sends it, so a throttle with no wait gets 100 | [Common response headers](https://learn.microsoft.com/en-us/rest/api/cosmos-db/common-cosmosdb-rest-response-headers) (`x-ms-retry-after-ms`), [Troubleshoot request rate too large](https://learn.microsoft.com/en-us/azure/cosmos-db/troubleshoot-request-rate-too-large) ("Request rate is large. More Request Units might be needed, so no changes were made."), and the SDKs, which retry a 429 by waiting for that header |
| Key Vault | `429`, `{"error":{"code":"Throttled","message":..}}` | `Retry-After` in seconds | [Key Vault throttling guidance](https://learn.microsoft.com/en-us/azure/key-vault/general/overview-throttling): the vault "returns HTTP status code 429 (Too many requests)". The body is a guess |
| App Configuration | `429`, `application/problem+json`: `{"type":"https://azconfig.io/errors/too-many-requests","title":"Resource utilization has surpassed the assigned quota","policy":"Total Requests","status":429}` | `retry-after-ms` in milliseconds; a throttle with no wait gets 10 | [REST API throttling](https://learn.microsoft.com/en-us/azure/azure-app-configuration/rest-api-throttling): the page shows this answer, with `retry-after-ms: 10` |
| Event Grid | `429`, `{"error":{"code":"TooManyRequests","message":..}}` | `Retry-After` in seconds | the usual Azure answer; the body is a guess |
| Resource Manager | `429`, `{"error":{"code":"TooManyRequests","message":..}}` | `Retry-After` in seconds | [Understand how Azure Resource Manager throttles requests](https://learn.microsoft.com/en-us/azure/azure-resource-manager/management/request-limits-and-throttling): "429 Too many requests", "The response includes a Retry-After value". The code and text are a guess |
| Identity (token endpoints) | `429`, `{"error":"temporarily_unavailable","error_description":..}` | `Retry-After` in seconds | a guess |
| Service Bus administration API (HTTP) | `429`, the Atom error `<Error><Code>429</Code><Detail>ServerBusy. ...</Detail></Error>` | `Retry-After` in seconds | a guess |
| Event Hubs, Kafka (their HTTP side) | the generic answer: `429`, `{"error":{"code":"TooManyRequests",..}}` | `Retry-After` in seconds | none. Their admin paths are never faulted; only a request to an `{ns}.eventhub.localhost` host (which answers 501 anyway) can reach it |
| Service Bus, Event Hubs (AMQP) | `com.microsoft:server-busy`, [below](#service-bus-event-hubs-and-kafka) | none | the condition the Service Bus SDKs name (`ServiceBusServerBusyError`) |
| Kafka | no answer of this kind: `latency`, `drop` and `timeout` only | | |

Retry headers: `Retry-After` takes whole seconds, so a wait in milliseconds is rounded up there.

The Azure SDKs' retry policies read `retry-after-ms`, `x-ms-retry-after-ms` and `Retry-After` (azure-core for Python: `RetryPolicy.get_retry_after`; the Rust and Go
core libraries read the same three), on a 429 or a 503.

## Service Bus, Event Hubs and Kafka

Over AMQP there are no HTTP statuses, so `operation` says what the client is doing, and `path` is the **entity**: the queue, topic or event hub name, as the client gave it
(`orders`, `orders/Subscriptions/billing`, `hub/Partitions/0`, `hub/ConsumerGroups/cg/Partitions/0`). A pattern matches the whole address or its first part, so `orders` covers
all the addresses under it. `account` is the namespace, `service` is `servicebus` or `eventhubs`.

| `operation` | Asked when | `throttle` or `error` | `latency` | `drop` and `timeout` |
|---|---|---|---|---|
| `connect` | the client's `open` arrives (a new connection) | the connection is closed with the error | `open` is answered late | the socket is closed, or held and then closed |
| `send` | a message arrives on a link to an entity | the transfer is settled `rejected` with the error | the settlement is late | |
| `management` | a request arrives on `$management` (peek, schedule, rules, ...) | the request is settled `rejected` with the error | late | |
| `receive` | a client attaches a link to receive from an entity | the attach is refused with the error (a detach carrying it) | the attach is answered late | |

The `$cbs` node (the token exchange) and transaction links are never faulted. The condition of a `throttle` is `com.microsoft:server-busy`, with the description "The request was
terminated because the entity is being throttled." (and "Please wait n seconds and try again." when the rule gives a wait; the wait is only in the text, AMQP has no header
for it). An `error` rule uses its `code` as the condition if it gives one, else one from the status: 404 `amqp:not-found`, 401 and 403 `amqp:unauthorized-access`, 408
`com.microsoft:timeout`, 429 and 503 `com.microsoft:server-busy`, 400 `amqp:invalid-field`, anything else `amqp:internal-error`.

Why these forms. What a client does with the error decides which form is useful; the Python Service Bus client (7.15.0) was tried with each:

- A `send` that is `rejected` with `server-busy` is raised as `ServiceBusServerBusyError`, and the client retries it.
- A refused connection and a refused attach are retried too.
- `receive` could have been asked for each grant of credit and answered by detaching the link. That was tried first: the client raised `ServiceBusServerBusyError` and did **not**
  attach again, so the error came back until its retries were used up. Refusing the attach is the form it recovers from, and it is what `receive` means here: **a receiver
  that is already attached is not affected by a rule added later**. Add the rule before the receiver is opened, or use `connect` or `send` to disturb the connection.
- A `management` request that is refused is raised, and in the same version the client did not recover on retry (the management link stayed failed). A real service answers a
  management request with a status in the response message instead; the emulator does not build that answer.
- Over a `drop`, the client opened a new connection and went on.
- The Python **Event Hubs** client (azure-eventhub 5.15.1) behaves differently: a `send` that is refused is raised as `EventDataSendError` and **not retried**, with retries on or
  off. That was the same for the three forms tried: the transfer settled `rejected`, the link detached with the error, and the connection closed with the error. It does retry a refused
  connection (`connect`). So a retry test of an event hub send needs a client that retries on its own (a loop in the program), not the SDK's policy.

The Python client waits by its own back-off schedule between retries; nothing in the AMQP answer tells it how long to wait.

**Kafka** (the Kafka endpoint of Event Hubs): `latency`, `drop` and `timeout` only; a rule of another kind is refused when it names `kafka` and left out when it names no service.
The operation is `connect` for a new connection, `send` for a produce request, `receive` for a fetch request and `management` for everything else. `path` is the first topic the
request names (empty for most `management` requests). A `timeout` reads the request and answers nothing for the time given, then closes the connection. No Kafka error answer is
built.

A wait on a messaging connection (`latency`, `timeout`) ends when the client closes the connection: a client that gives up and tries again does not keep a place in the limit of connections for the length of the rule, and removing the rule does not have to free them. What the client sends during a `latency` wait is kept and handled after it.

## A file of rules at start

`azemu --faults faults.json` (or `AZEMU_FAULTS=faults.json`) reads `{"rules": [...]}`, the same body as `PUT /_azemu/faults`, before anything listens. The rules exist from the
start and are put back, with the ids they had, by every full `POST /_azemu/reset` (as `--seed` does for seeded things). `DELETE /_azemu/faults` removes them too, until the next
full reset. A file that cannot be read, is not JSON, has a field that is not known, or names a service that is not mounted stops the start, and the message names the file and the
rule:

```
the faults file 'faults.json': rules[1]: unknown field `servce`, expected one of `service`, `account`, `method`, ...
```

In a container: `-e AZEMU_FAULTS=/faults.json -v ./faults.json:/faults.json:ro`.

## What the SDKs did

Tried against this build (Python 3.13 with the packages of `tests/sdk-python/requirements.txt`, Go with the modules of `tests/sdk-go`, Rust with the crates of
`tests/sdk-rust`). The tests are `tests/sdk-python/faults`, `tests/sdk-go/faults`, `tests/sdk-rust/tests/faults.rs`, `crates/azemu/tests/faults.rs` and `crates/core/tests/faults.rs`.

| Client | Throttled twice (`times: 2`) | Retries off |
|---|---|---|
| Python Blob 12.31 | retried, then succeeded. It waited by its own schedule (`initial_backoff`, `increment_base`), **not** by `Retry-After`: two 1 s waits took about 5 s with `initial_backoff=1, increment_base=2` | `HttpResponseError`, `status_code` 503, `error_code` `ServerBusy` |
| Python Queue 12.18 | the same | the same |
| Python Table 12.7 | retried, then succeeded | `HttpResponseError`, 503, `x-ms-error-code: ServerBusy` |
| Python Cosmos 4.17 | waited for `x-ms-retry-after-ms` (two 0.6 s waits took 1.2 s). **Only when `retry_fixed_interval` is not set**: with it, the header is ignored. A budget rule (`ruBudget`) was waited out the same way | `CosmosHttpResponseError`, 429, `sub_status` 3200. Note `retry_throttle_total=0` means "the default of 9" for this client; 1 is the smallest |
| Python Key Vault Secrets 4.12 | waited for `Retry-After` (two 1 s waits took 2.0 s) | `HttpResponseError`, 429, `error.code` `Throttled` |
| Python App Configuration 1.10 | waited for `retry-after-ms` (two 0.7 s waits took 1.4 s) | `HttpResponseError`, 429; the body is the problem document |
| Python Service Bus 7.15 | a rejected send was retried and then stored once | `ServiceBusServerBusyError` (see above for `receive` and `management`) |
| Python Event Hubs 5.15 | a refused connection was retried; a refused send was **not** (`EventDataSendError`, see above) | the same |
| Go Blob 1.8 (`azcore`) | waited for `Retry-After` (two 1 s waits took 2.0 s) | `ResponseError`, 503, `bloberror.ServerBusy` |
| Go Cosmos 1.5 | waited for `x-ms-retry-after-ms` (1.2 s) | `ResponseError`, 429, sub status header 3200 |
| Rust Blob 1.1 | waited for `Retry-After` (2.0 s) | `http_status()` 503, `error_code` `ServerBusy` |
| Rust Cosmos 1.1.0-beta.2 | waited for `x-ms-retry-after-ms`; with a throttle that never ended the read failed with 429 | 429 |

A **dropped connection**: Python Blob, Key Vault, Service Bus and Cosmos retried it and went on; with retries off the Python Blob client raised a transport error
(`ServiceResponseError` or `ServiceRequestError`); Rust retried it (an `Io` or `Connection` error) and with retries off returned it. Go's `net/http` repeats a `GET` that was sent on a kept-alive connection the
server then closed without an answer, before the SDK's retry policy sees anything; the Go test uses a client with no keep-alive to show the policy's own retry.

**Latency** longer than the client's timeout failed the call at the client's own time (Python `read_timeout`, Go context deadline, Rust `tokio::time::timeout`, Service Bus
`try_timeout`); with a longer timeout the call worked, late.

**Kafka**: tried with a hand-written `ApiVersions` request, not with a Kafka client: `latency`, `drop`, `timeout` and a dropped connection were seen as written above.

Not tried: the .NET, Java and JavaScript SDKs, the Kafka clients, an Event Hubs consumer, the Python Event Grid and Resource Manager clients, and the Data Lake and Table clients in Go and
Rust. The answers of those services were checked with plain HTTP requests (`crates/azemu/tests/faults.rs`), not with their SDKs.

## What is not possible

- **Kafka error answers.** A Kafka connection can be slowed, held or dropped; it cannot be given a `throttle` or an error code.
- **A throttled receive on a link that is already attached**, and a `management` request answered with a status in the response message (see above).
- **Faulting what the emulator sends out**: Event Grid deliveries to webhooks and to queues, and the events Blob, Key Vault and App Configuration raise, are not requests a rule can
  see.
- **Part of an answer.** A rule changes or replaces a whole answer; it cannot cut a download in the middle or corrupt a body.
- **Retry timing on AMQP.** The error carries no wait the clients read.
- **A fault on one HTTP/2 stream.** `drop` and `timeout` close the connection.
- **Faulting the admin API**, by design.

The emulator also never throttles by itself: no request units are counted unless a `ruBudget` rule is there, and no quota is checked.

## Behaviour that could not be confirmed against Azure

- The body and `code` of a Key Vault `429` (`Throttled`), an Event Grid `429` and a Resource Manager `429` (`TooManyRequests`), the token endpoint's `temporarily_unavailable`, and
  the Atom error of the Service Bus administration API. The documentation names the status and the retry header, not the body.
- Whether Blob, Queue and Table send `Retry-After` with a `503 ServerBusy`. The emulator sends it only when a rule asks.
- The Cosmos `429` body's `code` (`"429"`), the sub status `3200`, and the default wait of 100 ms are from memory of the SDKs and of the service, and were not checked against a recorded
  answer.
- The description text of `com.microsoft:server-busy`, and which operations a real Service Bus namespace throttles with it.
- The management answer of Service Bus to a throttled request (a status in the response message), which is not built.
- A Cosmos request is charged what the emulator reports, not what Azure would charge, so `ruBudget` counts different numbers than a real container of the same size.
