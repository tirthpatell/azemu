# Changelog

All notable changes to azemu are written here, newest first. The version is the one in the root `Cargo.toml`;
`azemu --version` prints it with the commit the binary was built from. The project follows [Semantic Versioning](https://semver.org/),
with the usual allowance for versions below 1.0: a minor version may change a flag, a setting or the answer of an endpoint.

## 0.1.0

The first version. It is an emulator of Azure services in one binary, made for developing and testing against: it is not for production, and it has no sign-in. What it
contains, what it does not, and how well it matches Azure are in [`README.md`](README.md) (per service: [`docs/services/`](docs/services/)); this entry does not repeat them.

- The services: Blob (with Data Lake Gen2 and quick query), Queue, Table, Key Vault, App Configuration, Cosmos DB (NoSQL API, gateway mode), Service Bus, Event Hubs (with a Kafka
  endpoint), Event Grid, Identity (token endpoints) and Resource Manager, behind one gateway port or one fixed port per service (`--standard-ports`).
- A container image (`Dockerfile`): the one binary on a small base, running as a user that is not root, with a health check (`azemu __healthcheck`), and stopping at once on `docker stop`.
- `azemu` builds with two Cargo features that are on by default: `cosmos-scripts` (the JavaScript engine for Cosmos stored procedures, triggers and user-defined functions) and
  `kafka` (the Kafka endpoint of Event Hubs). `--no-default-features` makes a smaller binary; the operations of a missing feature answer an error that says the build does not include them.
- Nothing comes into being by being named. A queue, topic, subscription, event hub, consumer group or Event Grid topic must be created first, as on Azure (an administration client,
  the Resource Manager endpoint, a config file or the admin API). The other behaviour is `--lenient` / `AZEMU_LENIENT=1`, or one service's own `..._AUTOCREATE` variable.
- Blob versioning is off for a storage account until it is turned on (through Resource Manager, or `AZEMU_BLOB_VERSIONING=1`).
- A request is served only for an expected host name, and a web page in a browser cannot call the emulator cross-site (`--allow-host`, `--allow-origin`). Event Grid webhooks
  go to local targets only (`--allow-webhook-host`).
- The admin API: `POST /_azemu/reset` (with `?service=` and `?account=`), `GET /_azemu/health`, `GET /_azemu/config`, `GET /_azemu/unsupported`.
- For Rust: [`docs/rust.md`](docs/rust.md) (crates and versions per service, a client for each, services with no official crate, tests, CI), a small project with tests
  ([`examples/rust`](examples/rust/README.md)), a GitHub Actions workflow ([`examples/ci/github-actions-rust.yml`](examples/ci/github-actions-rust.yml)), and `azemu-testkit`
  (`crates/testkit`), a crate with no dependencies that starts the emulator from `cargo test` and reaches it. `--port 0` picks a free port, the same on every address, and
  `--exit-on-stdin-close` stops the emulator when a test program that holds a pipe on its input ends.
- The certificate directory is created with mode 0700, and the emulator warns when an existing one is open to other users. A request that carries no credentials is refused with
  a log line that says why.
- Dual licensed under Apache 2.0 or MIT (`LICENSE-APACHE`, `LICENSE-MIT`), with `NOTICE`, `THIRD-PARTY-LICENSES.md` and `deny.toml`.

Linux x86-64 is the tested platform; other platforms are untested or unsupported ([`docs/install.md`](docs/install.md)).
