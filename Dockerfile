# syntax=docker/dockerfile:1
#
# The release image of azemu: the one `azemu` binary (all services, Linux x86-64 or arm64 glibc, whichever the Docker
# host is) in a small image with no shell and no package manager, running as a user that is not root.
#
#   docker build -t azemu:0.1.0 -t azemu:latest .
#   docker run --rm -p 127.0.0.1:4577:4577 azemu:latest
#
# Build memory: a release build of the whole emulator is the heaviest thing this project does, and it runs inside the
# Docker daemon. `CARGO_BUILD_JOBS` (default 2 here) bounds the compilers that run at once: raise it on a machine with
# plenty of memory (`--build-arg CARGO_BUILD_JOBS=8`), keep it low on a small one.
#   docker build --build-arg CARGO_BUILD_JOBS=2 -t azemu:latest .
# Smaller builds: `--build-arg CARGO_FEATURES=--no-default-features` leaves out the Cosmos script engine and the Kafka
# endpoint (`--no-default-features --features kafka` leaves out only the script engine).
# For quick local iteration use Dockerfile.dev instead: it copies a binary you built on the host.
#
# What the container does by default (the CMD below; replace it to change it):
#   --host 0.0.0.0      listen on every address of the container, so other containers and published ports reach it
#   --standard-ports    one fixed port per service next to the gateway 4577 (10000 Blob, 10001 Queue, 10002 Table,
#                       10003 Data Lake, 10010 Key Vault, 10011 App Configuration, 10012 Event Grid, 10013 ARM,
#                       10014 Identity, 8081 Cosmos, 5672 AMQP, 5671 AMQP over TLS, 9093 Kafka, and 443 for ARM and
#                       AMQP over WebSocket), so any host name works and nothing under .localhost has to resolve
# THE EMULATOR HAS NO SIGN-IN. Anyone who can reach a port can read, change and wipe everything in it. Publish ports to
# your own machine only (`-p 127.0.0.1:4577:4577`, never `-p 4577:4577`), or to no host port at all and let other
# containers on the same Docker network reach it by its name.
#
# Settings you will want (all are in docs/SETTINGS.md):
#   AZEMU_TLS_NAME=azemu   the name other containers use for this one (the Compose service name). https needs a certificate
#                          for it; the emulator's certificate authority only signs localhost, *.localhost, loopback
#                          addresses and what this adds. Fetch the authority from http://azemu:4577/_azemu/ca.pem.
#   AZEMU_DATA_DIR=/data   keep state between runs. Mount a volume there: `-v azemu-data:/data -e AZEMU_DATA_DIR=/data`.
#                          Without it state lives in memory and ends with the container.
#   a volume on /home/azemu/.azemu keeps the certificate authority too, so clients trust it once, not at every start.
# The image runs as user 10001. A named Docker volume takes that user's ownership from the image, so it just works. A bind
# mount (-v ./dir:/data) must be writable by uid 10001. `--user $(id -u)` works too: that user cannot write
# /home/azemu/.azemu, so azemu says so in its log and keeps the certificate authority in a private temporary directory for
# that run (a new authority at every start; clients fetch it from /_azemu/ca.pem). To keep one authority between runs, give
# a directory that user can write: `--cert-dir /data/ca` after the image name.
#
# Health: `azemu __healthcheck [port]` (what HEALTHCHECK runs) asks the gateway for /_azemu/health and exits 0 or 1; the
# image has no curl. If you start with another `--port`, give the check the same: `--health-cmd "/usr/local/bin/azemu __healthcheck 5000"`.
# Stop: SIGTERM saves state (when there is a data directory) and exits within a second or two.

ARG RUST_IMAGE=rust:1.99.1-bookworm
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:latest

FROM ${RUST_IMAGE} AS build
WORKDIR /src
ARG CARGO_BUILD_JOBS=2
ARG CARGO_FEATURES=""
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS} CARGO_TERM_COLOR=never
# Only what the build needs comes into the context (see .dockerignore: it names these and nothing else).
COPY Cargo.toml Cargo.lock ./
COPY crates crates
# `.git`, when the context has it, lets the build record which commit it was made from (`azemu --version`); without it
# the version says "unknown".
COPY .git* ./.git/
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo build --release --locked -p azemu ${CARGO_FEATURES} \
 && install -D target/release/azemu /out/usr/local/bin/azemu \
 && mkdir -p /out/data /out/home/azemu/.azemu /out/licenses \
 && chown -R 10001:10001 /out/data /out/home/azemu
COPY LICENSE-MIT LICENSE-APACHE NOTICE THIRD-PARTY-LICENSES.md /out/licenses/
COPY LICENSES /out/licenses/LICENSES

FROM ${RUNTIME_IMAGE}
ARG VERSION=0.1.0
ARG GIT_COMMIT=unknown
ARG BUILD_DATE=unknown
LABEL org.opencontainers.image.title="azemu" \
      org.opencontainers.image.description="A local emulator of Azure services (Blob, Queue, Table, Data Lake, Key Vault, App Configuration, Cosmos DB, Service Bus, Event Hubs and Kafka, Event Grid, Identity, Resource Manager) in one binary. For development and tests; it has no sign-in. Not affiliated with or endorsed by Microsoft." \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${GIT_COMMIT}" \
      org.opencontainers.image.created="${BUILD_DATE}" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
# `org.opencontainers.image.source` (the repository the image was built from) and `.url` are set by the release workflow
# (docker/metadata-action) from the GitHub repository; a local build has no source URL to name. This image is not made by or
# endorsed by Microsoft; see NOTICE in /licenses.
COPY --from=build /out/ /
USER 10001:10001
ENV HOME=/home/azemu
WORKDIR /home/azemu
EXPOSE 4577 10000 10001 10002 10003 10010 10011 10012 10013 10014 8081 5671 5672 9093 443
HEALTHCHECK --interval=5s --timeout=3s --start-period=3s --retries=3 CMD ["/usr/local/bin/azemu", "__healthcheck"]
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/azemu"]
CMD ["--host", "0.0.0.0", "--standard-ports"]
