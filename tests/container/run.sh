#!/usr/bin/env bash
# The container test: the emulator runs in one container, a Python client runs in a second one on the
# same Docker network, and reaches Blob, Queue, Table, Key Vault, Cosmos, Service Bus and Event Hubs by the name `azemu`
# on the standard ports. No name under .localhost has to resolve and nothing is published to the host.
#
#   tests/container/run.sh [path-to-azemu-binary]        which emulator image is tested, first match:
#     1. AZEMU_IMAGE=azemu:0.1.0 tests/container/run.sh   an image you built from the release Dockerfile (or pulled): it is
#                                                         tested as it is, and left in place afterwards
#     2. a binary given as the argument                   put into a small image with Dockerfile.dev (removed afterwards)
#     3. the image azemu:latest, when it exists           the release image: `docker build -t azemu:latest .`
#     4. target/debug/azemu                               built with `cargo build -p azemu`, put into an image the same way
#
# It needs Docker and the network (the base images `debian:stable-slim` and `python:3.12-slim`, and the SDKs from PyPI).
# It is not part of tests/run_sdk_suites.py for that reason. It prints one line per step, `ok ...` or `FAIL ...`, removes
# the containers, the network and the two images it built, and exits 1 when a step failed.
#
# Setting AZEMU_CONTAINER_KEEP=1 leaves the containers, network and images in place for a look (remove them by hand).
set -u
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
bin="${1:-}"
tag="$$"
net="azemu-ct-net-$tag"
emu="azemu-ct-emu-$tag"
warn="azemu-ct-warn-$tag"
user_ct="azemu-ct-user-$tag"
emu_image="azemu-ct-emulator:$tag"
client_image="azemu-ct-client:$tag"
# not /tmp, which is memory-backed on many systems
ctx="${AZEMU_CONTAINER_SCRATCH:-$HOME/.cache/azemu-container-test}/ctx-$tag"
failed=0
built_emu_image=1      # 1: this script builds the emulator image and removes it; 0: it uses an image that exists

say() { printf '%s\n' "$*"; }
step_ok() { say "ok   $*"; }
step_fail() { say "FAIL $*"; failed=1; }

cleanup() {
    if [ "${AZEMU_CONTAINER_KEEP:-0}" = 1 ]; then
        say "kept: containers $emu, network $net, images $emu_image $client_image"
        return
    fi
    docker rm -f "$emu" "$warn" "$user_ct" >/dev/null 2>&1
    docker network rm "$net" >/dev/null 2>&1
    [ "$built_emu_image" = 1 ] && docker rmi -f "$emu_image" >/dev/null 2>&1
    docker rmi -f "$client_image" >/dev/null 2>&1
    rm -rf "$ctx"
    leftovers=$(docker ps -aq --filter "name=azemu-ct-" | wc -l)
    leftovers=$((leftovers + $(docker network ls -q --filter "name=azemu-ct-" | wc -l)))
    if [ "$leftovers" = 0 ]; then say "ok   cleaned up: no container or network of this test is left"; else say "FAIL cleanup: $leftovers left"; failed=1; fi
}
trap cleanup EXIT

if ! docker info >/dev/null 2>&1; then step_fail "docker: not available"; exit 1; fi
step_ok "docker is available"
if [ -n "${AZEMU_IMAGE:-}" ]; then
    docker image inspect "$AZEMU_IMAGE" >/dev/null 2>&1 || { step_fail "image: AZEMU_IMAGE=$AZEMU_IMAGE does not exist (docker build -t $AZEMU_IMAGE .)"; exit 1; }
    emu_image="$AZEMU_IMAGE"; built_emu_image=0
    step_ok "image: the existing image $emu_image ($(docker image inspect -f '{{.Size}}' "$emu_image" | awk '{printf "%.1f MB", $1/1000000}'))"
else
    if [ -z "$bin" ] && docker image inspect azemu:latest >/dev/null 2>&1; then
        emu_image="azemu:latest"; built_emu_image=0
        step_ok "image: the release image azemu:latest ($(docker image inspect -f '{{.Size}}' "$emu_image" | awk '{printf "%.1f MB", $1/1000000}'))"
    else
        bin="${bin:-$root/target/debug/azemu}"
        [ -x "$bin" ] || { step_fail "binary: $bin is not an executable (build it with: cargo build -p azemu, or build the release image: docker build -t azemu:latest .)"; exit 1; }
        step_ok "binary: $bin"
        # 1. the emulator image: Dockerfile.dev around a copy of the binary
        mkdir -p "$ctx" && cp "$bin" "$ctx/azemu" || { step_fail "image: cannot copy the binary to $ctx"; exit 1; }
        if docker build -q -f "$root/Dockerfile.dev" -t "$emu_image" "$ctx" >/dev/null; then step_ok "image: Dockerfile.dev built $emu_image"; else step_fail "image: docker build of Dockerfile.dev failed"; exit 1; fi
        rm -rf "$ctx"
    fi
fi

# 2. the client image
if docker build -q -f "$here/Dockerfile.client" -t "$client_image" "$here" >/dev/null; then step_ok "image: the Python client built ($client_image)"; else step_fail "image: docker build of the client failed"; exit 1; fi

# 3. the network and the emulator; the names `azemu` and `eventhub` resolve to it, nothing under .localhost does
docker network create "$net" >/dev/null && step_ok "network: $net" || { step_fail "network: cannot create it"; exit 1; }
if docker run -d --name "$emu" --network "$net" --network-alias azemu --network-alias eventhub \
    -e AZEMU_TLS_NAME=azemu "$emu_image" >/dev/null; then step_ok "emulator: started (aliases azemu and eventhub, certificate for azemu)"; else step_fail "emulator: cannot start"; exit 1; fi
ready=""
for _ in $(seq 1 60); do
    ready=$(docker logs "$emu" 2>/dev/null | grep -m1 '^azemu ready' || true)
    [ -n "$ready" ] && break
    [ "$(docker inspect -f '{{.State.Running}}' "$emu")" = true ] || break
    sleep 0.5
done
if [ -n "$ready" ]; then step_ok "emulator: $ready"; else step_fail "emulator: no ready line"; docker logs "$emu" 2>&1 | tail -20; exit 1; fi

# the image's own health check (`azemu __healthcheck`) and the user it runs as
healthy=""
for _ in $(seq 1 40); do
    healthy=$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{end}}' "$emu")
    [ "$healthy" = healthy ] && break
    [ -z "$healthy" ] && break
    sleep 1
done
if [ "$healthy" = healthy ]; then step_ok "health: the image's HEALTHCHECK reports healthy"; elif [ -z "$healthy" ]; then step_fail "health: the image has no HEALTHCHECK"; else step_fail "health: the image's HEALTHCHECK says '$healthy'"; fi
user=$(docker inspect -f '{{.Config.User}}' "$emu_image")
case "$user" in ""|0|root|0:*|root:*) step_fail "user: the image runs as root ('$user')";; *) step_ok "user: the image runs as '$user', not root";; esac

# 4. the client container runs its checks against it
out=$(docker run --rm --network "$net" "$client_image" 2>&1); code=$?
printf '%s\n' "$out" | sed 's/^ok /ok   client: /; s/^FAIL /FAIL client: /'
[ "$code" = 0 ] || failed=1
for service in blob datalake queue table keyvault appconfig cosmos eventgrid servicebus eventhubs arm_and_identity; do
    printf '%s\n' "$out" | grep -q "^ok $service:" || { step_fail "client: no 'ok' line for $service"; }
done

# 4b. port 443: `--standard-ports` also listens on 443, which is where the Azure SDKs go for the management
# API when a client gives no port. The process in the image is not root; Docker lets it bind a port below 1024. ARM answers
# there, by the name `azemu`, over https (the certificate is for azemu; this check does not verify it, only the port).
arm443=$(docker run --rm --network "$net" --entrypoint python "$client_image" -c '
import ssl, urllib.request
c = ssl.create_default_context(); c.check_hostname = False; c.verify_mode = ssl.CERT_NONE
r = urllib.request.Request("https://azemu:443/subscriptions?api-version=2020-01-01", headers={"Authorization": "Bearer x"})
print(urllib.request.urlopen(r, context=c, timeout=10).status)' 2>&1 | tail -1)
if [ "$arm443" = 200 ]; then step_ok "port 443: the management API answers https://azemu:443 (the image's user is not root)"; else step_fail "port 443: expected 200 from https://azemu:443/subscriptions, saw: $arm443"; fi
if docker logs "$emu" 2>&1 | grep -q 'could not open port 443'; then step_fail "port 443: the emulator warned that it could not open 443 in the container"; else step_ok "port 443: no warning about it at start"; fi

# 4c. the image started as an arbitrary user (`docker run --user 12345`), with no --cert-dir: it cannot write the default certificate
# directory, so it must say so and keep the CA in a temporary directory instead of failing with "Permission denied" on ca.key
if docker run -d --name "$user_ct" --network "$net" --user 12345:12345 "$emu_image" >/dev/null; then
    ready=""
    for _ in $(seq 1 40); do
        ready=$(docker logs "$user_ct" 2>/dev/null | grep -m1 '^azemu ready' || true)
        [ -n "$ready" ] && break
        [ "$(docker inspect -f '{{.State.Running}}' "$user_ct")" = true ] || break
        sleep 0.5
    done
    if [ -n "$ready" ] && docker logs "$user_ct" 2>&1 | grep -q 'cannot be written.*temporary directory'; then
        step_ok "user: started as uid 12345 without --cert-dir; it logged that the CA is in a temporary directory"
    else
        step_fail "user: as uid 12345 without --cert-dir the emulator did not start with a temporary CA"
        docker logs "$user_ct" 2>&1 | tail -5
    fi
    docker rm -f "$user_ct" >/dev/null 2>&1
else
    step_fail "user: cannot start the container as uid 12345"
fi

# 5. where names under .localhost do not resolve, the emulator says so once and names the ways out. Docker's own DNS
# on a network you create answers for .localhost (newer versions), so this uses a second emulator on the default bridge,
# where the lookup fails.
if docker run -d --name "$warn" "$emu_image" >/dev/null; then
    n=0
    for _ in $(seq 1 40); do
        n=$(docker logs "$warn" 2>&1 | grep -c 'does not resolve names under .localhost' || true)
        [ "$n" != 0 ] && break
        sleep 0.5
    done
    sleep 1
    n=$(docker logs "$warn" 2>&1 | grep -c 'does not resolve names under .localhost' || true)
    if [ "$n" = 1 ] && docker logs "$warn" 2>&1 | grep 'does not resolve names under .localhost' | grep -q -- '--standard-ports.*hosts-file.*network alias'; then
        step_ok "warning: one line that .localhost names do not resolve in a container, naming the three ways out"
    else
        step_fail "warning: expected one .localhost warning naming the ways out, saw $n"
        docker logs "$warn" 2>&1 | grep -v 'INFO' | head -5
    fi
else
    step_fail "warning: cannot start the second emulator"
fi

# 6. `docker stop` (SIGTERM): the emulator must end well inside Docker's 10 second grace period, and by itself (exit 0), not
# by being killed (137)
t0=$(date +%s.%N)
docker stop -t 10 "$emu" >/dev/null 2>&1
t1=$(date +%s.%N)
code=$(docker inspect -f '{{.State.ExitCode}}' "$emu")
took=$(awk -v a="$t0" -v b="$t1" 'BEGIN {printf "%.1f", b - a}')
if [ "$code" = 0 ] && awk -v t="$took" 'BEGIN {exit !(t < 5)}'; then step_ok "stop: SIGTERM ended the emulator in ${took}s with exit code 0"; else step_fail "stop: took ${took}s, exit code $code (wanted under 5 s and 0)"; fi

exit $failed
