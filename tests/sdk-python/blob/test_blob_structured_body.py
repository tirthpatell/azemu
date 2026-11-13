"""`x-ms-structured-body: XSM/1.0; properties=crc64` (structured messages), over raw HTTP.

The message layout is the one the Go SDK writes (azblob `internal/shared/structured_message.go`) and the Python
SDK's recordings show (a 5150 byte block is 5189 bytes in a message: 39 bytes of framing for one segment).
The CRC is CRC-64/NVME, written here bit by bit, not taken from the emulator.
"""

import struct
from datetime import datetime, timedelta, timezone

import pytest
import requests
from azure.storage.blob import AccountSasPermissions, ResourceTypes, generate_account_sas

XSM = "XSM/1.0; properties=crc64"


def crc64(data: bytes) -> int:
    c = 0xFFFFFFFFFFFFFFFF
    for b in data:
        c ^= b
        for _ in range(8):
            c = (c >> 1) ^ 0x9A6C9329AC4BC9B5 if c & 1 else c >> 1
    return c ^ 0xFFFFFFFFFFFFFFFF


def message(data: bytes, segment: int = 4 * 1024 * 1024) -> bytes:
    parts = [data[i : i + segment] for i in range(0, len(data), segment)] or [b""]
    total = 13 + sum(10 + len(p) + 8 for p in parts) + 8
    out = struct.pack("<BQHH", 1, total, 1, len(parts))
    for i, p in enumerate(parts, 1):
        out += struct.pack("<HQ", i, len(p)) + p + struct.pack("<Q", crc64(p))
    return out + struct.pack("<Q", crc64(data))


@pytest.fixture
def sas(account_name, account_key):
    return generate_account_sas(
        account_name,
        account_key,
        resource_types=ResourceTypes(service=True, container=True, object=True),
        permission=AccountSasPermissions(read=True, write=True, delete=True, list=True, add=True, create=True),
        expiry=datetime.now(timezone.utc) + timedelta(hours=1),
    )


@pytest.fixture
def u(container, sas):
    return lambda name, extra="": f"{container.url}/{name}?{extra}{'&' if extra else ''}{sas}"


def payload(n):
    return bytes((i * 7) % 251 for i in range(n))


def test_framing_overhead_matches_the_recorded_size():
    assert len(message(payload(5150))) == 5189


def test_put_blob_stores_the_payload_and_get_wraps_on_request(u):
    data = payload(100_000)
    r = requests.put(
        u("s"),
        data=message(data, 32 * 1024),
        headers={"x-ms-blob-type": "BlockBlob", "x-ms-structured-body": XSM, "x-ms-structured-content-length": str(len(data))},
    )
    assert r.status_code == 201, r.text
    assert r.headers["x-ms-structured-body"] == XSM
    assert requests.get(u("s")).content == data
    r = requests.get(u("s"), headers={"x-ms-structured-body": XSM})
    assert r.status_code == 200
    assert r.headers["x-ms-structured-body"] == XSM
    assert r.headers["x-ms-structured-content-length"] == str(len(data))
    assert r.content == message(data)
    # a range, as in the recording: Content-Range is the blob's, the length header the range's
    r = requests.get(u("s"), headers={"x-ms-structured-body": XSM, "x-ms-range": "bytes=0-1535"})
    assert r.status_code == 206
    assert r.headers["Content-Range"] == "bytes 0-1535/100000"
    assert r.headers["x-ms-structured-content-length"] == "1536"
    assert len(r.content) == 1575


def test_block_and_append_block_take_messages(u):
    data = payload(5150)
    h = {"x-ms-structured-body": XSM, "x-ms-structured-content-length": str(len(data))}
    r = requests.put(u("b", "comp=block&blockid=MQ%3D%3D"), data=message(data), headers=h)
    assert r.status_code == 201 and r.headers["x-ms-structured-body"] == XSM
    r = requests.put(u("b", "comp=blocklist"), data="<BlockList><Latest>MQ==</Latest></BlockList>")
    assert r.status_code == 201
    assert requests.get(u("b")).content == data
    assert requests.put(u("a"), headers={"x-ms-blob-type": "AppendBlob"}).status_code == 201
    r = requests.put(u("a", "comp=appendblock"), data=message(data), headers=h)
    assert r.status_code == 201 and r.headers["x-ms-structured-body"] == XSM
    assert requests.get(u("a")).content == data


def test_a_damaged_message_is_refused_and_nothing_is_stored(u):
    data = payload(1000)
    base = {"x-ms-blob-type": "BlockBlob", "x-ms-structured-body": XSM, "x-ms-structured-content-length": "1000"}
    bad = bytearray(message(data, 400))
    bad[30] ^= 1
    r = requests.put(u("x"), data=bytes(bad), headers=base)
    assert r.status_code == 400 and r.headers["x-ms-error-code"] == "Crc64Mismatch"
    r = requests.put(u("x"), data=data, headers=base)  # the plain data is not a message
    assert r.status_code == 400 and "x-ms-structured-body" in r.text
    r = requests.put(u("x"), data=message(data), headers={**base, "x-ms-structured-content-length": "999"})
    assert r.status_code == 400
    assert requests.get(u("x")).status_code == 404
