# Security

azemu is a tool for development and testing. It has **no sign-in**: it accepts every credential, on purpose, so that tests need no set-up. This page says who
is trusted, what it protects you from, what it does not, and what it keeps on disk. Each statement was checked against the code or by trying it;
where something was only read, it says so.

## Who is trusted

- **Every program on your machine.** By default the emulator listens on `127.0.0.1` and `::1` only. Any program that can open a connection there can read,
  change and wipe everything in it, and the admin API (`/_azemu/...`) has no authentication either. `--strict-auth` checks bearer tokens and role assignments, to show a program its mistakes ([`docs/strict-auth.md`](docs/strict-auth.md)); it is not a boundary, because the admin API can give any role to anyone.
- **With `--host 0.0.0.0` (or any address other than loopback), or a published container port, everyone who can reach that port.** That includes
  every other container on the same Docker network and, for a port published to all addresses, the local network.

`docker run -p 4577:4577 ...` publishes the port on **all** addresses of the host. `docker run -p 127.0.0.1:4577:4577 ...` publishes it to your own machine only.
Always write the second form (that is Docker's behaviour; it was not tried with the first form here, on purpose). In Compose, give no `ports:` at all when
only other containers need the emulator, or write `"127.0.0.1:10000:10000"`. The image listens on all addresses *inside* the container, so this is the only
thing between it and the network. It runs as user 10001, not root, on a base with no shell.

## What it defends against

The emulator runs next to your browser, so it protects itself from web pages open in it. One place does this for every port, plain and https, http/1.1 and
http/2, and WebSocket upgrades. Programs that are not browsers (every SDK) send neither `Origin` nor `Sec-Fetch-Site` and are not affected.

- **DNS rebinding.** A request is served only when its `Host` is an IP address, `localhost` or `*.localhost`, a name with no dot (a Compose service name),
  `host.docker.internal` or `gateway.docker.internal`, or a name under a suffix you gave with `--tls-name` or `--allow-host`. Any other name gets `403 HostNotAllowed`
  (tried with `Host: evil.example.com`). A page that makes its own name point at `127.0.0.1` has a name with a dot that is not on the list.
- **Requests from other web sites.** The admin paths (`/_azemu/...`) and WebSocket upgrades refuse every request that a browser marks as coming from another
  site (`Sec-Fetch-Site: cross-site`, or an `Origin` that fails the host check): `403 CrossSiteRequestRefused`, no CORS headers (tried: admin reset, health,
  and the managed identity path). A service request that the browser marks `cross-site` is refused unless the service's own CORS rules (Blob, Queue,
  Table) accept the origin, or `--allow-origin` does. An `Origin` header alone does not open a private blob (it still needs credentials, as on Azure).
- **An older browser, and a page on a public address.** A browser before Safari 16.4 or Firefox 90 sends no `Sec-Fetch-Site`, so the `Origin` is all there is. An `Origin` with an IP address
  used to count as the emulator's own whatever the address was: a page on `http://203.0.113.9` could add a fault rule (`POST /_azemu/faults` with `Content-Type: text/plain` and no `Sec-Fetch-Site`).
  Now an `Origin` or `Referer` with an IP address passes only for an address on a private network (loopback, `10/8`, `172.16/12`, `192.168/16`, link-local, unique-local, and `100.64/10`,
  the shared range of Tailscale and carrier-grade NAT), or when it is the very address and port the request was sent to (the emulator on a machine with a public address, and its own
  page). Any other public address is another site: `403 CrossSiteRequestRefused`. (The host check is not changed: a request addressed to an IP address is still served, as DNS
  rebinding needs a name.) A page that runs on a public address and must use the emulator needs `--allow-origin`.
- **A browser must say that the body of an admin request is JSON.** `POST`, `PUT` and `PATCH` on `/_azemu/...` with a body, from a browser, must carry `Content-Type: application/json`
  (`415` otherwise; every admin path, the services' own included, except the sign-in protocol of the identity service, whose requests are forms as OAuth 2.0 says). A page can send a form or a
  `text/plain` request to `localhost` without asking first, and the admin API reads its body as JSON whatever the type says; `application/json` makes a browser ask first, and the answer to
  that is a refusal. It is a second line behind the check of the origin, for a browser that sends neither `Sec-Fetch-Site` nor a usable `Origin` (Firefox before 70 with a page that hides its
  referrer). "From a browser" means a `User-Agent` that holds `Mozilla/`, which every browser sends and which a page cannot change or leave out on a request it makes without asking first.
  A program that is not a browser is not held to the rule (`curl -d` sends a form type), so scripts written for earlier versions keep working. Tried: the five types a page can send and no
  type at all with a browser's `User-Agent` are refused on core's and a service's admin path; the same requests from `curl`'s and from no `User-Agent` go through.
- **The managed identity endpoint** refuses a request without the header `Metadata: true` (`400`), as the real one does.
- **Credentials in the log.** At `--log-level debug` the request log shows headers and query strings; the values of `Authorization`, `x-ms-copy-source-authorization`,
  `x-ms-encryption-key`, `aeg-sas-key`, `aeg-sas-token`, the Service Bus supplementary-authorization headers, cookies, and of the query parameters `sig`, `code`,
  `access_token` and the like, are replaced by `<redacted>` (tried with a bearer token and a SAS signature: neither appeared). No request body is logged at any level.
  This covers HTTP; the AMQP library's `trace` lines hold no payload or SASL credential, and the Kafka endpoint no longer logs the bytes of a SASL request (read,
  not covered by a test that captures the log).

**Options that widen this**, so use them knowingly: `--allow-host <name>` serves one more host name and everything under it (needed when a hosts file maps real
Azure names such as `core.windows.net` to the emulator); `--allow-origin <origin or host>` lets pages from that origin use the services (tried: the service path
passes, the admin paths stay refused); `--allow-origin '*'` turns the cross-site check off, admin paths included; `--tls-name` also serves its names as hosts.

## The inspector page

`GET /_azemu/ui` serves a page (three files compiled into the binary: HTML, one script, one style sheet) and `GET /_azemu/inspect/...` the read-only API behind it
([`docs/inspector.md`](docs/inspector.md)). What was done about it:

- **It shows what the services hold, to whoever can reach the port.** This was already so through the services; the inspector is another way to read it, not a new right. A Key Vault
  secret's value is only in the answer for that one secret (or version) with `?reveal=1`; private key material, shared access keys of Service Bus and Event Hubs, and lock tokens
  are never in an answer.
- **Another web site cannot use it.** The inspector is under `/_azemu/`, so a request a browser marks as coming from another site, or whose `Origin` fails the host check, is refused
  with `403 CrossSiteRequestRefused`, the page's files and `?reveal=1` included (tested with `Sec-Fetch-Site: cross-site` and with `Origin: http://evil.example`, and the answer
  holds no secret). `--allow-origin '*'` turns the check off, as for every admin path.
- **The page cannot be made to run what a test wrote.** Names, message bodies and documents are whatever a test stored and may hold markup. The script builds the page with
  `textContent` and created elements and never with `innerHTML` or the like (a test fails if `app.js` contains `innerHTML`, `outerHTML`, `insertAdjacentHTML`, `document.write`
  or `eval`); blob content is shown as text in a `<pre>` or as hex, never rendered and never navigated to. The page is served with `Content-Security-Policy: default-src 'none';
  script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'` (so no inline script, no inline style,
  nothing from another host, no frame), `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer` and `Cache-Control: no-store`. Tried in Chrome with a blob called
  `<script>alert(1)</script>.txt`: it is shown as text and no script element was made.
- **It only reads.** `POST`, `PUT`, `DELETE` and `PATCH` on `/_azemu/inspect` are `405`; looking creates no account.
- **A row and a page have a size.** One row carries at most 128 KiB (a body at its 64 KiB and the properties around it): a value that does not fit is replaced by `{"cut": true, "bytes": <the
  whole length>, "preview": ...}`, the largest first, and one answer carries at most 4 MiB of rows (a page that would be longer ends early and `next` goes on from there). A table page showed every entity
  whole (tens of MB for entities of 480 KB, copied while the table was locked); it is now cut while it is built and stays under the limit. Cosmos DB's documents keep their
  own, smaller cut (16 KiB, `{"shownInTheNode": true, "bytes": n}`). What a client stored on a stored procedure, a trigger or a function is under `properties`, so that it cannot set the
  `path` or the `name` of its row, from which the page builds a link.

## Requests that ask for a lot of work or memory (Azure Files, Event Hubs Schema Registry, Monitor, Resource Manager, Event Grid)

A request that is small can be made to cost a great deal. These were looked for and tried with a debug build; each is now bounded by a count or a size, not by the hope that a
stack or a heap is big enough. The limits that are the emulator's own say so in their answer.

- **Faults.** A rule is small: `path`, `query` and `account` are at most 1024 bytes, a code 256, a message 2048, a header name 128 and its value 2048, at most 8 headers, at most 1000
  rules. A path pattern is matched in one pass over the path, however many `*` it has (a pattern of 1,000 letters after one `*` against a path of 60,000 took 190 ms per request, and every
  request waited for it, because the rules are asked under one lock). A wait a rule asks for on an AMQP or Kafka
  connection ends when the client closes the connection, so a client that retries against a rule of an hour does not fill the limit of connections.
- **Strict mode: the old internal token.** The token text the emulator's services used between them (`Bearer azemu-arm`, `Bearer azemu-seed`) switched every check off for a request from the
  network that carried it (Blob, Data Lake, Queue, Table, Key Vault, App Configuration and Event Grid took it; a Blob list that strict mode refuses with 401 was answered as for an unchecked request).
  The calls between the services are now marked in the process (a task-local value, set only around the call, that the HTTP server never sets), and the text is an ordinary, invalid
  bearer token. A copy or a rename read the source without asking for a role on it (a principal with a role on `out` only copied `secret/b.txt` into `out`, also by writing the source with
  `-secondary` or an escaped account name, and with `x-ms-copy-source-authorization: Bearer junk`); the source is now resolved as the copy resolves it and needs `read` (and `move` for a rename).
  A token was accepted when only the first label of its audience host matched (`https://acct1.evil.example` for the account `acct1`); the whole host has to be a form the service is known by.
  A SAS for a Service Bus or Event Hubs namespace was accepted by every namespace (they share the development key); its host has to name the namespace, and `se=9223372036854775807` no
  longer overflows the expiry. A connection keeps at most 1,000 credentials from `put-token`.
- **Azure Files.** A file may be declared as large as 4 TiB. A read used to allocate the whole range (a read of a 4 TiB empty file stopped the process); a read of 1 MiB or more is now
  sent in parts as the client takes them, and a hole costs nothing. A read of 1 MiB from the middle of such a file and the start of a full read that is dropped after 1 MiB were
  tried: the memory of the process grew by less than the 256 MB that the test allows (it does not depend on the size of the file). Clearing a range looks only at the stored pages (clearing 4 TiB took 27 s and held the service's lock; it is now
  immediate). A range written as the largest 64-bit number is an answer, not a panic. A rename compares names as the tree keys them (Unicode case folding), and puts the node back when
  it fails: a directory moved into itself in another case used to be lost with its content.
- **Schema Registry.** The compatibility check counts how deep it goes (64 levels, names that lead to records counted) and how many steps it takes (200,000), remembers the pairs of
  records it found compatible, and runs with the namespace's state unlocked. A schema of 9,000 records that lead to one another stopped the process, and one with four fields of the
  same type in each of ten levels took ten seconds with every other Event Hubs request waiting. The JSON reader stops at 128 levels, so a schema of 10,000 nested brackets is a 400.
  One schema has at most 1,000 versions, and finding a version by content does not read the others.
- **Monitor.** Items and log rows are kept as text, so the byte limit counts what is held (a parsed value took thirty times the text: three requests of 19 KB of gzip took the process
  from 39 MB to 76 MB, where the same kind of request had taken it to 466 MB). A request is read one item at a time, has at most 65,536 items, and an item over the length limit is
  refused without being parsed.
- **Resource Manager templates.** The type check of parameters, function arguments and outputs is charged to the request's work budget, and looks names up through an index; `like`
  is linear; `indexOf` does not slice text at a position taken from the lower-cased copy; an ISO 8601 duration is added with checked arithmetic.
- **Event Grid.** A delivery property cannot be named like a header of the connection (`Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `Upgrade`, `TE`, `Trailer`,
  `Expect`, `Keep-Alive`, `Proxy-*`); one that a stored state holds is left out of the request. CR and LF could not get into a header before either.

## What it does not defend against

- **Other users on the same machine, and other containers on the same network.** They can connect like any program. There is nothing to check who they are.
- A program on your machine that reads the files below.
- Nothing is checked for role assignments, tokens or keys (see "What it cannot tell you" in the README): this is a test tool, not a security boundary.

## What is on disk in the clear

- **The certificate directory** (`~/.azemu`, or `--cert-dir`). The emulator creates it with mode 0700 (tried with umask 002: the directory is 700; a parent it has to create on the way gets the default mode). A directory that already exists is left as it is, and when it is open to other users the emulator logs a warning that says so and how to fix it (`chmod 700`). It holds: `ca.pem` (public), `ca.issuing.pem` (public), `ca.meta`, and
  **`ca.key`, mode 0600, the private key of the issuing certificate**. The root's own key is thrown away when it is made and never stored.
- **With `--data-dir`, everything the services hold**, because the files are the state: blobs, queue messages, table entities, Cosmos documents, App
  Configuration values, Service Bus messages, event hub events. In particular **Key Vault secret values and private keys, and the identity signing key, are
  written unencrypted.** The directory is mode 0700 and the state files mode 0600 (tried: a Key Vault secret was found as plain text in `keyvault/keyvault.state`).
  Do not copy a data directory somewhere other people can read it, and do not commit one.
- Without `--data-dir` nothing is written, except the certificate directory.
- On Windows the file modes do not exist; the files get the default permissions of the folder.

## The certificate authority

`ca.pem` is a root; the key that signs certificates belongs to an issuing certificate that carries X.509 name constraints. By default it can sign only for
`localhost`, anything under `.localhost`, and the loopback addresses (read with `openssl x509 -ext nameConstraints`). A stolen `ca.key` can therefore sign
only for those names. A name outside gets no certificate: the handshake ends with an alert and the log says why.

`--tls-name` widens the list. It refuses, at start, a whole top-level domain (`com`, `co.uk`), a wildcard and a very wide address range (tried with `com`).
A single IP address is allowed, public ones too: the name constraint then covers that one address only, no more than a DNS name you list covers, and the address is also put in the certificate that is served when a client
connects to an address and sends no name. A public address logs a warning at start, because a client that trusts `ca.pem` would accept that address as the emulator. Do not list an address you do not control.
**A CA made with `--tls-name core.windows.net` can sign for `*.core.windows.net`, and must never be installed system-wide**: the whole machine would trust
whoever can read `ca.key` as the real Azure storage. Trust such a CA in one client at a time (`docs/reaching-the-emulator.md`). Installing the CA system-wide
is fine only for the default names. Do not set `SSL_CERT_FILE` or `REQUESTS_CA_BUNDLE` to `ca.pem` for a whole shell: they replace the system list.

The CA lasts 2 years and is replaced at the first start after it ends or after the set of `--tls-name` changes; clients must then trust the new `ca.pem`.

## Outbound calls: Event Grid webhooks

An Event Grid subscription with a webhook makes the emulator call a URL. By default a target must be a loopback or private address (`127.0.0.0/8`, `::1`,
`10/8`, `172.16/12`, `192.168/16`, `100.64/10`, IPv6 `fc00::/7` and `fe80::/10`; not `169.254.0.0/16`, where cloud metadata services answer), a name with no dot,
`localhost` or `*.localhost`, `host.docker.internal`, or a name the host check serves. Any other target is refused with `400 BadRequest` when the subscription is
made (tried with `https://example.com/hook`), and the message names the option. The rule is applied again when a delivery connects, so a name that changes what
it points to cannot lead the emulator to a public address. `--allow-webhook-host <name>` (repeatable; `*` allows every host) opens more. An `https` webhook is
called without checking its certificate. This is the only place the emulator makes outbound calls.

Without that limit, anyone who can reach an emulator listening on a network address could make it call addresses only the emulator can reach; that is why the limit is on by default.

## Cosmos DB stored procedures: client-written JavaScript

Stored procedures, triggers and user-defined functions are JavaScript sent by the client. They run in a **separate worker process** (the same program
started again), never inside the emulator, so a script cannot reach the emulator's memory or end it. The worker gets an empty environment, no files and no
sockets (only its three standard streams), and sees a private copy of one partition. The emulator kills it at the time limit (5 seconds by default) and when the
request is cancelled or the emulator ends. Limits (all in `docs/SETTINGS.md`, `AZEMU_COSMOS_SCRIPT_*`):

| Limit | Linux | macOS | Windows |
|---|---|---|---|
| Time (5 s), kill, slots (4 at a time) | yes | yes (same code; not run) | yes (not run) |
| Memory (192 MiB beyond the engine's start) | yes (address-space limit) | **no** | **no** |
| Processor time (time limit + 3 s) | yes | yes (not run) | **no** |

Tried: a stored procedure ran, and `while(true){}` was stopped after under a second. On macOS and Windows a runaway script is limited in time but not in
memory. A build made without the `cosmos-scripts` feature has no script engine at all and answers 501.
