# Container test for azemu

Starts the emulator in one container and a Python client in a second container on the same Docker network, and reaches Blob, Queue, Table, Key Vault, Cosmos DB, Service
Bus and Event Hubs by the name `azemu` on the standard ports. No name under `.localhost` has to resolve and nothing is published to the host. It also checks the image
(the health check, the user it runs as, the warning about `.localhost` names, `docker stop` ending the emulator at once) and that nothing is left behind.

## What you need

Docker, and network access for the base images (`debian:stable-slim`, `python:3.12-slim`) and the SDKs from PyPI.

## Run

```bash
docker build -t azemu:latest .                         # the release image, from the repository root
AZEMU_IMAGE=azemu:latest tests/container/run.sh        # test that image as it is (it stays in place)
tests/container/run.sh target/release/azemu            # a binary, put into a small image with Dockerfile.dev
tests/container/run.sh                                 # the image azemu:latest if it exists, else target/debug/azemu
```

It prints one line per step, `ok ...` or `FAIL ...`, removes the containers, the network and the images it built, and exits 1 when a step failed. `AZEMU_CONTAINER_KEEP=1`
leaves them in place for a look (remove them by hand). A step is skipped only when the environment cannot do it (for example, no Docker).
