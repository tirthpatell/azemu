"""Message encoding: default text, base64 text and base64 binary policies.

Each test sends with one client and reads back with a client that has a matching
policy, so the test checks what the service stores as well as what the SDK returns.
"""

import base64

import pytest
from azure.core.exceptions import HttpResponseError
from azure.storage.queue import (
    BinaryBase64DecodePolicy,
    BinaryBase64EncodePolicy,
    QueueClient,
    TextBase64DecodePolicy,
    TextBase64EncodePolicy,
)

from queue_support import assert_storage_error

XML_SPECIAL = 'a < b & c > d "quoted" \'single\' </tag>'
UNICODE = "héllo wörld 世界 \U0001f680 Ω"
WHITESPACE = " mess\t age1\n"


def _client_for(emu, queue_name, **policies):
    return QueueClient.from_connection_string(emu.storage_conn_str(), queue_name, **policies)


@pytest.mark.parametrize(
    "text",
    [
        pytest.param(XML_SPECIAL, id="xml-special-characters"),
        pytest.param(UNICODE, id="unicode"),
        pytest.param(WHITESPACE, id="tabs-newlines-leading-space"),
    ],
)
def test_default_text_policy_round_trips(queue, text):
    queue.send_message(text)

    (got,) = queue.receive_messages(max_messages=1)

    assert got.content == text


@pytest.mark.not_on_azurite(
    reason="Azurite accepts an XML control character; Azure returns 400 InvalidXmlDocument (recording test_message_text_xml_invalid_chars)"
)
def test_default_text_policy_rejects_control_character_with_invalid_xml(queue):
    with pytest.raises(HttpResponseError) as exc:
        queue.send_message("\x01")

    assert_storage_error(exc.value, 400, "InvalidXmlDocument")


def test_text_base64_policy_round_trips_text(emu, queue_name, queue):
    writer = _client_for(emu, queue_name, message_encode_policy=TextBase64EncodePolicy(),
                         message_decode_policy=TextBase64DecodePolicy())
    writer.send_message(XML_SPECIAL + " " + UNICODE)

    got = writer.receive_message()

    assert got.content == XML_SPECIAL + " " + UNICODE


def test_text_base64_policy_stores_base64_on_the_wire(emu, queue_name, queue):
    writer = _client_for(emu, queue_name, message_encode_policy=TextBase64EncodePolicy())
    writer.send_message("hello <world>")

    (raw,) = queue.peek_messages()

    assert raw.content == base64.b64encode("hello <world>".encode("utf-8")).decode("ascii")


def test_binary_base64_policy_round_trips_bytes(emu, queue_name):
    payload = b"\x00\x01\xfe\xff binary \x80"
    client = _client_for(emu, queue_name, message_encode_policy=BinaryBase64EncodePolicy(),
                         message_decode_policy=BinaryBase64DecodePolicy())
    client.create_queue()
    client.send_message(payload)

    got = client.receive_message()

    assert got.content == payload


def test_binary_base64_policy_stores_base64_on_the_wire(emu, queue_name):
    payload = b"\x00\x01\xfe\xff"
    client = _client_for(emu, queue_name, message_encode_policy=BinaryBase64EncodePolicy())
    client.create_queue()
    client.send_message(payload)

    (raw,) = client.peek_messages()

    assert raw.content == base64.b64encode(payload).decode("ascii")

