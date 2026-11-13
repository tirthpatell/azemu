"""Error codes and status codes for missing resources, across many operations."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.storage.blob import BlobBlock, ContentSettings

from blobutil import assert_error

MISSING_BLOB_OPS = {
    "get_blob_properties": lambda b: b.get_blob_properties(),
    "download_blob": lambda b: b.download_blob().readall(),
    "delete_blob": lambda b: b.delete_blob(),
    "set_blob_metadata": lambda b: b.set_blob_metadata({"a": "b"}),
    "set_http_headers": lambda b: b.set_http_headers(ContentSettings(content_type="a/b")),
    "create_snapshot": lambda b: b.create_snapshot(),
    "acquire_lease": lambda b: b.acquire_lease(),
    "get_block_list": lambda b: b.get_block_list("all"),
    "get_blob_tags": lambda b: b.get_blob_tags(),
    "set_standard_blob_tier": lambda b: b.set_standard_blob_tier("Cool"),
    "append_block": lambda b: b.append_block(b"x"),
}


@pytest.mark.parametrize("op", sorted(MISSING_BLOB_OPS))
def test_missing_blob_is_blob_not_found(container, blob_name, op):
    with pytest.raises(ResourceNotFoundError) as ei:
        MISSING_BLOB_OPS[op](container.get_blob_client(blob_name))
    assert_error(ei, "BlobNotFound", 404)


@pytest.mark.parametrize("op", sorted(MISSING_BLOB_OPS))
def test_missing_container_is_container_not_found(service, uniq, op):
    b = service.get_blob_client(uniq("nocontainer"), "blob")
    with pytest.raises(ResourceNotFoundError) as ei:
        MISSING_BLOB_OPS[op](b)
    assert_error(ei, "ContainerNotFound", 404)


def test_commit_block_list_in_missing_container(service, uniq):
    b = service.get_blob_client(uniq("nocontainer"), "blob")
    with pytest.raises(ResourceNotFoundError) as ei:
        b.commit_block_list([BlobBlock("x")])
    assert_error(ei, "ContainerNotFound", 404)


def test_stage_block_in_missing_container(service, uniq):
    b = service.get_blob_client(uniq("nocontainer"), "blob")
    with pytest.raises(ResourceNotFoundError) as ei:
        b.stage_block("blk00001", b"x")
    assert_error(ei, "ContainerNotFound", 404)


def test_exists_never_raises_for_missing_things(service, container, uniq):
    assert container.get_blob_client(uniq("none")).exists() is False
    assert service.get_blob_client(uniq("nocontainer"), "blob").exists() is False
    assert service.get_container_client(uniq("nocontainer")).exists() is False


def test_error_exposes_code_status_and_message(container, blob_name):
    with pytest.raises(ResourceNotFoundError) as ei:
        container.get_blob_client(blob_name).download_blob()
    err = ei.value
    assert err.error_code == "BlobNotFound"
    assert err.status_code == 404
    assert "The specified blob does not exist" in str(err)
    assert err.response is not None


def test_head_error_code_comes_from_header(container, blob_name):
    """HEAD has no body, so the SDK must get the code from x-ms-error-code."""
    with pytest.raises(ResourceNotFoundError) as ei:
        container.get_blob_client(blob_name).get_blob_properties()
    assert ei.value.error_code == "BlobNotFound"
    assert ei.value.response.headers["x-ms-error-code"] == "BlobNotFound"


def test_wrong_blob_type_operation_is_invalid_blob_type(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.create_append_blob()
    with pytest.raises(HttpResponseError) as ei:
        b.upload_page(b"x" * 512, offset=0, length=512)
    assert_error(ei, "InvalidBlobType", 409)


def test_lease_error_status_is_412_for_blob_writes_and_409_for_lease_ops(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x")
    lease = b.acquire_lease()
    with pytest.raises(HttpResponseError) as ei:
        b.set_blob_metadata({"a": "b"})
    assert ei.value.status_code == 412
    with pytest.raises(HttpResponseError) as ei:
        b.acquire_lease()
    assert ei.value.status_code == 409
    lease.release()
