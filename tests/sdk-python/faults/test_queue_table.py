"""Queue and Table: 503 ServerBusy is retried by the storage clients; with retries off it is an HttpResponseError."""

import uuid

import pytest
from azure.core.exceptions import HttpResponseError
from azure.data.tables import TableServiceClient
from azure.storage.queue import QueueServiceClient

from faultutil import Stopwatch

FAST = dict(retry_total=4, initial_backoff=1, increment_base=2)


@pytest.fixture
def queue_name(emu):
    name = "q" + uuid.uuid4().hex[:10]
    QueueServiceClient.from_connection_string(emu.storage_conn_str()).create_queue(name)
    return name


@pytest.fixture
def table_name(emu):
    name = "t" + uuid.uuid4().hex[:10]
    TableServiceClient.from_connection_string(emu.storage_conn_str()).create_table(name)
    return name


def test_queue_retries_a_throttled_send_and_then_succeeds(emu, faults, queue_name):
    faults.add(service="queue", method="POST", path=f"/{queue_name}/messages", times=2, fault={"kind": "throttle", "retryAfterSeconds": 1})
    with QueueServiceClient.from_connection_string(emu.storage_conn_str(), **FAST) as svc:
        q = svc.get_queue_client(queue_name)
        with Stopwatch() as t:
            sent = q.send_message("hello")
        assert sent.id
        assert [m.content for m in q.peek_messages()] == ["hello"]
    assert faults.fired() == 2
    assert t.seconds >= 1.0


def test_queue_with_retries_off_raises_server_busy(emu, faults, queue_name):
    faults.add(service="queue", method="POST", path=f"/{queue_name}/messages", fault={"kind": "throttle"})
    with QueueServiceClient.from_connection_string(emu.storage_conn_str(), retry_total=0) as svc:
        with pytest.raises(HttpResponseError) as e:
            svc.get_queue_client(queue_name).send_message("hello")
    assert (e.value.status_code, e.value.error_code) == (503, "ServerBusy")
    # nothing was stored by the request that was refused
    with QueueServiceClient.from_connection_string(emu.storage_conn_str(), retry_total=0) as svc:
        assert list(svc.get_queue_client(queue_name).peek_messages()) == []


def test_table_retries_a_throttled_insert_and_then_succeeds(emu, faults, table_name):
    faults.add(service="table", method="POST", path=f"/{table_name}*", times=2, fault={"kind": "throttle", "retryAfterSeconds": 1})
    with TableServiceClient.from_connection_string(emu.storage_conn_str(), **FAST) as svc:
        t = svc.get_table_client(table_name)
        with Stopwatch() as w:
            t.create_entity({"PartitionKey": "p", "RowKey": "r", "v": 1})
        assert t.get_entity("p", "r")["v"] == 1
    assert faults.fired() == 2
    assert w.seconds >= 1.0


def test_table_with_retries_off_raises_server_busy(emu, faults, table_name):
    faults.add(service="table", method="POST", path=f"/{table_name}*", fault={"kind": "throttle"})
    with TableServiceClient.from_connection_string(emu.storage_conn_str(), retry_total=0) as svc:
        with pytest.raises(HttpResponseError) as e:
            svc.get_table_client(table_name).create_entity({"PartitionKey": "p", "RowKey": "r"})
    assert e.value.status_code == 503
    assert e.value.response.headers["x-ms-error-code"] == "ServerBusy"
    assert "unable to receive requests" in str(e.value)
