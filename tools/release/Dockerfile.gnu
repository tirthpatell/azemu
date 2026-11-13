# syntax=docker/dockerfile:1
# The glibc release binary of the Docker host's own architecture (x86-64 or arm64, whichever builds it), made on Debian 12
# (bookworm) so that it runs on Debian 12 and newer, Ubuntu 22.04 and newer, and anything else with glibc 2.35 or newer. Built
# like the root Dockerfile builds the image's binary; this one only leaves the file in a directory instead of an image.
# The release workflow (.github/workflows/release.yml) runs it on GitHub's x86-64 and arm64 runners. From the repository root:
#
#   docker build -f tools/release/Dockerfile.gnu --target export --output type=local,dest=$HOME/.cache/azemu-gnu .
#   $HOME/.cache/azemu-gnu/azemu --version
#
# `.git`, when the context has it, lets the build record the commit (`azemu --version`). Same memory warning as the root
# Dockerfile: this compiles inside the Docker daemon; CARGO_BUILD_JOBS limits it (the default here is for a small machine).
ARG RUST_IMAGE=rust:1.99.1-bookworm
FROM ${RUST_IMAGE} AS build
WORKDIR /src
ARG CARGO_BUILD_JOBS=2
ARG CARGO_FEATURES=""
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS} CARGO_TERM_COLOR=never
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY crates crates
COPY .git* ./.git/
RUN cargo build --release --locked -p azemu ${CARGO_FEATURES} && install -D target/release/azemu /out/azemu

FROM scratch AS export
COPY --from=build /out/azemu /azemu
