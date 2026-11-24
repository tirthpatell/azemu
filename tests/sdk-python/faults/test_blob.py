"""Blob: the client retries a 503 ServerBusy, a dropped connection and shows what it was given when retries are off."""

import uuid

import pytest
from azure.core.exceptions import HttpResponseError, ServiceRequestError, ServiceResponseError
from azure.storage.blob import BlobServiceClient

from faultutil import Stopwatch

FAST = dict(retry_total=4, initial_backoff=1, increment_base=2)


def client(emu, **kw):
    return BlobServiceClient.from_connection_string(emu.storage_conn_str(), **kw)


@pytest.fixture
def blob(emu):
    """A blob that exists, made with a client that is not faulted yet (the rules come after)."""
    c = client(emu)
    name = "c" + uuid.uuid4().hex[:10]
    container = c.create_container(name)
    container.upload_blob("b1", b"hello")
    return name


def test_blob_retries_a_throttled_download_and_then_succeeds(emu, faults, blob):
    faults.add(service="blob", method="GET", path=f"/{blob}/*", times=2, fault={"kind": "throttle", "retryAfterSeconds": 1})
    with client(emu, **FAST) as c:
        with Stopwatch() as t:
            data = c.get_blob_client(blob, "b1").download_blob().readall()
    assert data == b"hello"
    assert faults.fired() == 2
    # the storage retry policy backs off by its own schedule (initial_backoff, then 1 + 2^n ...), not by Retry-After
    assert t.seconds >= 1.0


def test_blob_with_retries_off_raises_server_busy(emu, faults, blob):
    faults.add(service="blob", method="GET", path=f"/{blob}/*", fault={"kind": "throttle", "retryAfterSeconds": 3})
    with client(emu, retry_total=0) as c:
        with pytest.raises(HttpResponseError) as e:
            c.get_blob_client(blob, "b1").download_blob()
    assert e.value.status_code == 503
    assert e.value.error_code == "ServerBusy"
    assert "currently unable to receive requests" in str(e.value)
    assert e.value.response.headers["Retry-After"] == "3"
    assert e.value.response.headers["x-azemu-fault"] == "throttle"


def test_blob_error_rule_gives_the_status_and_code_it_spells_out(emu, faults, blob):
    faults.add(service="blob", method="DELETE", fault={"kind": "error", "status": 500, "code": "InternalError", "message": "on purpose"})
    with client(emu, retry_total=0) as c:
        with pytest.raises(HttpResponseError) as e:
            c.get_blob_client(blob, "b1").delete_blob()
    assert (e.value.status_code, e.value.error_code) == (500, "InternalError")
    assert "on purpose" in str(e.value)


def test_blob_retries_after_a_dropped_connection(emu, faults, blob):
    faults.add(service="blob", method="GET", path=f"/{blob}/*", times=1, fault={"kind": "drop"})
    with client(emu, **FAST) as c:
        assert c.get_blob_client(blob, "b1").download_blob().readall() == b"hello"
    assert faults.fired() == 1


def test_blob_with_retries_off_sees_a_dropped_connection_as_a_transport_error(emu, faults, blob):
    faults.add(service="blob", method="GET", path=f"/{blob}/*", times=1, fault={"kind": "drop"})
    with client(emu, retry_total=0, retry_connect=0, retry_read=0, retry_status=0) as c:
        with pytest.raises((ServiceResponseError, ServiceRequestError)):
            c.get_blob_client(blob, "b1").download_blob()


def test_blob_latency_longer_than_the_client_timeout_fails_the_call(emu, faults, blob):
    faults.add(service="blob", method="GET", path=f"/{blob}/*", times=1, fault={"kind": "latency", "ms": 3000})
    with client(emu, retry_total=0, retry_connect=0, retry_read=0, retry_status=0, read_timeout=1) as c:
        with Stopwatch() as t:
            with pytest.raises((ServiceResponseError, ServiceRequestError)):
                c.get_blob_client(blob, "b1").download_blob()
    assert t.seconds < 2.9, "the client gave up at its own timeout"
    # and with a timeout longer than the latency the call works, late
    faults.add(service="blob", method="GET", path=f"/{blob}/*", times=1, fault={"kind": "latency", "ms": 600})
    with client(emu, retry_total=0) as c:
        with Stopwatch() as t:
            assert c.get_blob_client(blob, "b1").download_blob().readall() == b"hello"
    assert t.seconds >= 0.55
