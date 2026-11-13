"""Raw HTTP checks: error bodies, response headers and XML shapes.

These tests bypass the SDK's parsing. Authentication is an account SAS made with
the SDK and appended to the URL. Element and header names come from the Queue
REST spec and the recordings in refs/recordings.

XML is parsed with the standard library. The input is always a response from the
local storage target under test, never untrusted data. defusedxml is not in the venv.
"""

import re
from datetime import timedelta
from email.utils import parsedate_to_datetime
from xml.etree import ElementTree

import pytest
import requests
from azure.storage.queue import (
    AccountSasPermissions,
    QueueServiceClient,
    ResourceTypes,
    generate_account_sas,
)

from queue_support import parse_conn_str, unique_queue_name, utc_now

TIMEOUT = 10


@pytest.fixture
def raw(emu):
    """Base URL of the account and a SAS token that covers queue creation and messages."""
    conn = emu.storage_conn_str()
    parts = parse_conn_str(conn)
    service = QueueServiceClient.from_connection_string(conn)
    sas = generate_account_sas(
        parts["AccountName"],
        parts["AccountKey"],
        resource_types=ResourceTypes(service=True, container=True, object=True),
        permission=AccountSasPermissions(
            read=True, write=True, delete=True, list=True, add=True, create=True, update=True, process=True
        ),
        expiry=utc_now() + timedelta(hours=1),
    )
    return service.url.rstrip("/"), sas


def _url(base, path, query, sas):
    parts = [p for p in (query, sas) if p]
    return f"{base}/{path}" + (("?" + "&".join(parts)) if parts else "")


def test_raw_missing_queue_error_body_is_xml_error_document(emu, raw):
    base, sas = raw
    url = _url(base, unique_queue_name(), "comp=metadata", sas)

    resp = requests.get(url, timeout=TIMEOUT)

    assert resp.status_code == 404
    assert resp.headers["Content-Type"].startswith("application/xml")
    root = ElementTree.fromstring(resp.content)
    assert root.tag == "Error"
    assert root.findtext("Code") == "QueueNotFound"
    assert root.findtext("Message", "").startswith("The specified queue does not exist.")


def test_raw_error_response_carries_error_code_and_request_id_headers(emu, raw):
    base, sas = raw
    url = _url(base, unique_queue_name(), "comp=metadata", sas)

    resp = requests.get(url, timeout=TIMEOUT)

    assert resp.headers["x-ms-error-code"] == "QueueNotFound"
    assert re.fullmatch(r"[0-9a-f-]{36}", resp.headers["x-ms-request-id"])


def test_raw_success_response_carries_request_id_and_date(emu, raw):
    base, sas = raw
    url = _url(base, unique_queue_name(), None, sas)

    resp = requests.put(url, timeout=TIMEOUT)

    assert resp.status_code == 201
    assert re.fullmatch(r"[0-9a-f-]{36}", resp.headers["x-ms-request-id"])
    assert resp.headers.get("Date")
    assert resp.headers.get("x-ms-version")


@pytest.mark.not_on_azurite(
    reason="Azurite answers with its own x-ms-version (2026-06-06) instead of echoing the request version; Azure echoes 2021-02-12 (recording test_queue.py create)"
)
def test_raw_response_x_ms_version_echoes_the_request_version(emu, raw):
    base, sas = raw
    url = _url(base, unique_queue_name(), None, sas)

    resp = requests.put(url, headers={"x-ms-version": "2021-02-12"}, timeout=TIMEOUT)

    assert resp.status_code == 201
    assert resp.headers["x-ms-version"] == "2021-02-12"


def test_raw_create_queue_again_without_metadata_is_204(emu, raw):
    base, sas = raw
    url = _url(base, unique_queue_name(), None, sas)

    assert requests.put(url, timeout=TIMEOUT).status_code == 201
    resp = requests.put(url, timeout=TIMEOUT)

    assert resp.status_code == 204
    assert resp.content == b""
    assert "x-ms-error-code" not in resp.headers


def test_raw_create_queue_again_with_same_metadata_is_204(emu, raw):
    base, sas = raw
    url = _url(base, unique_queue_name(), None, sas)
    headers = {"x-ms-meta-env": "test"}

    assert requests.put(url, headers=headers, timeout=TIMEOUT).status_code == 201
    resp = requests.put(url, headers=headers, timeout=TIMEOUT)

    assert resp.status_code == 204
    assert resp.content == b""


def test_raw_create_queue_again_with_different_metadata_is_409_queue_already_exists(emu, raw):
    # Recording test_create_queue_fail_on_exist_different_metadata: 201, then 409.
    base, sas = raw
    url = _url(base, unique_queue_name(), None, sas)

    assert requests.put(url, timeout=TIMEOUT).status_code == 201
    resp = requests.put(url, headers={"x-ms-meta-val": "value"}, timeout=TIMEOUT)

    assert resp.status_code == 409
    assert resp.headers["x-ms-error-code"] == "QueueAlreadyExists"
    assert ElementTree.fromstring(resp.content).findtext("Code") == "QueueAlreadyExists"


def test_raw_send_message_returns_201_with_queue_messages_list_xml(emu, raw):
    base, sas = raw
    queue_name = unique_queue_name()
    requests.put(_url(base, queue_name, None, sas), timeout=TIMEOUT).raise_for_status()
    body = b"<QueueMessage><MessageText>raw body</MessageText></QueueMessage>"

    resp = requests.post(
        _url(base, f"{queue_name}/messages", None, sas),
        data=body,
        headers={"Content-Type": "application/xml"},
        timeout=TIMEOUT,
    )

    assert resp.status_code == 201
    root = ElementTree.fromstring(resp.content)
    assert root.tag == "QueueMessagesList"
    message = root.find("QueueMessage")
    for tag in ("MessageId", "InsertionTime", "ExpirationTime", "PopReceipt", "TimeNextVisible"):
        assert message.findtext(tag), f"missing {tag}"


def test_raw_get_messages_returns_queue_message_elements(emu, raw, queue):
    base, sas = raw
    sent = queue.send_message("shape check")

    resp = requests.get(_url(base, f"{queue.queue_name}/messages", "numofmessages=1", sas), timeout=TIMEOUT)

    assert resp.status_code == 200
    root = ElementTree.fromstring(resp.content)
    assert root.tag == "QueueMessagesList"
    message = root.find("QueueMessage")
    assert message.findtext("MessageId") == sent.id
    for tag in ("InsertionTime", "ExpirationTime", "PopReceipt", "TimeNextVisible"):
        assert message.findtext(tag), f"missing {tag}"
    assert message.findtext("DequeueCount") == "1"
    assert message.findtext("MessageText") == "shape check"
    # RFC 1123 timestamps, e.g. "Thu, 27 Oct 2022 22:04:56 GMT"
    assert parsedate_to_datetime(message.findtext("InsertionTime")).tzinfo is not None

