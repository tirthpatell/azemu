"""Storage Analytics logging (classic): the `$logs` container and the log blobs.

Microsoft Learn: "Azure Storage Analytics logging", "Storage Analytics log format", "Storage Analytics logged operations and
status messages". On Azure the log blobs appear up to an hour after the requests; the emulator writes them after a short
delay (`AZEMU_BLOB_LOG_DELAY`, 3 seconds by default), so these tests wait up to 40 seconds. They cannot run against Azurite
(it has no Storage Analytics logging).
"""

import html
import re
import time

import pytest
import requests
from azure.core.exceptions import HttpResponseError
from azure.storage.blob import (
    BlobAnalyticsLogging,
    BlobSasPermissions,
    RetentionPolicy,
    generate_blob_sas,
)

from blobutil import future

pytestmark = pytest.mark.not_on_azurite(reason="Azurite has no Storage Analytics logging")

LOG_NAME = re.compile(r"^blob/\d{4}/\d{2}/\d{2}/\d{2}00/\d{6}\.log$")
TIMESTAMP = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{7}Z$")
META_TIME = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$")


def set_logging(service, read=False, write=False, delete=False, days=1):
    service.set_service_properties(
        analytics_logging=BlobAnalyticsLogging(
            version="1.0", read=read, write=write, delete=delete, retention_policy=RetentionPolicy(enabled=True, days=days)
        )
    )


@pytest.fixture
def logging_on(service):
    """`logging_on(read=..., write=..., delete=...)`; logging is switched off again after the test."""
    yield lambda **kw: set_logging(service, **kw)
    try:
        service.set_service_properties(
            analytics_logging=BlobAnalyticsLogging(read=False, write=False, delete=False, retention_policy=RetentionPolicy(enabled=False))
        )
    except Exception:
        pass


def fields(line):
    """One log line split into its fields: `;` separates them, a quoted field may contain one."""
    out, cur, quoted = [], "", False
    for ch in line:
        if ch == '"':
            quoted = not quoted
            cur += ch
        elif ch == ";" and not quoted:
            out.append(cur)
            cur = ""
        else:
            cur += ch
    out.append(cur)
    return out


def log_blobs(service):
    return list(service.get_container_client("$logs").list_blobs(name_starts_with="blob/", include=["metadata"]))


def log_lines(service):
    """Every line of every log blob: (blob name, fields)."""
    cc = service.get_container_client("$logs")
    out = []
    for b in log_blobs(service):
        for line in cc.download_blob(b.name).readall().decode().splitlines():
            out.append((b.name, fields(line)))
    return out


def wait_for_line(service, match, timeout=40):
    """Poll the logs until a line satisfies `match(fields)`; return it (or fail with what was there)."""
    deadline = time.time() + timeout
    seen = []
    while time.time() < deadline:
        seen = log_lines(service)
        for _, f in seen:
            if match(f):
                return f
        time.sleep(1)
    pytest.fail(f"no matching log line after {timeout}s; {len(seen)} lines in $logs, last ones: {[f[2:5] + f[11:13] for _, f in seen[-5:]]}")


def has_key(f, *parts):
    return len(f) >= 13 and all(p in f[12] for p in parts)


# --- the container ---------------------------------------------------------------------------------------------------


def test_logs_container_exists_once_logging_is_on_and_is_not_in_a_plain_listing(service, logging_on):
    logging_on(write=True)
    props = service.get_container_client("$logs").get_container_properties()
    assert props.name == "$logs"
    assert "$logs" not in [c.name for c in service.list_containers()]
    assert "$logs" not in [c.name for c in service.list_containers(name_starts_with="$")]
    assert "$logs" in [c.name for c in service.list_containers(include_system=True)]


def test_logs_container_cannot_be_deleted_but_its_blobs_can(service, container, blob_name, logging_on):
    logging_on(write=True)
    container.get_blob_client(blob_name).upload_blob(b"x")
    wait_for_line(service, lambda f: f[2] == "PutBlob" and has_key(f, container.container_name, blob_name))
    with pytest.raises(HttpResponseError) as e:
        service.get_container_client("$logs").delete_container()
    assert e.value.status_code in (400, 409)
    # one log blob can be deleted
    cc = service.get_container_client("$logs")
    name = log_blobs(service)[0].name
    cc.delete_blob(name)
    assert name not in [b.name for b in log_blobs(service)]
    assert cc.get_container_properties().name == "$logs"


# --- the log blobs and their lines ---------------------------------------------------------------------------------------


def test_a_put_blob_is_logged_in_the_documented_format(service, container, blob_name, account_name, logging_on):
    logging_on(write=True)
    seen = {}
    blob = container.get_blob_client(blob_name)
    blob.upload_blob(b"hello", client_request_id="client-id-123", raw_response_hook=lambda r: seen.update({k.lower(): v for k, v in r.http_response.headers.items()}))
    f = wait_for_line(service, lambda f: f[2] == "PutBlob" and has_key(f, container.container_name, blob_name))
    assert len(f) == 30, f
    assert f[0] == "1.0"
    assert TIMESTAMP.match(f[1]), f[1]
    assert f[3] == "Success" and f[4] == "201"
    assert f[7] == "authenticated" and f[8] == account_name and f[9] == account_name and f[10] == "blob"
    assert f[11].startswith('"') and f[11].endswith('"') and f"/{container.container_name}/{blob_name}" in f[11]
    assert f[12] == f'"/{account_name}/{container.container_name}/{blob_name}"'
    assert f[13] == seen["x-ms-request-id"]
    assert f[14] == "0"
    assert f[16] and re.match(r"^\d{4}-\d{2}-\d{2}$", f[16]), "the x-ms-version of the request"
    assert f[21] == "5", "request content length"
    assert html.unescape(f[24][1:-1]) == seen["etag"], (f[24], seen["etag"])
    assert f[25].endswith(' GMT"') and f[25].startswith('"'), f[25]
    assert "client-id-123" in f[29], f[29]


def test_log_blobs_are_named_by_hour_and_carry_the_documented_metadata(service, container, blob_name, logging_on):
    logging_on(write=True)
    container.get_blob_client(blob_name).upload_blob(b"x")
    wait_for_line(service, lambda f: f[2] == "PutBlob" and has_key(f, blob_name))
    blobs = [b for b in log_blobs(service) if "write" in (b.metadata or {}).get("LogType", "")]
    assert blobs
    for b in blobs:
        assert LOG_NAME.match(b.name), b.name
        assert b.blob_type == "BlockBlob"
        assert b.metadata["LogVersion"] == "1.0"
        assert META_TIME.match(b.metadata["StartTime"]) and META_TIME.match(b.metadata["EndTime"])
        assert b.metadata["StartTime"] <= b.metadata["EndTime"]
        assert set(b.metadata["LogType"].split(",")) <= {"read", "write", "delete"}
        assert b.size > 0


def test_only_the_classes_that_are_switched_on_are_logged(service, container, blob_name, logging_on):
    logging_on(read=True, write=False, delete=True)
    blob = container.get_blob_client(blob_name)
    blob.upload_blob(b"data")  # a write: not logged
    blob.download_blob().readall()  # a read
    blob.delete_blob()  # a delete
    wait_for_line(service, lambda f: f[2] == "DeleteBlob" and has_key(f, blob_name))
    mine = [f for _, f in log_lines(service) if has_key(f, container.container_name, blob_name)]
    ops = sorted(f[2] for f in mine)
    assert ops == ["DeleteBlob", "GetBlob"], ops


def test_a_failed_request_is_logged_with_its_error_code(service, container, blob_name, logging_on):
    logging_on(read=True)
    with pytest.raises(HttpResponseError):
        container.get_blob_client(blob_name).get_blob_properties()
    f = wait_for_line(service, lambda f: f[2] == "GetBlobProperties" and has_key(f, container.container_name, blob_name))
    assert f[3] == "BlobNotFound" and f[4] == "404"


def test_copy_blob_logs_three_operations_with_one_request_id(service, container, blob_name, uniq, logging_on):
    logging_on(write=True)
    src = container.get_blob_client(blob_name)
    src.upload_blob(b"copy me")
    dst_name = uniq("d")
    container.get_blob_client(dst_name).start_copy_from_url(src.url)
    wait_for_line(service, lambda f: f[2] == "CopyBlobDestination" and has_key(f, dst_name))
    rows = [f for _, f in log_lines(service) if f[2].startswith("CopyBlob") and has_key(f, dst_name) or (f[2] == "CopyBlobSource" and blob_name in f[12])]
    names = {f[2]: f for f in rows}
    assert {"CopyBlob", "CopyBlobSource", "CopyBlobDestination"} <= set(names), names.keys()
    assert names["CopyBlob"][13] == names["CopyBlobSource"][13] == names["CopyBlobDestination"][13]
    assert [names[n][14] for n in ("CopyBlob", "CopyBlobSource", "CopyBlobDestination")] == ["0", "1", "2"]


def test_a_sas_request_is_logged_and_the_signature_is_not(service, container, blob_name, account_name, account_key, logging_on):
    logging_on(read=True, write=True)
    blob = container.get_blob_client(blob_name)
    blob.upload_blob(b"sas data")
    token = generate_blob_sas(
        account_name, container.container_name, blob_name, account_key=account_key, permission=BlobSasPermissions(read=True), expiry=future()
    )
    sig = re.search(r"(?:^|&)sig=([^&]+)", token).group(1)
    assert requests.get(f"{blob.url}?{token}").status_code == 200
    f = wait_for_line(service, lambda f: f[2] == "GetBlob" and f[7] == "sas" and has_key(f, blob_name))
    assert f[3] == "SASSuccess" and f[8] == "", "no requester account for a SAS request"
    assert "sig=XXXXX" in f[11], f[11]
    text = "".join(";".join(f) for _, f in log_lines(service))
    from urllib.parse import unquote

    assert sig not in text and unquote(sig) not in text, "the SAS signature must never be in the log"
    assert "SharedKey" not in text


# --- maxresults ------------------------------------------------------------------------------------------------------------


def test_maxresults_above_5000_returns_up_to_5000_instead_of_an_error(service, container, uniq):
    """Microsoft Learn, List Blobs and List Containers: more than 5,000 returns up to 5,000; zero or less is a 400."""
    for i in range(3):
        container.get_blob_client(f"b{i}").upload_blob(b"x")
    assert sorted(b.name for b in container.list_blobs(results_per_page=10000)) == ["b0", "b1", "b2"]
    assert container.container_name in [c.name for c in service.list_containers(results_per_page=10000)]
    with pytest.raises(HttpResponseError) as e:
        container._client.container.list_blob_flat_segment(maxresults=0)
    assert e.value.status_code == 400
