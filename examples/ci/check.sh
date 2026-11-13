#!/usr/bin/env bash
# What a CI job does: start the emulator image next to the job, wait, run the job's tests. This reproduces both shapes
# on one machine with Docker, without publishing a port on the host:
#
#   examples/ci/check.sh gitlab   the job reaches the service by its alias, like `services: - name: azemu:latest alias: azemu`
#   examples/ci/check.sh github   the job reaches the service on localhost, like a GitHub Actions service container
#                                  with published ports (the job shares the service's network here)
#
# Needs the image `azemu:latest` (`docker build -t azemu:latest .` at the root; or AZEMU_IMAGE=... for another, for example
# `azemu:dev` from Dockerfile.dev, the quick local path around a binary you built) and Docker. Prints what the job's script prints.
set -eu
mode="${1:-gitlab}"
here="$(cd "$(dirname "$0")" && pwd)"
image="${AZEMU_IMAGE:-azemu:latest}"
tag="$$"
net="azemu-ci-net-$tag"
svc="azemu-ci-svc-$tag"
client="azemu-ci-client-$tag"
cleanup() { docker rm -f "$svc" "$client" >/dev/null 2>&1 || true; docker network rm "$net" >/dev/null 2>&1 || true; docker rmi -f "azemu-ci-check:$tag" >/dev/null 2>&1 || true; }
trap cleanup EXIT

docker build -q -t "azemu-ci-check:$tag" "$here/../common" >/dev/null
docker network create "$net" >/dev/null
case "$mode" in
gitlab)
    docker run -d --name "$svc" --network "$net" --network-alias azemu -e AZEMU_TLS_NAME=azemu "$image" >/dev/null
    host=azemu; net_args="--network $net" ;;
github)
    docker run -d --name "$svc" --network "$net" "$image" >/dev/null
    host=localhost; net_args="--network container:$svc" ;;
*) echo "usage: $0 gitlab|github" >&2; exit 2 ;;
esac
# the job: its script waits for the emulator itself (check.py), like `until curl ...` would
docker run --rm --name "$client" $net_args -e AZEMU_HOST="$host" "azemu-ci-check:$tag"
