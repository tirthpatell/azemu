"""Fixtures for the Data Lake Storage Gen2 SDK conformance tests.

Data Lake is served by the same binary as Blob (azemu-blob): one account, two
endpoints (`<account>.dfs.core.localhost` and `<account>.blob.core.localhost`,
same port).

Markers (all optional, registered below):
    docs      behaviour taken from the Azure REST documentation / SDK source and
              NOT present in the recorded real traffic (refs/recordings).
              Deselect with `-m "not docs"` if you only want recorded behaviour.
"""

import pytest
from azure.storage.blob import BlobServiceClient
from azure.storage.filedatalake import DataLakeServiceClient

import dlutil
from dlutil import DEV_ACCOUNT, DEV_KEY


def pytest_configure(config):
    config.addinivalue_line(
        "markers",
        "docs: behaviour from Azure docs / SDK source, not in the recorded real traffic",
    )


@pytest.fixture(scope="session")
def azemu_service():
    return "blob"  # Data Lake is served by the same binary as Blob (azemu-blob)


@pytest.fixture
def uniq():
    return dlutil.uniq


@pytest.fixture
def service(emu):
    """DataLakeServiceClient for the dev account, shared-key authenticated."""
    return DataLakeServiceClient(account_url=emu.url("dfs", DEV_ACCOUNT), credential=DEV_KEY)


@pytest.fixture
def blob_service(emu):
    """BlobServiceClient for the same account (the blob endpoint)."""
    return BlobServiceClient(account_url=emu.url("blob", DEV_ACCOUNT), credential=DEV_KEY)


@pytest.fixture
def fs(service, uniq):
    """A freshly created filesystem (FileSystemClient) with a unique name."""
    return service.create_file_system(uniq("fs"))


@pytest.fixture
def fs2(service, uniq):
    """A second, independent filesystem."""
    return service.create_file_system(uniq("fs"))


@pytest.fixture
def make_fs(service, uniq):
    def make(prefix="fs", **kwargs):
        return service.create_file_system(uniq(prefix), **kwargs)

    return make


@pytest.fixture
def blob_container(blob_service, fs):
    """BlobServiceClient's view of the filesystem `fs` (it is a container)."""
    return blob_service.get_container_client(fs.file_system_name)


@pytest.fixture
def raw(emu):
    """Raw HTTP helper (requests) authenticated with an account SAS."""
    return dlutil.Raw(emu)
