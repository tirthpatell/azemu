# Rust example

A small library that uses Blob, Queue, Key Vault and Cosmos DB through the Azure SDK for Rust, a program that runs it against an emulator you started, and tests that start the
emulator themselves. Read [`docs/rust.md`](../../docs/rust.md) for the reasons behind each setting; this is its worked example.

| File | What it is |
|---|---|
| `src/lib.rs` | the code under test: a Blob, a Queue and a Key Vault round trip, and Cosmos DB (create the database and container, insert, a parameterised query, a cross-partition query, a `412` for a stale ETag). It knows nothing about azemu: it takes endpoints, a credential and `ClientOptions`. |
| `tests/services.rs` | four tests, one account each, run in parallel, each emptied at the end. They use `azemu-testkit`, which starts one emulator for the test binary. |
| `src/bin/hello.rs` | the same code against an emulator you started; its first lines say how. |

## Run the tests

You need `azemu` on `PATH` (or `AZEMU_BIN` pointing at it), Rust, and a C compiler. From a checkout of this repository:

```bash
cargo install --locked --path ../../crates/azemu     # once: azemu in ~/.cargo/bin
cargo test                                           # 4 tests, about 0.1 s after the build
```

## Run the program

```bash
azemu &                                              # prints the ready line; `kill %1` stops it
cat /etc/ssl/certs/ca-certificates.crt ~/.azemu/ca.pem > ~/.azemu/bundle.pem
SSL_CERT_FILE=~/.azemu/bundle.pem AZURE_COSMOS_BINARY_ENCODING_ENABLED=false COSMOS_DISABLE_IMDS=1 \
IDENTITY_ENDPOINT=http://localhost:4577/_azemu/identity/msi/token IDENTITY_HEADER=azemu cargo run --bin hello
```

It prints the blob, the queue message and the secret it wrote, and the result of the Cosmos DB queries. A second run says `409 ContainerAlreadyExists` until you `curl -X POST localhost:4577/_azemu/reset`.

## In your own project

The testkit is a path dependency here so that the example works from a checkout. In your own project it is a git dependency:

```toml
[dev-dependencies]
azemu-testkit = { git = "https://github.com/tirthpatell/azemu", tag = "v0.1.0" }
```

CI for such a project: [`examples/ci/github-actions-rust.yml`](../ci/github-actions-rust.yml).
