"""Fixtures for the Azure Files SDK tests (azure-storage-file-share 12.27.1).

The tests describe what Azure Files does through the SDK and through raw REST requests. They run against the emulator by
default and against a real storage account with `AZEMU_AZURE=1` (see README.md, "Azure Files"): then the account name and key
come from `AZ_STORAGE_ACCOUNT` and `AZ_STORAGE_KEY` and nothing calls the management plane.

Note: this directory is called ``file``; do not add an __init__.py here.
"""

import pytest
from azure.storage.fileshare import ShareServiceClient

import fileutil


@pytest.fixture(scope="session")
def azemu_service():
    return "file"


@pytest.fixture
def account(emu):
    return fileutil.account_name(emu)


@pytest.fixture
def conn_str(emu):
    return fileutil.conn_str(emu)


@pytest.fixture
def service(conn_str):
    return ShareServiceClient.from_connection_string(conn_str)


@pytest.fixture
def share_name():
    return fileutil.unique("sh")


@pytest.fixture
def share(service, share_name):
    """A new share, deleted (with its snapshots) when the test ends."""
    client = service.create_share(share_name)
    yield client
    fileutil.delete_share(client)


@pytest.fixture(scope="session")
def _azure_shared_share(emu):
    """On Azure one share serves the whole run: a share that is deleted stays on the account for the soft delete days, so the
    tests that only need directories and files do not make one each."""
    client = ShareServiceClient.from_connection_string(fileutil.conn_str(emu)).create_share(fileutil.unique("shs"))
    yield client
    fileutil.delete_share(client)


@pytest.fixture
def shared(emu, request, service):
    """A share for tests that only need directories and files: on the emulator a new one per test (it is reset between
    tests anyway), on Azure the one of the run. Tests work inside a directory of their own (`work`)."""
    if emu.azure:
        yield request.getfixturevalue("_azure_shared_share")
        return
    client = service.create_share(fileutil.unique("shm"))
    yield client
    fileutil.delete_share(client)


@pytest.fixture
def work(shared):
    """A directory of its own inside the module's share."""
    return shared.create_directory(fileutil.unique("d"))
