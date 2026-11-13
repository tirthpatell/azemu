# Examples

Runnable arrangements for reaching the emulator from somewhere that cannot resolve `*.localhost`: another container, a CI
job (the Compose and CI examples below). Both use the standard ports (`--standard-ports`): one fixed port per service, any host name works. A Rust project is
in `rust/`. The ports are
listed in `Dockerfile.dev` and in [`docs/reaching-the-emulator.md`](../docs/reaching-the-emulator.md).

The Compose and CI examples need an image, which is not published yet: today you build it yourself. The release image is built by the `Dockerfile` at the root (about 3 minutes; it compiles the emulator inside Docker):

```
docker build -t azemu:latest .
```

For quick local iteration, `Dockerfile.dev` puts a binary you built yourself into a small image (`AZEMU_IMAGE=azemu:dev` makes the examples use it):

```
cargo build -p azemu
mkdir -p ~/.cache/azemu-image && cp target/debug/azemu ~/.cache/azemu-image/
docker build -f Dockerfile.dev -t azemu:dev ~/.cache/azemu-image
```

| Directory | Shows |
|---|---|
| `common/` | `check.py`: a client that writes and reads one Blob, one Queue message and one Key Vault secret through the Python SDKs, and fetches the emulator's CA from the emulator. Used by both examples. |
| `compose/` | `compose.yaml`: the emulator and that client in one Compose project, the client reaching the emulator by the service name `azemu`. `cd examples/compose && docker compose up --build --abort-on-container-exit --exit-code-from client`. |
| `ci/` | `gitlab-ci.yml` (the emulator as a `services:` entry with the alias `azemu`) and `github-actions.yml` (a service container with published ports, reached on `localhost`). `check.sh gitlab` and `check.sh github` reproduce both arrangements on one machine with Docker. |
| `rust/` | a small Rust project: Blob, Queue, Key Vault and Cosmos DB through the Azure SDK for Rust, a program, and tests that start the emulator themselves (`cargo test`). Not Docker: it needs the `azemu` binary. [`rust/README.md`](rust/README.md), and [`docs/rust.md`](../docs/rust.md) for the reasons. |
| `ci/github-actions-rust.yml` | GitHub Actions for a Rust project whose tests start azemu: installs it from a tag with `cargo install --git`, caches it, runs `cargo test`; the release archive and the image as commented alternatives for after the first release. |

Expected result: `docker compose up ... --exit-code-from client` ends with exit 0 and three `ok` lines; `examples/ci/check.sh gitlab` and `check.sh github` each print three `ok` lines and
exit 0. The two YAML files for the CI systems are meant for those systems; `check.sh` reproduces the arrangement they describe on one machine with Docker. `tests/container/run.sh` is the bigger test of the same idea (eleven services).

Things the examples rely on, so they are not surprises:

- **https needs a certificate for the name the client uses.** The emulator's CA only signs `localhost`, `*.localhost` and
  loopback addresses, plus what `--tls-name` / `AZEMU_TLS_NAME` adds. In Compose that is the service name: `AZEMU_TLS_NAME=azemu`.
  The client gets the CA from `http://azemu:10000/_azemu/ca.pem` (any port answers `/_azemu/` paths).
- **Key Vault, App Configuration and Event Grid on their ports: the host name is the vault, store or topic.** `https://azemu:10010`
  is the vault `azemu`; a second network alias gives a second vault. With `localhost` or an IP address it is the vault `default`.
- **Service Bus and Event Hubs share port 5672, and 5671 (AMQP over TLS) is open too.** A client that signs in with an identity gives the host name without a port (`azemu`) and the SDK dials 5671; a connection string with `sb://azemu:5672` uses 5672. `sb://azemu:5672` is Service Bus; a host with the label `eventhub` is Event Hubs,
  so the Compose file gives the emulator a second network alias, `eventhub`.
- **The emulator has no sign-in.** Publish a port to the host only as `127.0.0.1:PORT:PORT`.
