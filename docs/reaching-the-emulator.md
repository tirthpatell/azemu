# Reaching the emulator

The first thing a new team trips over is not a bug in a service: it is the address. This page says how a client finds the emulator, which way works
where, and how to make a client trust the emulator's certificate for https. Everything here was tried on Linux (with Docker) unless it
says "not tried".

## Three ways in

| Way | Looks like | Works where | Start the emulator with |
|---|---|---|---|
| Names under `.localhost` on the gateway port | `http://myacct.blob.core.localhost:4577` | where the system resolves any name under `.localhost` to the local machine: a Linux desktop. Not inside most containers (Debian and Alpine images do not resolve it), and not on Windows or on macOS before 26. `curl`, Chrome and Firefox resolve such names themselves, so a quick check with them can look fine when an SDK still fails | nothing: this is the default |
| Standard ports | `http://azemu:10000/devstoreaccount1` | everywhere: the port picks the service, so any host name works (`localhost`, `azemu`, a Compose service name, an IP address) | `--standard-ports` (or `AZEMU_STANDARD_PORTS=1`; the Docker image does this by default) |
| Pinned ports | `--pin blob=10000` | everywhere, for the services you pin | `--pin <service>=<port>`, repeatable |

When the emulator starts and `azemu-self-test.blob.core.localhost` does not resolve on that system, it logs one warning that names these ways out. A
request whose host names no service gets `404 NoServiceMatched`, and the answer says which URLs the services use.

Another way, not tried here: a line in the hosts file (`127.0.0.1 myacct.blob.core.localhost`), or a Compose network alias, for each name a client uses.

### Standard ports

| Port | Service | How the account is chosen |
|---|---|---|
| 10000 | Blob | first path segment: `http://azemu:10000/{account}/{container}/{blob}`, the Azurite address (`UseDevelopmentStorage=true` works) |
| 10001 | Queue | first path segment |
| 10002 | Table | first path segment |
| 10003 | Data Lake | first path segment |
| 10010 | Key Vault | https; the host name is the vault: `https://azemu:10010` is the vault `azemu` (`localhost` or an address: the vault `default`) |
| 10011 | App Configuration | https; the host name is the store, the same way |
| 10012 | Event Grid | https; the host name is the topic (`/api/events`), the same way |
| 10013 | Resource Manager | https; no account |
| 10014 | Identity | http; no account |
| 8081 | Cosmos DB | https; the host name is the account (`localhost` or an address: `localhost`, the address of Microsoft's emulator) |
| 5672 and 5671 | Service Bus and Event Hubs (AMQP, plain and TLS) | the first label of the host is the namespace (`sb://azemu:5672`); a host with the label `eventhub` is Event Hubs, so give the emulator a network alias `eventhub`. The Service Bus administration client uses https on the same port. Give Service Bus and Event Hubs namespaces different names: a namespace that has an event hub is Event Hubs (`sb://localhost:5672` is the namespace `default`). A client that signs in with an identity gives the host without a port, and the SDK then dials 5671 |
| 9093 | Kafka endpoint | raw protocol |
| 443 | Resource Manager over https, and AMQP in WebSocket for Service Bus and Event Hubs | opened only when the process may bind it (it may in the container image; on a laptop it usually may not, and the emulator then logs one warning and starts without it). Identity-based Service Bus and Event Hubs clients that take no port use 5671 or 443 |
| 4577 | the gateway | also on every port above: `/_azemu/...` (health, CA, reset) |

`--pin` for a service keeps your port and takes that service out of this list. If a standard port is taken, the start fails and says so.

Two things to know:

- On a port whose service takes its account from the host (Key Vault, App Configuration, Event Grid, Cosmos), a second network alias is a second vault, store,
  topic or account.
- The Key Vault SDKs refuse a challenge whose resource is not the vault's own domain. The emulator answers with the request's own domain, which passes for a
  host with two labels or more (`myvault.vault.localhost:4577`, `sub.localhost:10010`) in all six SDKs. A host with a single label (`https://azemu:10010`)
  still needs the client's switch (`verify_challenge_resource=False` in Python and its twins in other languages): see `docs/services/keyvault.md`.

## From another container

Other containers reach the emulator by its container or Compose service name. Start it with `--standard-ports` (the image does) and tell it its own name
for https (`AZEMU_TLS_NAME=azemu`). The tested example is `examples/compose/compose.yaml`:

```
docker build -t azemu:latest .                    # once, from a checkout (not re-run for this page)
cd examples/compose
docker compose up --build --abort-on-container-exit --exit-code-from client
```

It ended with exit code 0 and one `ok` line each for Blob (`azemu:10000`), Queue (`azemu:10001`) and Key Vault (`https://azemu:10010`). The client container
fetches the emulator's CA from `http://azemu:10000/_azemu/ca.pem`, so nothing is copied between containers. Do not publish the emulator's ports to other
machines: `ports: ["127.0.0.1:10000:10000"]`, or no `ports:` at all.

## In CI

`examples/ci/gitlab-ci.yml` runs the emulator as a `services:` entry with the alias `azemu`; `examples/ci/github-actions.yml` runs it as a service container
with published ports and reaches it on `localhost` (`http://localhost:10000/devstoreaccount1`). `examples/ci/check.sh gitlab` and `check.sh github` reproduce
both arrangements on one machine with Docker (`AZEMU_IMAGE=azemu:latest examples/ci/check.sh gitlab`: three `ok` lines, exit 0, and the same for `github`).
The two YAML files were not run on a GitLab or GitHub server. For a Rust project whose tests start the emulator themselves, see `examples/ci/github-actions-rust.yml` and [`rust.md`](rust.md#9-ci).

## https and the emulator's certificate authority

Azure SDKs refuse to send a bearer token over plain http, and Key Vault, App Configuration, Cosmos DB and Event Grid are https services, so most
clients need https. Every port answers both http and https. The first start makes a certificate authority in `~/.azemu` (or `--cert-dir`):

- `ca.pem` is what a client trusts. It is also served at `GET /_azemu/ca.pem` on any port.
- It can sign only for `localhost`, anything under `.localhost`, and the loopback addresses. For any other name (a Compose service name, a hosts-file name)
  the emulator refuses the TLS handshake and logs why. `--tls-name <name>` (or `AZEMU_TLS_NAME`, comma separated, repeatable) adds a DNS suffix, an IP
  address or an address range such as `10.0.0.0/24`; it is also served as a host name. A whole top-level domain (`com`, `co.uk`), a wildcard and a range
  wider than a /8 (IPv4) or a /32 (IPv6) are refused at start.
- It lasts 2 years. A new one is made when it is within 30 days of its end, or when the set of `--tls-name` changes; then clients must trust the new
  `ca.pem`. An older azemu left a CA with no limits and a 10-year life: it is replaced at the first start.

Make **one client** trust it. Do not point `SSL_CERT_FILE` or `REQUESTS_CA_BUNDLE` at `ca.pem` for a whole shell: those replace the system's list, and every
other https call from that shell then fails.

| Client | How | Tried |
|---|---|---|
| Python Azure SDKs | `connection_verify="/path/ca.pem"` on the client (Cosmos: see below) | yes (Blob, Key Vault, Service Bus administration) |
| Cosmos DB, Python | the SDK ignores a CA path in `connection_verify`: use `connection_verify=False`, or `REQUESTS_CA_BUNDLE` / `SSL_CERT_FILE` (async) for that process | `connection_verify=False`: yes |
| Node | `NODE_EXTRA_CA_CERTS=/path/ca.pem` (adds to the system list; read when Node starts) | yes |
| curl | `--cacert /path/ca.pem` | yes |
| Java | copy the JDK's `cacerts`, add the CA, run with the copy: `keytool -importcert -noprompt -alias azemu -file ca.pem -keystore my-cacerts -storepass changeit`, then `java -Djavax.net.ssl.trustStore=my-cacerts -Djavax.net.ssl.trustStorePassword=changeit ...` | yes |
| Go, Terraform, Rust, .NET, and tools that take one file (`SSL_CERT_FILE`) | a bundle: the system list plus the emulator's CA (below), given to that one process. Rust: the Azure crates read it; without it a Blob request is retried for over a minute before it reports the certificate error ([`rust.md`](rust.md)) | Go: yes; Rust: yes (Blob, Queue, Key Vault, Cosmos DB, Event Hubs and `reqwest`); Terraform: yes (its test runner does this); .NET: not tried here |
| Everything on the machine | install `ca.pem` as a system CA (Debian and Ubuntu: copy it to `/usr/local/share/ca-certificates/azemu.crt`, run `update-ca-certificates`) | not tried (needs root) |

A bundle for tools that read one file:

```bash
cat /etc/ssl/certs/ca-certificates.crt ~/.azemu/ca.pem > ~/.azemu/bundle.pem     # Linux; on macOS the system bundle is /etc/ssl/cert.pem
SSL_CERT_FILE=~/.azemu/bundle.pem terraform apply
```

azemu does not build the bundle for you: it may run in a container whose system list is not yours.

**Installing the CA for the whole system is fine only for the default names.** The private key (`ca.key`, mode 0600) can sign only for the names the CA is
limited to. A CA made with `--tls-name core.windows.net` can sign for `*.core.windows.net`, so with that CA installed system-wide, anyone who can read
`ca.key` can impersonate real Azure storage to that machine. Do not install such a CA system-wide; trust it per client. See `SECURITY.md`.

## Real Azure host names

To keep a client's endpoints exactly as in production (`myacct.blob.core.windows.net`), map the names to `127.0.0.1` in the hosts file and start the emulator
with `--allow-host core.windows.net --tls-name core.windows.net`. Tried with `curl --resolve` instead of a hosts file: with both options the request reaches
the Blob service over https; with neither, the emulator answers `403 HostNotAllowed` (or refuses TLS). The host check is what stops DNS rebinding (see
`SECURITY.md`), which is why a name outside the allowed ones is refused by default.

## Which Host names are served

A request is served only when its `Host` is an IP address, `localhost` or `*.localhost`, a name with no dot (a Compose service name such as `azemu`),
`host.docker.internal` or `gateway.docker.internal`, or a name under a suffix given with `--tls-name` or `--allow-host`. Any other name gets
`403 HostNotAllowed`, and the answer names the host and the option to use.
