"""Fixtures for the storage-events suite: Blob Storage events delivered through Event Grid.

This suite needs the unified binary (`target/debug/azemu`): Blob, Queue, Service Bus, Event Hubs and Event Grid in one
process. The expected event bodies are written from the Microsoft system event schemas (see `seutil.py`), not
from the emulator's code.
"""

import pytest
from azure.storage.blob import BlobServiceClient
from azure.storage.filedatalake import DataLakeServiceClient
from azure.storage.queue import QueueServiceClient

import seutil


@pytest.fixture(scope="session")
def azemu_service():
    return "azemu"  # the unified binary, not azemu-<service>


@pytest.fixture
def blob_service(emu):
    return BlobServiceClient.from_connection_string(emu.storage_conn_str(seutil.ACCOUNT, seutil.KEY))


@pytest.fixture
def dl_service(emu):
    return DataLakeServiceClient(account_url=emu.url("dfs", seutil.ACCOUNT), credential=seutil.KEY)


@pytest.fixture
def queue_service(emu):
    return QueueServiceClient.from_connection_string(emu.storage_conn_str(seutil.ACCOUNT, seutil.KEY))


@pytest.fixture
def container(blob_service):
    return blob_service.create_container(seutil.uniq("c"))


@pytest.fixture
def hook():
    wh = seutil.Webhook()
    yield wh
    wh.stop()
