"""A Shared Key request whose headers hold non-ASCII text is checked like any other.

The Python SDK signs the Unicode string and puts each character up to U+00FF on the wire as one Latin-1
byte (it cannot send more: `UnicodeEncodeError`). Other clients (Go) send UTF-8. Either way the string to
sign holds the characters, encoded as UTF-8. The signature must verify, and then the service gives its own
answer: Queue metadata values must be ASCII, so `InvalidMetadata` (400), not `AuthenticationFailed` (403).
"""

import base64
import hashlib
import hmac
from email.utils import formatdate

import pytest
import requests
from azure.core.exceptions import HttpResponseError
from azure.storage.queue import QueueServiceClient

from queue_support import DEV_ACCOUNT, unique_queue_name

DEV_KEY = (
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/"
    "K1SZFPTOtr/KBHBeksoGMGw=="
)


def test_sdk_latin1_metadata_value_gets_invalid_metadata_not_authentication_failed(emu):
    svc = QueueServiceClient.from_connection_string(emu.storage_conn_str(account=DEV_ACCOUNT, key=DEV_KEY))
    with pytest.raises(HttpResponseError) as e:
        svc.create_queue(unique_queue_name(), metadata={"owner": "café"})
    assert e.value.status_code == 400
    assert e.value.error_code.value == "InvalidMetadata"


def test_wrong_key_with_non_ascii_metadata_is_still_authentication_failed(emu):
    wrong = base64.b64encode(bytes(range(32))).decode()
    svc = QueueServiceClient.from_connection_string(emu.storage_conn_str(account=DEV_ACCOUNT, key=wrong))
    with pytest.raises(HttpResponseError) as e:
        svc.create_queue(unique_queue_name(), metadata={"owner": "café"})
    assert e.value.status_code == 403
    assert e.value.error_code.value == "AuthenticationFailed"


@pytest.mark.parametrize("wire", ["latin-1", "utf-8"])
def test_signature_over_the_utf8_bytes_verifies_for_either_wire_encoding(emu, wire):
    name = unique_queue_name()
    date = formatdate(usegmt=True)
    version = "2023-11-03"
    value = "café"
    # Microsoft Learn, "Authorize with Shared Key", Blob/Queue: 12 lines of standard headers (all empty here,
    # Content-Length is empty when zero), canonicalized headers, canonicalized resource.
    sts = "PUT" + "\n" * 12 + f"x-ms-date:{date}\nx-ms-meta-owner:{value}\nx-ms-version:{version}\n/{DEV_ACCOUNT}/{name}"
    sig = base64.b64encode(hmac.new(base64.b64decode(DEV_KEY), sts.encode("utf-8"), hashlib.sha256).digest()).decode()
    r = requests.put(
        f"http://127.0.0.1:{emu.port}/{name}",
        headers={
            "Host": f"{DEV_ACCOUNT}.queue.core.localhost:{emu.port}",
            "x-ms-date": date,
            "x-ms-version": version,
            "x-ms-meta-owner": value.encode(wire),
            "Authorization": f"SharedKey {DEV_ACCOUNT}:{sig}",
        },
        timeout=10,
    )
    assert r.status_code == 400, r.text
    assert r.headers["x-ms-error-code"] == "InvalidMetadata"
    # the same request with a signature over different text is rejected at the door
    bad = sts.replace(value, "cafe")
    sig = base64.b64encode(hmac.new(base64.b64decode(DEV_KEY), bad.encode("utf-8"), hashlib.sha256).digest()).decode()
    r = requests.put(
        f"http://127.0.0.1:{emu.port}/{name}",
        headers={
            "Host": f"{DEV_ACCOUNT}.queue.core.localhost:{emu.port}",
            "x-ms-date": date,
            "x-ms-version": version,
            "x-ms-meta-owner": value.encode(wire),
            "Authorization": f"SharedKey {DEV_ACCOUNT}:{sig}",
        },
        timeout=10,
    )
    assert r.status_code == 403
    assert r.headers["x-ms-error-code"] == "AuthenticationFailed"
