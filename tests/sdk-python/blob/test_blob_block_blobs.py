"""Block blobs: upload/download, blocks, block lists, ranges, overwrite."""

import base64
import hashlib

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.storage.blob import BlobBlock, BlobServiceClient, BlobType

from blobutil import assert_error, rand_bytes


def _bid(i: int) -> str:
    return f"block-{i:05d}"


@pytest.fixture
def small_chunk_service(conn_str):
    """A client that switches to Put Block / Put Block List above 1 KiB."""
    return BlobServiceClient.from_connection_string(
        conn_str, max_single_put_size=1024, max_block_size=1024
    )


# --- single shot upload / download -----------------------------------------


def test_upload_and_download_small_blob(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"hello azure")
    assert b.download_blob().readall() == b"hello azure"


def test_upload_returns_etag_last_modified_and_md5(container, blob_name):
    data = b"some bytes"
    res = container.get_blob_client(blob_name).upload_blob(data)
    assert res["etag"].startswith('"')
    assert res["last_modified"] is not None
    assert bytes(res["content_md5"]) == hashlib.md5(data).digest()
    assert res["request_server_encrypted"] is True


def test_upload_empty_blob(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"")
    assert b.get_blob_properties().size == 0
    assert b.download_blob().readall() == b""


def test_upload_binary_data_roundtrips_exactly(container, blob_name):
    data = rand_bytes(100_000)
    b = container.get_blob_client(blob_name)
    b.upload_blob(data)
    assert b.download_blob().readall() == data


def test_upload_large_single_shot(container, blob_name):
    data = rand_bytes(3 * 1024 * 1024)
    b = container.get_blob_client(blob_name)
    b.upload_blob(data)
    got = b.download_blob().readall()
    assert len(got) == len(data) and got == data


def test_upload_str_is_utf8_encoded(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob("héllo wörld ✓")
    assert b.download_blob().readall() == "héllo wörld ✓".encode("utf-8")
    assert b.download_blob(encoding="utf-8").readall() == "héllo wörld ✓"


def test_upload_from_stream_with_length(container, blob_name):
    import io

    b = container.get_blob_client(blob_name)
    b.upload_blob(io.BytesIO(b"0123456789"), length=4)
    assert b.download_blob().readall() == b"0123"


def test_upload_default_blob_type_is_block_blob(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x")
    assert b.get_blob_properties().blob_type == BlobType.BLOCKBLOB


def test_overwrite_false_raises_blob_already_exists(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"first")
    with pytest.raises(ResourceExistsError) as ei:
        b.upload_blob(b"second", overwrite=False)
    assert_error(ei, "BlobAlreadyExists", 409)
    assert b.download_blob().readall() == b"first"


def test_overwrite_true_replaces_content_and_changes_etag(container, blob_name):
    b = container.get_blob_client(blob_name)
    r1 = b.upload_blob(b"first")
    r2 = b.upload_blob(b"second, longer", overwrite=True)
    assert r1["etag"] != r2["etag"]
    assert b.download_blob().readall() == b"second, longer"
    assert b.get_blob_properties().size == len(b"second, longer")


def test_overwrite_resets_metadata(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"first", metadata={"k": "v"})
    b.upload_blob(b"second", overwrite=True)
    assert not b.get_blob_properties().metadata


def test_chunked_upload_uses_blocks_and_roundtrips(small_chunk_service, container, blob_name):
    data = rand_bytes(5 * 1024 + 100)  # 6 blocks of up to 1 KiB
    b = small_chunk_service.get_blob_client(container.container_name, blob_name)
    b.upload_blob(data, max_concurrency=1)
    assert b.download_blob().readall() == data
    committed, uncommitted = b.get_block_list("all")
    assert [blk.size for blk in committed] == [1024] * 5 + [100]
    assert uncommitted == []


def test_chunked_upload_parallel_roundtrips(small_chunk_service, container, blob_name):
    data = rand_bytes(20 * 1024)
    b = small_chunk_service.get_blob_client(container.container_name, blob_name)
    b.upload_blob(data, max_concurrency=4)
    assert b.download_blob().readall() == data


def test_chunked_upload_overwrite_false_conflicts(small_chunk_service, container, blob_name):
    b = small_chunk_service.get_blob_client(container.container_name, blob_name)
    b.upload_blob(b"x" * 10)
    with pytest.raises(ResourceExistsError) as ei:
        b.upload_blob(rand_bytes(4096), overwrite=False)
    assert_error(ei, "BlobAlreadyExists", 409)


def test_chunked_download_with_small_chunks(conn_str, container, blob_name):
    data = rand_bytes(10 * 1024)
    container.upload_blob(blob_name, data)
    svc = BlobServiceClient.from_connection_string(conn_str, max_chunk_get_size=1024, max_single_get_size=1024)
    got = svc.get_blob_client(container.container_name, blob_name).download_blob(max_concurrency=2).readall()
    assert got == data


def test_download_chunks_iterator_concatenates_to_content(container, blob_name):
    data = rand_bytes(5000)
    b = container.upload_blob(blob_name, data)
    stream = b.download_blob()
    assert b"".join(stream.chunks()) == data


def test_download_missing_blob_is_not_found(container, blob_name):
    with pytest.raises(ResourceNotFoundError) as ei:
        container.get_blob_client(blob_name).download_blob()
    assert_error(ei, "BlobNotFound", 404)


def test_delete_blob_then_download_is_not_found(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x")
    b.delete_blob()
    with pytest.raises(ResourceNotFoundError) as ei:
        b.download_blob()
    assert_error(ei, "BlobNotFound", 404)


def test_delete_missing_blob_is_not_found(container, blob_name):
    with pytest.raises(ResourceNotFoundError) as ei:
        container.get_blob_client(blob_name).delete_blob()
    assert_error(ei, "BlobNotFound", 404)


# --- range downloads -------------------------------------------------------


@pytest.fixture
def hundred(container, blob_name):
    data = bytes(range(100))
    b = container.get_blob_client(blob_name)
    b.upload_blob(data)
    return b, data


def test_range_download_offset_and_length(hundred):
    b, data = hundred
    assert b.download_blob(offset=10, length=20).readall() == data[10:30]


def test_range_download_offset_only_reads_to_the_end(hundred):
    b, data = hundred
    assert b.download_blob(offset=90).readall() == data[90:]


def test_range_download_single_byte(hundred):
    b, data = hundred
    assert b.download_blob(offset=99, length=1).readall() == data[99:100]


def test_range_download_length_past_end_is_truncated(hundred):
    b, data = hundred
    assert b.download_blob(offset=50, length=1000).readall() == data[50:]


def test_range_download_reports_total_blob_size(hundred):
    b, data = hundred
    d = b.download_blob(offset=10, length=5)
    assert d.size == 5
    assert d.properties.content_range == "bytes 10-14/100"


@pytest.mark.not_on_azurite(reason="Azurite answers a range starting exactly at the blob size with 206 and an empty body")
def test_range_download_start_at_end_is_invalid_range(hundred):
    b, _ = hundred
    with pytest.raises(HttpResponseError) as ei:
        b.download_blob(offset=100, length=10).readall()
    assert_error(ei, "InvalidRange", 416)


def test_range_download_far_past_end_is_invalid_range(hundred):
    b, _ = hundred
    with pytest.raises(HttpResponseError) as ei:
        b.download_blob(offset=5000).readall()
    assert_error(ei, "InvalidRange", 416)


# --- blocks and block lists ------------------------------------------------


def test_stage_and_commit_blocks(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.stage_block(_bid(1), b"AAAA")
    b.stage_block(_bid(2), b"BBBB")
    b.commit_block_list([BlobBlock(_bid(1)), BlobBlock(_bid(2))])
    assert b.download_blob().readall() == b"AAAABBBB"


def test_commit_block_list_returns_etag_and_last_modified(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.stage_block(_bid(1), b"x")
    res = b.commit_block_list([BlobBlock(_bid(1))])
    assert res["etag"].startswith('"')
    assert res["last_modified"] is not None


def test_commit_order_defines_content_order(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.stage_block(_bid(1), b"one-")
    b.stage_block(_bid(2), b"two-")
    b.stage_block(_bid(3), b"three")
    b.commit_block_list([BlobBlock(_bid(3)), BlobBlock(_bid(1)), BlobBlock(_bid(2))])
    assert b.download_blob().readall() == b"three" + b"one-" + b"two-"


def test_same_block_can_be_committed_twice(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.stage_block(_bid(1), b"ab")
    b.commit_block_list([BlobBlock(_bid(1)), BlobBlock(_bid(1)), BlobBlock(_bid(1))])
    assert b.download_blob().readall() == b"ababab"


def test_staged_blocks_do_not_create_the_blob(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.stage_block(_bid(1), b"pending")
    with pytest.raises(ResourceNotFoundError) as ei:
        b.get_blob_properties()
    assert_error(ei, "BlobNotFound", 404)
    assert blob_name not in [x.name for x in container.list_blobs()]


def test_get_block_list_uncommitted_only_blob(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.stage_block(_bid(1), b"12345")
    b.stage_block(_bid(2), b"678")
    committed, uncommitted = b.get_block_list("all")
    assert committed == []
    assert sorted((x.id, x.size) for x in uncommitted) == [(_bid(1), 5), (_bid(2), 3)]


def test_get_block_list_types(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.stage_block(_bid(1), b"1111")
    b.commit_block_list([BlobBlock(_bid(1))])
    b.stage_block(_bid(2), b"22")
    committed, _ = b.get_block_list("committed")
    _, uncommitted = b.get_block_list("uncommitted")
    c_all, u_all = b.get_block_list("all")
    assert [(x.id, x.size) for x in committed] == [(_bid(1), 4)]
    assert [(x.id, x.size) for x in uncommitted] == [(_bid(2), 2)]
    assert [x.id for x in c_all] == [_bid(1)] and [x.id for x in u_all] == [_bid(2)]


def test_staging_a_block_does_not_change_committed_content(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"original")
    etag = b.get_blob_properties().etag
    b.stage_block(_bid(1), b"new stuff")
    assert b.download_blob().readall() == b"original"
    assert b.get_blob_properties().etag == etag


def test_restaging_same_block_id_replaces_uncommitted_data(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.stage_block(_bid(1), b"old")
    b.stage_block(_bid(1), b"newer")
    b.commit_block_list([BlobBlock(_bid(1))])
    assert b.download_blob().readall() == b"newer"


def test_commit_latest_prefers_uncommitted_over_committed(container, blob_name):
    from azure.storage.blob import BlockState

    b = container.get_blob_client(blob_name)
    b.stage_block(_bid(1), b"aa")
    b.stage_block(_bid(2), b"bb")
    b.commit_block_list([BlobBlock(_bid(1)), BlobBlock(_bid(2))])
    b.stage_block(_bid(2), b"CCC")
    b.commit_block_list([BlobBlock(_bid(1), BlockState.Committed), BlobBlock(_bid(2), BlockState.Latest)])
    assert b.download_blob().readall() == b"aaCCC"


def test_sdk_committed_state_is_sent_as_latest(container, blob_name):
    # azure-storage-blob 12.x bug in _commit_block_list_options: it compares
    # `block.state.value == "committed"` but BlockState.COMMITTED.value is
    # "Committed", so every BlobBlock (Committed, Uncommitted or Latest) is sent as
    # <Latest>. Through the SDK, BlockState.COMMITTED therefore picks up a newer
    # uncommitted block. The real <Committed> behaviour is checked with raw HTTP in
    # test_blob_raw_http.py::test_put_block_list_committed_ignores_newer_uncommitted.
    from azure.storage.blob import BlockState

    b = container.get_blob_client(blob_name)
    b.stage_block(_bid(1), b"aa")
    b.commit_block_list([BlobBlock(_bid(1))])
    b.stage_block(_bid(1), b"ZZ")
    b.commit_block_list([BlobBlock(_bid(1), BlockState.COMMITTED)])
    assert b.download_blob().readall() == b"ZZ"


def test_commit_discards_uncommitted_blocks_not_in_list(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.stage_block(_bid(1), b"keep")
    b.stage_block(_bid(2), b"drop")
    b.commit_block_list([BlobBlock(_bid(1))])
    committed, uncommitted = b.get_block_list("all")
    assert [x.id for x in committed] == [_bid(1)]
    assert uncommitted == []


def test_commit_unknown_block_is_invalid_block_list(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.stage_block(_bid(1), b"x")
    with pytest.raises(HttpResponseError) as ei:
        b.commit_block_list([BlobBlock(_bid(1)), BlobBlock(_bid(9))])
    assert_error(ei, "InvalidBlockList", 400)


def test_commit_block_list_replaces_existing_blob_content(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"old content here")
    b.stage_block(_bid(1), b"fresh")
    b.commit_block_list([BlobBlock(_bid(1))])
    assert b.download_blob().readall() == b"fresh"


def test_commit_block_list_with_metadata_and_content_type(container, blob_name):
    from azure.storage.blob import ContentSettings

    b = container.get_blob_client(blob_name)
    b.stage_block(_bid(1), b"{}")
    b.commit_block_list(
        [BlobBlock(_bid(1))],
        content_settings=ContentSettings(content_type="application/json"),
        metadata={"origin": "blocks"},
    )
    p = b.get_blob_properties()
    assert p.content_settings.content_type == "application/json"
    assert p.metadata == {"origin": "blocks"}


def test_get_block_list_on_missing_blob(container, blob_name):
    with pytest.raises(ResourceNotFoundError) as ei:
        container.get_blob_client(blob_name).get_block_list("all")
    assert_error(ei, "BlobNotFound", 404)


def test_stage_block_validate_content(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.stage_block(_bid(1), b"checked", validate_content=True)
    b.commit_block_list([BlobBlock(_bid(1))])
    assert b.download_blob().readall() == b"checked"


def test_upload_with_validate_content(container, blob_name):
    data = rand_bytes(2000)
    b = container.get_blob_client(blob_name)
    b.upload_blob(data, validate_content=True)
    assert b.download_blob(validate_content=True).readall() == data


def test_blob_with_block_ids_of_different_length_is_rejected(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.stage_block("short", b"x")
    with pytest.raises(HttpResponseError) as ei:
        b.stage_block("a-much-longer-block-id", b"y")
    assert_error(ei, "InvalidBlobOrBlock", 400)
