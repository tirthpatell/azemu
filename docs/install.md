# Installing azemu

Three ways to get it, and the platforms a release provides.

## The container image

Each release is on the GitHub Container Registry as `ghcr.io/tirthpatell/azemu`, with the version as the tag, and `latest`, for `linux/amd64` and `linux/arm64`:

```bash
docker run --rm -p 127.0.0.1:4577:4577 -p 127.0.0.1:10000:10000 ghcr.io/tirthpatell/azemu:latest
```

It is built from the [`Dockerfile`](../Dockerfile) at the root of the repository. To build it yourself (about 3 minutes, and the compile happens inside the Docker daemon):

```bash
docker build -t azemu:latest .
docker run --rm -p 127.0.0.1:4577:4577 -p 127.0.0.1:10000:10000 azemu:latest
```

The container listens on every address *inside* the container and opens one port per service (`--standard-ports`: 10000 Blob, 10001 Queue, 10002 Table, 10003 Data
Lake, 10010 Key Vault, 10011 App Configuration, 10012 Event Grid, 10013 Resource Manager, 10014 Identity, 8081 Cosmos DB, 5672 and 5671 AMQP, 9093 Kafka; the gateway is
4577). **The emulator has no sign-in**: publish ports to `127.0.0.1` only (`-p 127.0.0.1:...`), never `-p 4577:4577`, or to no host port at all.

Other containers reach it by its container or Compose service name ([`examples/compose/compose.yaml`](../examples/compose/compose.yaml)). For https add that name to the
certificate (`-e AZEMU_TLS_NAME=azemu`) and fetch the authority from `http://azemu:4577/_azemu/ca.pem`. Keep state between runs with a volume:
`-v azemu-data:/data -e AZEMU_DATA_DIR=/data`; keep the certificate authority with a second volume on `/home/azemu/.azemu`. The image runs as user 10001 and has no shell;
`docker stop` ends it at once. If you start it with another `--port`, tell the health check: `--health-cmd "/usr/local/bin/azemu __healthcheck 5000"`.

The image is two stages: a build stage with the Rust toolchain, and a runtime stage on a small base (no shell, no package manager) with the binary, the licence files in
`/licenses`, an empty `/data` and the user's certificate directory. It is about 60 MB. A Docker build uses memory outside any limit set on your shell: it compiles with two
parallel jobs by default (`--build-arg CARGO_BUILD_JOBS=2`); give it more if you have the memory.

## The binary

Binaries are attached to each release on the [GitHub release page](https://github.com/tirthpatell/azemu/releases), with a checksum file (`sha256sum -c SHA256SUMS`): Linux x86-64 and arm64 (glibc), Linux x86-64 static (musl), and macOS arm64 (see "Supported platforms"). It is one file, `azemu`; copy it anywhere and run it: `azemu`
(gateway on `127.0.0.1:4577`) or `azemu --standard-ports`. `azemu --version` prints the version and the commit it was built from. No other file is needed.

A binary taken out of the image is built on Debian 12 and needs glibc 2.35 or newer (Debian 12, Ubuntu 22.04 and newer). One built with `cargo build --release` links to the
glibc of the machine that builds it. A static (musl) build is described in [`docs/releasing.md`](releasing.md).

## From source

You need Rust 1.91 or newer (the JavaScript engine used for Cosmos DB scripts requires it; the `Dockerfile` pins the toolchain it was tested with) and a C compiler (`ring`,
the TLS crypto, has C and assembly parts). No `cmake`, no OpenSSL, and no network once the crates are fetched.

```bash
git clone <this repository> && cd azemu
cargo build --release -p azemu          # target/release/azemu, about 3 minutes
cargo install --path crates/azemu       # or: install it into ~/.cargo/bin
```

azemu is not published to crates.io; install it from a checkout. In CI you can install a pinned version without a clone: `cargo install --locked --git https://github.com/tirthpatell/azemu --tag vX.Y.Z azemu --root <dir>` (cache `<dir>`; tried with a local copy of the repository, as `--git` with a path). A test program that starts azemu itself should use `--port 0`, which picks a free port (the same one on `127.0.0.1` and `::1`) and prints it as `gateway=` in the ready line; `--exit-on-stdin-close` makes azemu stop when the test program, which holds the other end of a pipe on its standard input, ends.

A smaller binary: `cargo build --release -p azemu --no-default-features` leaves out the Cosmos script engine (stored procedures, triggers and user-defined functions then answer 501) and
the Kafka endpoint (about 28% smaller). The two features, `cosmos-scripts` and `kafka`, are both on by default.

## Supported platforms

| Platform | What a release provides |
|---|---|
| Linux x86-64, glibc 2.35 or newer | the release binary and the container image. The SDK test suites, the container test and the replay run here |
| Linux x86-64, static (musl) | a release binary that runs on any x86-64 Linux |
| Linux arm64, glibc 2.35 or newer | the release binary and the container image |
| macOS on Apple silicon | the release binary |
| Windows | no native build: use the container image (Docker Desktop) or WSL |

Every release binary is started and smoke-tested on its own platform by the release workflow before it is published.

What differs off Linux:

- File modes (0700 for the data directory, 0600 for state files, the CA key and the identity key) are set only on Unix. On Windows the files get the default permissions of the
  folder, so the statement that the data directory is private to your user would not hold.
- Only Ctrl-C stops the emulator and saves where there is no SIGTERM (Windows).
- Finding out whether a process id exists reads `/proc`; elsewhere only this process is known to run, which affects only the clean-up of leftover temporary files in a data directory.
- Cosmos DB script worker: the processor-time limit is set on Unix; the memory limit is Linux only, so on **macOS and Windows a runaway script is not limited in memory** (it is still
  ended at the time limit).
- `*.localhost` names do not resolve on Windows and on macOS before 26: use `--standard-ports` there ([`reaching-the-emulator.md`](reaching-the-emulator.md)).
- `ring` needs a C compiler on every platform (`cc` or clang on macOS, MSVC on Windows).
- The test suites, `tests/run_all.sh`, the CI and the container test assume Linux and bash.

On an arm64 kernel with 16 KiB or 64 KiB pages the Cosmos script worker reads its memory use from `/proc/self/status`, which needs no page size.

## What it needs at run time

Nothing beyond the binary. It makes a certificate authority on first start in `~/.azemu` (or `--cert-dir`), listens on loopback, and keeps state in memory unless you give it a data
directory (`--data-dir`). It makes no outbound calls except Event Grid webhooks to the addresses you allow ([`SECURITY.md`](../SECURITY.md)).
