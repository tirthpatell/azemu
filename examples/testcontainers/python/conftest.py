"""Starts azemu in a container with Testcontainers: one container for the whole test run, emptied after each test."""
import os
from pathlib import Path

import pytest
import requests
from testcontainers.core.container import DockerContainer
from testcontainers.core.wait_strategies import HttpWaitStrategy

# Pin the version: below 1.0 a minor version may change a flag or a setting.
IMAGE = "ghcr.io/tirthpatell/azemu:0.3.0"

# The gateway, and the standard ports of the services these tests use.
GATEWAY, BLOB, KEY_VAULT, SERVICE_BUS = 4577, 10000, 10010, 5672


class Emulator:
    def __init__(self, container: DockerContainer):
        self.host = container.get_container_host_ip()
        self._container = container

    def port(self, container_port: int) -> int:
        """The host port that a container port is published on (a different one for every container)."""
        return int(self._container.get_exposed_port(container_port))

    def url(self, scheme: str, container_port: int, path: str = "") -> str:
        return f"{scheme}://{self.host}:{self.port(container_port)}{path}"

    def reset(self) -> None:
        """Empties every service; the seed file is applied again."""
        requests.post(self.url("http", GATEWAY, "/_azemu/reset"), timeout=10).raise_for_status()

    def ca_file(self, directory: Path) -> str:
        """`ca.pem` of this container, saved to a file: a new authority is made at every start."""
        response = requests.get(self.url("http", GATEWAY, "/_azemu/ca.pem"), timeout=10)
        response.raise_for_status()
        path = directory / "ca.pem"
        path.write_bytes(response.content)
        return str(path)


@pytest.fixture(scope="session")  # scope="function" gives every test a container of its own
def emulator():
    container = DockerContainer(IMAGE)
    for port in (GATEWAY, BLOB, KEY_VAULT, SERVICE_BUS):
        # The emulator has no sign-in: publish only on the loopback address (the default is every address).
        # Remove the address for a Docker host on another machine.
        container.ports[str(port)] = ("127.0.0.1", None)
    container.with_env("AZEMU_SEED", "/seed.json")
    if name := os.environ.get("AZEMU_TLS_NAME"):  # for a Docker host that is not "localhost": see the page
        container.with_env("AZEMU_TLS_NAME", name)
    # A seed file: copied into the container (a bind mount would need the file on the Docker host).
    container.with_copy_into_container(Path(__file__).parent / "seed.json", "/seed.json")
    # Ready when the gateway answers 200 on /_azemu/health.
    container.waiting_for(HttpWaitStrategy(GATEWAY, "/_azemu/health").for_status_code(200))
    with container:  # stopped and removed at the end
        yield Emulator(container)


@pytest.fixture(autouse=True)
def reset_after_each_test(emulator):
    yield
    emulator.reset()
