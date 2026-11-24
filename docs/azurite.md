# Moving from Azurite

This page is for a project that tests Blob, Queue or Table storage against Azurite today and wants to use azemu instead (or as well). It says what stays the same, which Azurite option
becomes which azemu option, what is different, and how a Rust project with the current Azure SDK reaches the emulator.

What "tried" means here: azemu was built from this source and Azurite 3.37.0 (the npm package) ran next to it for the comparisons; the clients were the Python SDKs
(`azure-storage-blob` 12.31.1, `azure-storage-queue` 12.18.1, `azure-data-tables` 12.7.0), the Go SDK (`azblob` 1.8.3, `azqueue` 1.0.1) and the Rust crates named below, on Linux. The
JavaScript, Java and .NET SDKs and macOS were not tried for this page. Each claim below says "tried" or says what it comes from.

**In short.** Start azemu with `--standard-ports` (the container image already does). The Azurite addresses, the account `devstoreaccount1` and its key work as they are. Replace the options with the table
below, make the containers and queues with a [seed file](seed.md) instead of an init container, and look at "What differs" before you trust a green test.

## What stays the same

With `--standard-ports` azemu listens on the Azurite ports and takes the account from the first segment of the path, as Azurite does:

| | Address | Account and key |
|---|---|---|
| Blob | `http://127.0.0.1:10000/devstoreaccount1` | the well-known development account `devstoreaccount1` and its well-known key (below) |
| Queue | `http://127.0.0.1:10001/devstoreaccount1` | the same |
| Table | `http://127.0.0.1:10002/devstoreaccount1` | the same |
| Azure Files (Azurite has none) | `http://127.0.0.1:10004/devstoreaccount1` | the same; the REST API only, see [`services/file.md`](services/file.md) |

The key is `Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==`. Both connection strings work:

```
UseDevelopmentStorage=true
```

```
DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;BlobEndpoint=http://127.0.0.1:10000/devstoreaccount1;QueueEndpoint=http://127.0.0.1:10001/devstoreaccount1;TableEndpoint=http://127.0.0.1:10002/devstoreaccount1;
```

Tried:

- **Python:** both strings, with `BlobServiceClient`, `QueueServiceClient` and `TableServiceClient`: a container and a blob, a queue and a message, a table and an entity, all written and read back.
- **Go:** the long form works for Blob and Queue (`azblob.NewClientFromConnectionString`, `azqueue.NewServiceClientFromConnectionString`). **The Go SDK refuses `UseDevelopmentStorage=true` itself**
  (`connection string needs either AccountName or BlobEndpoint`, and the same for the queue): the error comes from the SDK before any request is sent, so use the long form in Go.
- On the gateway port (4577, no `--standard-ports`) the path form does not work: the Azurite address needs the standard ports or `--pin blob=10000` (below). The host form
  (`http://devstoreaccount1.blob.core.localhost:4577`) is the default way in; see [`reaching-the-emulator.md`](reaching-the-emulator.md).

A request to `http://127.0.0.1:10000/` with no account in the path is the account `devstoreaccount1` (tried: 200; Azurite answered 400 `InvalidQueryParameterValue` to the same request).

## Azurite options and what to use here

| Azurite | azemu | Tried |
|---|---|---|
| `--blobHost`, `--queueHost`, `--tableHost` | `--host <IP>`, one option for every service, repeatable. The default is `127.0.0.1` and `::1`. `--host 0.0.0.0` listens on every address: do that only in a container (the image does), because azemu has no sign-in | `--host 127.0.0.2`: it listened there and no longer on `127.0.0.1` |
| `--blobPort`, `--queuePort`, `--tablePort` | `--pin blob=<port>`, `--pin queue=<port>`, `--pin table=<port>` (repeatable; port 0 picks a free one and `GET /_azemu/config` names it), or `--standard-ports` for 10000, 10001, 10002 and the ports of the other services. A pinned service is also on the gateway | `--pin blob=0`, and `--host 0.0.0.0 --pin blob=7777` in the container image with `-p 127.0.0.1:7777:7777` |
| `--location`, `--inMemoryPersistence` | The default is **memory only** (Azurite's default is files in the working directory). `--data-dir <path>` (or `AZEMU_DATA_DIR`) keeps state between runs: [`services/persistence.md`](services/persistence.md). azemu does not read Azurite's data files | `--data-dir`: a container and a blob were still there after a stop and a start |
| `--loose` | Not needed. azemu ignores headers and query parameters it does not know: a request with `x-ms-foo: bar` and one with `?foo=bar` answered `200`, as Azurite's default mode did. An operation azemu does not have answers `501` and names itself (`GET /_azemu/unsupported` lists them) | yes, Blob |
| `--skipApiVersionCheck` | Not needed for a newer SDK. Blob accepts any `x-ms-version` written as a date from 2008 to 2029 and answers with that same version in the `x-ms-version` header; one that is not a date in that form or is outside those years is `400 InvalidHeaderValue`. Azurite 3.37.0 refused `2026-12-06` and later with `400` and named `--skipApiVersionCheck`; azemu answered `200` to `2026-12-06` and `2027-01-01`, and `400` to `2099-01-01`, `2030-01-01`, `2007-01-01` and `abc`. azemu does not change its behaviour with the version | yes, Blob. Queue and Table not tried |
| `--cert`, `--key`, `--pwd`, `--oauth basic` | https is always on, on the same port as http, with a certificate from azemu's own authority (`ca.pem`, also served at `GET /_azemu/ca.pem`). There is no option for your own certificate. A bearer token is accepted whatever it holds: azemu does not decode it, and checks no issuer, audience or expiry. Make the client trust `ca.pem`: [`reaching-the-emulator.md`](reaching-the-emulator.md#https-and-the-emulators-certificate-authority) | `curl --cacert ca.pem -H "Authorization: Bearer anything" https://localhost:10000/devstoreaccount1?comp=list`: 200; with no `Authorization` header: 401 |
| `--disableProductStyleUrl` | Not needed. A pinned or standard port takes the account from the path (`/devstoreaccount1/...`) and also accepts the host form (`devstoreaccount1.blob.core.localhost:10000`) | both forms: 200 |
| `--silent` | `--log-level warn` (or `error`). The default, `info`, logs one line per request | `warn`: nothing but warnings in the log |
| `--debug <file>` | `--log-level debug`, which adds each request's query and headers (credentials replaced by `<redacted>`); the log goes to standard error, so `2> azemu.log` | `debug`: yes |
| `--disableTelemetry` | Nothing to switch off: azemu sends nothing anywhere ([`SECURITY.md`](../SECURITY.md)) | not applicable |
| `AZURITE_ACCOUNTS` (other accounts and keys) | No equivalent: any other account name is a new, empty account and its key is not checked (below) | |
| `--blobKeepAliveTimeout` and the other keep-alive options, `--extentMemoryLimit` | no equivalent | |

Azurite's data folder on disk is not read by azemu. Make what the project needs at start with `--seed` (next section).

### An init container becomes a seed file

A Compose project often has a second container that runs the Azure CLI to create the blob containers and queues once Azurite is up. azemu makes them itself at start, and again after every reset, from a JSON file
(`--seed seed.json`, or `AZEMU_SEED`; the format, with Cosmos databases, secrets and settings too, is in [`seed.md`](seed.md)):

```json
{
  "blob": { "devstoreaccount1": { "containers": ["uploads"] } },
  "queue": { "devstoreaccount1": { "queues": ["jobs"] } }
}
```

Tried (below, in the Compose example): after the start, `list_containers()` gave `uploads` and `list_queues()` gave `jobs`, with no creating call. Something that already exists is left as it is.

## What differs

- **Signatures are checked for `devstoreaccount1`, as in Azurite.** A wrong key or a wrong SAS is `403`. The error code differs: azemu answers `AuthenticationFailed` (the code Azure uses for a signature that does not match), Azurite
  answered `AuthorizationFailure` to the same request. A test that checks the status behaves the same; one that checks the code does not.
- **A made-up signature is refused for every account.** A signature that is not base64 (`sig=bad`) is `400 InvalidAuthenticationInfo` in a Shared Key header and `403 AuthenticationFailed` in a SAS, as Azure answers (tried in November 2026), whatever the account. Azurite
  answered `403` to both. A request signed with a placeholder (recorded traffic) needs `AZEMU_LOOSE_AUTH=1`. Azure also refuses a Shared Key request more than 15 minutes old, an `sv` and an `x-ms-version` it does not know, and a `sip` that is not the caller; so does azemu. Details: [`services/blob.md`](services/blob.md#auth).
- **Other account names are new, empty accounts with no key check.** Azurite knows `devstoreaccount1` and the accounts of `AZURITE_ACCOUNTS`. azemu makes an account when a request names it and checks the signature only for
  `devstoreaccount1`, since no client can know the key of another one. An expired SAS, a missing permission and a wrong protocol are refused for every account.
- **An operation azemu does not have is `501` and names itself**, where Azurite answered `400` with no body for an unknown `comp` (tried with `?comp=nosuch`).
- **State is in memory by default** (above), and nothing is made by being named: a container, a queue or a table exists only after something creates it, as on Azure (seed file or the SDK's create call).
- **It listens on `127.0.0.1` and `::1`.** An address that is not a loopback address needs `--host`.

What azemu has that Azurite does not, in one list:

- the other services: [Key Vault](services/keyvault.md), [App Configuration](services/appconfig.md), [Cosmos DB](services/cosmos.md) (NoSQL API, port 8081),
  [Service Bus](services/servicebus.md), [Event Hubs](services/eventhubs.md) (and a [Kafka endpoint](services/kafka.md)), [Event Grid](services/eventgrid.md), [Resource Manager](services/arm.md) and the
  [token endpoints](services/identity.md) of Microsoft Entra, so `DefaultAzureCredential`-style code and Terraform have something to talk to;
- [Data Lake Gen2](services/datalake.md) and [quick query](services/blob-query.md) on the same blob data (Azurite's README lists neither);
- a [seed file](seed.md), a reset per service or per account (`POST /_azemu/reset?account=...`), and an admin address that lists its calls (`GET /_azemu/`);
- a [test kit for Rust](rust.md#8-tests-with-cargo-test) that starts an emulator for a test program on a free port.

What Azurite has that azemu does not: the options in the table above that have no equivalent (your own certificate, `AZURITE_ACCOUNTS`, an SQL metadata database), and Azurite's behaviour of keeping data in files by default.

## The Rust SDK

`azure_storage_blob` 1.x has token credentials only: there is no Shared Key, no connection string and no SAS generation, and it **refuses to send a token over `http://`** (`"http://.../ must use https"`, tried). So there is no
connection string to carry over, and two other ways work against the emulator. Both were tried with `azure_storage_blob` 1.1.0 (against the standard-port address) and with 1.0.0 (against the host form), see
[`rust.md`](rust.md#2-pick-the-crates), "Older versions".

**(a) https, a credential, and the certificate authority trusted.** Use `ManagedIdentityCredential`, which asks azemu for a token when two variables are set, and trust `ca.pem`:

```bash
export IDENTITY_ENDPOINT=http://localhost:4577/_azemu/identity/msi/token IDENTITY_HEADER=azemu
cat /etc/ssl/certs/ca-certificates.crt ~/.azemu/ca.pem > ~/.azemu/bundle.pem && export SSL_CERT_FILE=~/.azemu/bundle.pem     # Linux
```

```rust
let url = Url::parse("https://localhost:10000/devstoreaccount1")?;      // no trailing slash
let blobs = BlobServiceClient::new(url, Some(ManagedIdentityCredential::new(None)?), Some(options))?;
```

Tried: a container, an upload and a download against the standard-port address, with the bundle. Without the bundle the request failed with a certificate error (and, with the default retries, only after over a minute: use
`RetryOptions::none()` in tests). **On macOS `SSL_CERT_FILE` is not read** and the authority has to be trusted in the Keychain, or given to the client in code: [`rust.md`](rust.md#macos) (the System keychain way was run on a macOS 26 runner).

**(b) No credential, and an account SAS in the URL over plain `http://`.** A client built with no credential sends no token, so the SDK allows `http://`, and nothing has to be trusted. The emulator checks a SAS for the development account against
its well-known key, so sign one with that key (`hmac` 0.12, `sha2` 0.10, `base64` 0.22):

```rust
use base64::{engine::general_purpose::STANDARD, Engine};
use hmac::{Hmac, Mac};
use sha2::Sha256;

const KEY: &str = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

/// An account SAS for Blob (read, write, delete, list, add, create; the service, containers and blobs). The key is public, so a far expiry does no harm.
fn dev_sas() -> String {
    let (sp, ss, srt, se, spr, sv) = ("rwdlac", "b", "sco", "2099-12-31T00:00:00Z", "https,http", "2020-12-06");
    // service version 2020-12-06: account, permissions, services, resource types, start (empty), expiry, IP (empty), protocols, version, encryption scope (empty), each ending in a newline
    let to_sign = format!("devstoreaccount1\n{sp}\n{ss}\n{srt}\n\n{se}\n\n{spr}\n{sv}\n\n");
    let mut mac = Hmac::<Sha256>::new_from_slice(&STANDARD.decode(KEY).unwrap()).unwrap();
    mac.update(to_sign.as_bytes());
    let sig = STANDARD.encode(mac.finalize().into_bytes());
    let enc = |s: &str| s.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D").replace(':', "%3A").replace(',', "%2C");
    format!("sv={sv}&ss={ss}&srt={srt}&sp={sp}&se={}&spr={}&sig={}", enc(se), enc(spr), enc(&sig))
}

let url = Url::parse(&format!("http://127.0.0.1:10000/devstoreaccount1?{}", dev_sas()))?;
let blobs = BlobServiceClient::new(url, None, Some(options))?;           // no credential: plain http is allowed
```

Tried: create, upload and download through that client with 1.1.0 against the standard-port address; create, upload, download, list and delete with 1.0.0 through the host form (`http://devstoreaccount1.blob.core.localhost:<port>/?<sas>`);
with 1.0.0 a SAS with a wrong signature of the right shape was `403` and a client with neither credential nor SAS was `401`. This route needs no certificate at all, so it is also the simplest on macOS (plain http involves no TLS; not run there). Another way to get a SAS is the Python SDK's `generate_account_sas`, which the Python tests of this repository use; `az storage account generate-sas` was not tried.

## Compose and containers

**Compose.** The service that was Azurite becomes this (the image has a health check, so `depends_on` with `condition: service_healthy` waits for it):

```yaml
services:
  azemu:
    image: ghcr.io/tirthpatell/azemu:latest     # was: mcr.microsoft.com/azure-storage/azurite
    ports:
      - "127.0.0.1:10000:10000"   # Blob, the Azurite port
      - "127.0.0.1:10001:10001"   # Queue
      - "127.0.0.1:10002:10002"   # Table
      - "127.0.0.1:4577:4577"     # health, reset and the other admin calls
    environment:
      AZEMU_SEED: /seed.json      # replaces an init container that makes containers and queues
    volumes:
      - ./seed.json:/seed.json:ro
```

Tried: `docker compose up -d`, wait for `healthy`, then from the host `UseDevelopmentStorage=true` with the Python SDK listed `uploads` and `jobs` (the seed above) and wrote a blob. It was run with an image built from this source
(`Dockerfile.dev`), not with the published image. Publish ports to `127.0.0.1` only: azemu has no sign-in. Another container in the project reaches it as `http://azemu:10000/devstoreaccount1` (the Compose service name is the host name);
[`examples/compose`](../examples/compose/compose.yaml) and [`examples/seed`](../examples/seed/compose.yaml) are tested examples of that, with https for the services that need it. To keep data between runs add
`-v azemu-data:/data` and `AZEMU_DATA_DIR: /data` (Azurite's `-v ...:/data` is the same idea).

**Other ports.** The image starts with `--host 0.0.0.0 --standard-ports`. Giving a command replaces that, so ask for the address as well: `--host 0.0.0.0 --pin blob=7777` and `-p 127.0.0.1:7777:7777`. Tried: Blob on 7777 with the Python SDK
(`BlobEndpoint=http://127.0.0.1:7777/devstoreaccount1`). `--standard-ports --pin blob=7777` keeps the other standard ports and moves Blob (not tried).

**Testcontainers.** The emulator needs the generic container of the library, the image, the ports and a wait on `GET /_azemu/health`; [`testcontainers.md`](testcontainers.md) has it for Rust, Go and Python, each from a project that was run against the published image.
What differs from an Azurite test:

- The connection string is Azurite's, with the published port in `BlobEndpoint`, `QueueEndpoint` and `TableEndpoint` (`http://<host>:<port>/devstoreaccount1`; Go and Python, tried for Blob, and for Queue and Table in Python). The account and its key are the same.
- Testcontainers publishes the container's port 10000 on a free port of the host, so `UseDevelopmentStorage=true`, which means `127.0.0.1:10000`, does not reach it. Use the full connection string. (Publishing 10000 on 10000 would, and allows one container at a time; not tried.)
- The Rust crates have no connection string: the page shows the account SAS that takes its place.
- The image's own health check works as the wait in all three libraries, but with the 0.2.0 image it took about 3 seconds (from 0.3.0 the image checks every 250 ms while it starts and was healthy in under a second, see [`testcontainers.md`](testcontainers.md)); the wait on `/_azemu/health` takes a third of a second.

## What was not run

- JavaScript, Java and .NET clients with the Azurite addresses.
- `x-ms-version` checks for Queue and Table; Azurite's `--loose` and `--skipApiVersionCheck` themselves (Azurite 3.37.0 was run in its default mode, and its own README and error message name what the two options do).
- `--standard-ports --pin blob=<port>`, `az storage account generate-sas`, and the Compose file with the published image instead of one built from this source.
- Anything on macOS or Windows.
