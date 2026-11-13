# azemu-testkit

Start the azemu emulator from `cargo test`, or find one that is already running, and reach it: the address of each service,
the certificate authority a client has to trust, a name no other test uses, reset of one account, and a clean stop.

The crate has no dependencies (the standard library is all it uses), so it does not pull in an HTTP stack, a particular Azure
SDK or any of the emulator's own crates, and a project that takes it compiles only this crate.

Use it as a git dependency, for tests only:

```toml
[dev-dependencies]
azemu-testkit = { git = "https://github.com/tirthpatell/azemu" }
```

The emulator is a program of its own. Install the one that matches the revision you use (or build it from a checkout) and
put it on the `PATH`, or name it with `AZEMU_BIN`:

```text
cargo install --locked --git https://github.com/tirthpatell/azemu azemu
```

## A test

```rust,no_run
use azemu_testkit::{shared, unique_name, Service};

#[test]
fn my_blob_code() {
    let emu = shared(); // starts azemu once for this test program (or uses AZEMU_GATEWAY)
    let account = unique_name("blobs"); // a name of this test's own: tests run side by side
    let endpoint = emu.endpoint(Service::Blob, &account); // https://blobs1a2b3c....blob.core.localhost:<port>/
    // ... build the Azure client with `endpoint`, run the code under test ...
    # let _ = endpoint;
    emu.reset_account(&account); // empty the account when the test is done
}
```

`shared()` does four things, once per test program:

1. Finds the emulator. If `AZEMU_GATEWAY` is set (for example `http://127.0.0.1:4577`, or the address of the container
   image), it uses that emulator and starts nothing. Otherwise it starts `azemu` (`AZEMU_BIN`, else the one on the `PATH`;
   the error says which) on a free port with `--port 0`, in a directory of its own, and waits for the ready line.
2. Makes sure the emulator stops. The program is started with `--exit-on-stdin-close` and a pipe on its standard input, and the
   system closes the pipe when the test program ends, whether the tests passed, one of them panicked or the program was
   killed. (`Emulator::start()` gives you one that stops when the value is dropped.)
3. Writes `bundle.pem`, the system's certificate list plus the emulator's authority.
4. Sets the environment the Azure SDKs for Rust need (`Emulator::apply_environment`): `SSL_CERT_FILE` (the bundle),
   `IDENTITY_ENDPOINT` and `IDENTITY_HEADER`, `COSMOS_DISABLE_IMDS`, `AZURE_COSMOS_BINARY_ENCODING_ENABLED`.

## Addresses

| Call | Gives |
|---|---|
| `emu.endpoint(Service::Blob, "acct")` | `https://acct.blob.core.localhost:<port>/`, the host-name form (names under `.localhost` resolve to the local machine on most systems) |
| `emu.http_endpoint(Service::Table, "acct")` | the same with `http` |
| `emu.host_name(Service::EventHubs, "ns")` | `ns.eventhub.localhost:<port>`, the "fully qualified namespace" the Event Hubs crate takes |
| `emu.standard_endpoint(Service::Blob, "acct")` | `http://localhost:10000/acct`, the standard-port form, for an emulator started with `--standard-ports` (the container image) where `.localhost` names do not resolve |
| `emu.gateway()` | `http://127.0.0.1:<port>`, where `/_azemu/...` (health, reset, the CA) is served |
| `emu.identity_endpoint()` | the token endpoint for `ManagedIdentityCredential` |

## Trusting the emulator

Every port answers https with the certificate authority azemu made. A client has to trust it:

| Call | For |
|---|---|
| `emu.ca_file()` | the path of `ca.pem` |
| `emu.ca_pem()` | its text, for a client that takes the certificate as bytes (`reqwest::Certificate::from_pem(emu.ca_pem().as_bytes())`) |
| `emu.ca_bundle()` | a PEM file with the system's list and the authority, for the programs that read one list and no more (`SSL_CERT_FILE`, what `shared()` sets) |

For the Azure SDK for Rust, the environment variable is enough for every service, including Cosmos DB and Event Hubs. A
client that takes a certificate in code can use the PEM text instead; no particular HTTP library is needed by this crate.

## An emulator that is already running

`AZEMU_GATEWAY=http://127.0.0.1:4577 cargo test` (or the address of a container) uses that emulator. Its certificate
authority is the file `AZEMU_CA_FILE` names; if you give none, the crate fetches it from the emulator
(`GET /_azemu/ca.pem`). With the container image:

```text
docker run -d --name azemu -p 127.0.0.1:4577:4577 -p 127.0.0.1:10000:10000 azemu:latest
AZEMU_GATEWAY=http://127.0.0.1:4577 cargo test
```

(Use `emu.standard_endpoint(...)` when the `.localhost` names do not reach it.)

## What else it does

* `emu.reset_account("acct")` empties one account in every service; `emu.reset_all()` empties everything.
* `emu.create_event_hub("ns", "hub", 2)` and `emu.create_service_bus_queue("ns", "queue")` make what Azure's management API
  makes (the Rust SDK has no client for it).
* `emu.request("GET", "/_azemu/config", "")` calls the emulator's own API, and `emu.call(Service::Blob, "acct", "GET", "/", &[], "")`
  a service, with no SDK (no credential is added).
* `unique_name("prefix")` makes a lowercase name of at most 24 characters that is valid for a storage account, a vault, a
  Cosmos account and a namespace, and is different in every test and every test program.
* `COSMOS_EMULATOR_KEY` is the well-known Cosmos DB emulator key (azemu takes any key, but the Rust SDK wants base64).

## Environment variables

| Variable | Meaning |
|---|---|
| `AZEMU_GATEWAY` | use the emulator at this address; start none |
| `AZEMU_CA_FILE` | the `ca.pem` of that emulator (fetched when not given) |
| `AZEMU_BIN` | the `azemu` program to start (default: `azemu` on the `PATH`) |
| `AZEMU_TESTKIT_ARGS` | more arguments for the program, split at spaces (`--lenient`) |
| `AZEMU_TESTKIT_DIR` | where the files go (default: `target/tmp` of the project, else the temporary directory) |

The files are `azemu.log` (the emulator's log, shown when the start fails), the certificate authority, and `bundle-<port>.pem`,
in `azemu-testkit-<test program>` under that directory. The same test program uses the same directory every run.

## Calling the Azure SDK for Rust

The SDK has no credential that works without Azure. The emulator accepts any bearer token, so a fixed one is enough:

```rust,ignore
#[derive(Debug)]
struct AnyToken;

#[async_trait::async_trait]
impl azure_core::credentials::TokenCredential for AnyToken {
    async fn get_token(&self, _scopes: &[&str], _options: Option<azure_core::credentials::TokenRequestOptions<'_>>)
        -> azure_core::Result<azure_core::credentials::AccessToken>
    {
        Ok(azure_core::credentials::AccessToken::new(
            azure_core::credentials::Secret::new("azemu"),
            azure_core::time::OffsetDateTime::now_utc() + azure_core::time::Duration::hours(1),
        ))
    }
}
```

and `RetryOptions::none()` in the client options of a test makes a broken set-up fail at once instead of after the SDK's
default retries.

## Without the emulator

The pieces that do not need one work as they are:

```rust
use azemu_testkit::{Emulator, Service};

let emu = Emulator::connect("http://127.0.0.1:1");
assert!(emu.is_err(), "nothing listens there");
assert!(azemu_testkit::unique_name("vault").starts_with("vault"));
assert_eq!(Service::Blob.standard_port(), 10000);
```
