"""Blob batch (multipart) API: delete_blobs, set_standard_blob_tier_blobs."""

import pytest
from azure.core.exceptions import HttpResponseError
from azure.storage.blob import PartialBatchErrorException
from azure.storage.blob import StandardBlobTier


def _make(container, n, prefix="b"):
    names = [f"{prefix}{i}" for i in range(n)]
    for name in names:
        container.upload_blob(name, name.encode())
    return names


def test_delete_blobs_removes_all_of_them(container):
    names = _make(container, 5)
    responses = list(container.delete_blobs(*names))
    assert [r.status_code for r in responses] == [202] * 5
    assert list(container.list_blobs()) == []


def test_delete_blobs_keeps_blobs_not_named(container):
    names = _make(container, 3)
    list(container.delete_blobs(names[0], names[2]))
    assert [b.name for b in container.list_blobs()] == [names[1]]


def test_delete_blobs_accepts_blob_properties_items(container):
    _make(container, 2)
    list(container.delete_blobs(*list(container.list_blobs())))
    assert list(container.list_blobs()) == []


def test_delete_blobs_reports_missing_blob_per_item(container):
    names = _make(container, 2)
    responses = list(container.delete_blobs(names[0], "does-not-exist", names[1], raise_on_any_failure=False))
    assert [r.status_code for r in responses] == [202, 404, 202]
    assert list(container.list_blobs()) == []


def test_delete_blobs_raises_partial_batch_error_by_default(container):
    names = _make(container, 1)
    with pytest.raises(PartialBatchErrorException):
        list(container.delete_blobs(names[0], "does-not-exist"))
    assert list(container.list_blobs()) == []  # the good one was still deleted


def test_delete_blobs_with_snapshots_requires_option(container):
    (name,) = _make(container, 1)
    container.get_blob_client(name).create_snapshot()
    responses = list(container.delete_blobs(name, raise_on_any_failure=False))
    assert responses[0].status_code == 409
    responses = list(container.delete_blobs(name, delete_snapshots="include", raise_on_any_failure=False))
    assert responses[0].status_code == 202


def test_delete_blobs_on_missing_container_is_404_either_way(service, uniq):
    """Whether the outer request or every sub-request reports it is not pinned down; the 404 is."""
    c = service.get_container_client(uniq("c"))
    try:
        responses = list(c.delete_blobs("x", "y", raise_on_any_failure=False))
    except HttpResponseError as e:
        assert e.status_code == 404
    else:
        assert [r.status_code for r in responses] == [404, 404]


def test_delete_blobs_batch_of_many(container):
    names = _make(container, 30)
    assert all(r.status_code == 202 for r in container.delete_blobs(*names))


def test_set_standard_blob_tier_blobs(container):
    names = _make(container, 3)
    responses = list(container.set_standard_blob_tier_blobs(StandardBlobTier.COOL, *names))
    assert [r.status_code for r in responses] == [200] * 3
    for n in names:
        assert container.get_blob_client(n).get_blob_properties().blob_tier == "Cool"


def test_set_standard_blob_tier_blobs_missing_blob(container):
    names = _make(container, 1)
    responses = list(
        container.set_standard_blob_tier_blobs(StandardBlobTier.COOL, names[0], "nope", raise_on_any_failure=False)
    )
    assert [r.status_code for r in responses] == [200, 404]


# --- the bytes of the answer ---------------------------------------------------------------------
# A real answer (recorded: test_container_async.pyTestStorageContainerAsynctest_delete_blobs_with_properties_versioning,
# and ..._with_if_tags for parts with a body) has a part without a body as the headers, one blank line and the next
# delimiter; a part with a body as the headers, a blank line, the body and a line break; the closing delimiter has no line
# break after it. The Java batch client reads any text after the blank line as an error body, so one extra empty line
# turns every successful delete into a failure there.


def _raw_batch(container, sas, names, url=None):
    import requests

    boundary = "batch_azemu_py"
    parts = ""
    for i, n in enumerate(names):
        parts += (
            f"--{boundary}\r\nContent-Type: application/http\r\nContent-ID: {i}\r\nContent-Transfer-Encoding: binary\r\n\r\n"
            f"DELETE {container.url}/{n}?{sas} HTTP/1.1\r\nContent-Length: 0\r\n\r\n\r\n"
        )
    body = parts + f"--{boundary}--\r\n"
    return requests.post(
        url or f"{container.url}?restype=container&comp=batch&{sas}",
        data=body.encode(),
        headers={"Content-Type": f"multipart/mixed; boundary={boundary}"},
    )


@pytest.fixture
def batch_sas(account_name, account_key):
    from datetime import datetime, timedelta, timezone

    from azure.storage.blob import AccountSasPermissions, ResourceTypes, generate_account_sas

    return generate_account_sas(
        account_name,
        account_key,
        resource_types=ResourceTypes(service=True, container=True, object=True),
        permission=AccountSasPermissions(read=True, write=True, delete=True, list=True),
        expiry=datetime.now(timezone.utc) + timedelta(hours=1),
    )


def test_batch_answer_has_no_empty_line_after_a_part_without_a_body(container, batch_sas):
    names = _make(container, 2)
    r = _raw_batch(container, batch_sas, names + ["missing"])
    assert r.status_code == 202, r.text
    boundary = r.headers["Content-Type"].split("boundary=")[1]
    text = r.content.decode("utf-8")
    chunks = text.split(f"--{boundary}")
    assert chunks[0] == "" and len(chunks) == 5, text
    assert chunks[4] == "--", "no line break after the closing delimiter"
    # two deletes without a body: headers, one blank line, delimiter
    for ok in chunks[1:3]:
        assert "HTTP/1.1 202 Accepted" in ok
        assert ok.endswith("\r\n\r\n") and not ok.endswith("\r\n\r\n\r\n"), repr(ok)
    # the missing blob: a body, then one line break before the delimiter
    missing = chunks[3]
    assert "HTTP/1.1 404" in missing
    payload = missing.split("\r\n\r\n", 2)[2]
    assert "<Error>" in payload and payload.endswith("</Error>\r\n") and not payload.endswith("\r\n\r\n"), repr(missing)
    assert list(container.list_blobs()) == []


def test_service_level_batch_answer_has_the_same_layout(container, service, batch_sas):
    names = _make(container, 1)
    r = _raw_batch(container, batch_sas, names, url=f"{service.url.rstrip('/')}/?comp=batch&{batch_sas}")
    assert r.status_code == 202, r.text
    boundary = r.headers["Content-Type"].split("boundary=")[1]
    text = r.content.decode("utf-8")
    assert text.endswith(f"\r\n\r\n--{boundary}--"), repr(text)
    assert "\r\n\r\n\r\n" not in text
