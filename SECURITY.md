# Security

azemu is a tool for development and testing. It has **no sign-in**: it accepts every credential, on purpose, so that tests need no set-up. This page says who
is trusted, what it protects you from, what it does not, and what it keeps on disk. Each statement was checked against the code or by trying it;
where something was only read, it says so.

## Who is trusted

- **Every program on your machine.** By default the emulator listens on `127.0.0.1` and `::1` only. Any program that can open a connection there can read,
  change and wipe everything in it, and the admin API (`/_azemu/...`) has no authentication either.
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
- **The managed identity endpoint** refuses a request without the header `Metadata: true` (`400`), as the real one does.
- **Credentials in the log.** At `--log-level debug` the request log shows headers and query strings; the values of `Authorization`, `x-ms-copy-source-authorization`,
  `x-ms-encryption-key`, `aeg-sas-key`, `aeg-sas-token`, the Service Bus supplementary-authorization headers, cookies, and of the query parameters `sig`, `code`,
  `access_token` and the like, are replaced by `<redacted>` (tried with a bearer token and a SAS signature: neither appeared). No request body is logged at any level.
  This covers HTTP; the AMQP library's `trace` lines hold no payload or SASL credential, and the Kafka endpoint no longer logs the bytes of a SASL request (read,
  not covered by a test that captures the log).

**Options that widen this**, so use them knowingly: `--allow-host <name>` serves one more host name and everything under it (needed when a hosts file maps real
Azure names such as `core.windows.net` to the emulator); `--allow-origin <origin or host>` lets pages from that origin use the services (tried: the service path
passes, the admin paths stay refused); `--allow-origin '*'` turns the cross-site check off, admin paths included; `--tls-name` also serves its names as hosts.

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
