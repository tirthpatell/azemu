"""File content: append / flush / upload_data / download_file."""

import io

import pytest
from azure.core import MatchConditions
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.storage.filedatalake import ContentSettings, DataLakeFileClient

from dlutil import DEV_ACCOUNT, DEV_KEY, assert_error, assert_status, put_file, rand_bytes, read_file


# -- append / flush ----------------------------------------------------------
def test_append_returns_response_headers(fs):
    f = fs.create_file("f")
    resp = f.append_data(b"abc", 0, 3)
    assert resp is not None


def test_append_then_flush_makes_data_visible(fs):
    f = fs.create_file("f")
    f.append_data(b"abc", 0, 3)
    resp = f.flush_data(3)
    assert resp["etag"]
    assert resp["last_modified"] is not None
    assert f.get_file_properties().size == 3
    assert read_file(f) == b"abc"


@pytest.mark.docs
def test_appended_data_is_not_visible_before_flush(fs):
    f = fs.create_file("f")
    f.append_data(b"abcdef", 0, 6)
    assert f.get_file_properties().size == 0
    assert read_file(f) == b""
    f.flush_data(6)
    assert f.get_file_properties().size == 6


def test_several_appends_then_one_flush(fs):
    f = fs.create_file("f")
    f.append_data(b"aaa", 0, 3)
    f.append_data(b"bbbb", 3, 4)
    f.append_data(b"cc", 7, 2)
    f.flush_data(9)
    assert read_file(f) == b"aaabbbbcc"
    assert f.get_file_properties().size == 9


def test_append_data_with_flush_true(fs):
    f = fs.create_file("f")
    resp = f.append_data(b"abc", 0, 3, flush=True)
    assert resp is not None
    assert f.get_file_properties().size == 3
    assert read_file(f) == b"abc"


def test_append_without_length_argument(fs):
    f = fs.create_file("f")
    f.append_data(b"hello", 0)
    f.flush_data(5)
    assert read_file(f) == b"hello"


def test_append_in_two_flushed_rounds(fs):
    f = fs.create_file("f")
    f.append_data(b"abc", 0, 3)
    f.flush_data(3)
    f.append_data(b"def", 3, 3)
    f.flush_data(6)
    assert read_file(f) == b"abcdef"


def test_flush_empty_file(fs):
    """Recorded: flush position=0 on a fresh file -> 200, size 0."""
    f = fs.create_file("f")
    f.flush_data(0)
    assert f.get_file_properties().size == 0


def test_flush_changes_etag_and_last_modified(fs):
    f = fs.create_file("f")
    before = f.get_file_properties()
    f.append_data(b"abc", 0, 3)
    resp = f.flush_data(3)
    after = f.get_file_properties()
    assert after.etag != before.etag
    assert after.last_modified >= before.last_modified
    assert after.etag.strip('"') == resp["etag"].strip('"')


def test_flush_with_close_true(fs):
    f = fs.create_file("f")
    f.append_data(b"abc", 0, 3)
    f.flush_data(3, close=True)
    assert read_file(f) == b"abc"


def test_append_after_flush_requires_new_flush_to_extend(fs):
    f = put_file(fs, "f", b"abc")
    f.append_data(b"def", 3, 3)
    assert f.get_file_properties().size == 3
    f.flush_data(6)
    assert read_file(f) == b"abcdef"


@pytest.mark.docs
def test_flush_with_content_settings_sets_headers(fs):
    f = fs.create_file("f")
    f.append_data(b"{}", 0, 2)
    f.flush_data(2, content_settings=ContentSettings(content_type="application/json"))
    assert f.get_file_properties().content_settings.content_type == "application/json"


@pytest.mark.docs
def test_flush_wrong_position_is_invalid_flush_position(fs):
    f = fs.create_file("f")
    f.append_data(b"abc", 0, 3)
    with pytest.raises(HttpResponseError) as e:
        f.flush_data(10)
    assert_error(e, "InvalidFlushPosition", 400)
    assert f.get_file_properties().size == 0


@pytest.mark.docs
def test_flush_position_shorter_than_appended_is_invalid(fs):
    f = fs.create_file("f")
    f.append_data(b"abcdef", 0, 6)
    with pytest.raises(HttpResponseError) as e:
        f.flush_data(3)
    assert_error(e, "InvalidFlushPosition", 400)


@pytest.mark.docs
def test_flush_nonzero_position_on_file_with_nothing_appended_is_invalid(fs):
    f = fs.create_file("f")
    with pytest.raises(HttpResponseError) as e:
        f.flush_data(5)
    assert_error(e, "InvalidFlushPosition", 400)


@pytest.mark.docs
def test_append_to_missing_file_is_path_not_found(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_file_client("nofile").append_data(b"abc", 0, 3)
    assert_error(e, "PathNotFound", 404)


@pytest.mark.docs
def test_flush_missing_file_is_path_not_found(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_file_client("nofile").flush_data(0)
    assert_error(e, "PathNotFound", 404)


@pytest.mark.docs
def test_appends_arriving_out_of_order_are_assembled_by_position(fs):
    """append supports parallel uploads: data is placed by `position`, flush commits the whole range."""
    f = fs.create_file("f")
    f.append_data(b"world", 5, 5)
    f.append_data(b"hello", 0, 5)
    f.flush_data(10)
    assert read_file(f) == b"helloworld"


# -- upload_data -------------------------------------------------------------
def test_upload_data_small_to_new_file(fs):
    f = fs.get_file_client("d/f")
    data = rand_bytes(1000)
    f.upload_data(data, overwrite=True)
    assert read_file(f) == data
    assert f.get_file_properties().size == 1000


def test_upload_data_returns_etag_and_last_modified(fs):
    f = fs.get_file_client("f")
    resp = f.upload_data(b"abc", overwrite=True)
    assert resp["etag"]
    assert resp["last_modified"] is not None


def test_upload_data_200k_with_concurrency(fs):
    f = fs.get_file_client("f")
    data = rand_bytes(200 * 1024)
    f.upload_data(data, overwrite=True, max_concurrency=3)
    assert read_file(f) == data


@pytest.mark.parametrize("concurrency", [1, 3])
def test_upload_data_in_many_chunks(fs, concurrency):
    """chunk_size=1024 -> 8 appends at consecutive offsets then one flush (recorded request pattern)."""
    f = fs.get_file_client("f")
    data = rand_bytes(8 * 1024 + 100)
    f.upload_data(data, overwrite=True, chunk_size=1024, max_concurrency=concurrency)
    assert f.get_file_properties().size == len(data)
    assert read_file(f) == data


def test_upload_data_progress_hook(fs):
    f = fs.get_file_client("f")
    data = rand_bytes(8 * 1024)
    seen = []
    f.upload_data(
        data, overwrite=True, progress_hook=lambda cur, total: seen.append((cur, total)), max_concurrency=1, chunk_size=1024
    )
    assert seen, "progress hook never called"
    assert seen[-1][0] == len(data)
    assert [c for c, _ in seen] == sorted(c for c, _ in seen)
    assert all(t == len(data) for _, t in seen)


def test_upload_data_to_existing_file_without_overwrite_fails(fs):
    f = put_file(fs, "f", b"abc")
    with pytest.raises(HttpResponseError) as e:
        f.upload_data(rand_bytes(100))
    assert_error(e, "ConditionNotMet", 412)  # recorded: flush with If-None-Match: * -> 412
    assert read_file(f) == b"abc"


def test_upload_data_to_existing_file_with_overwrite_replaces(fs):
    f = put_file(fs, "f", b"abc")
    data = rand_bytes(100)
    f.upload_data(data, overwrite=True)
    assert read_file(f) == data
    assert f.get_file_properties().size == 100


def test_upload_data_overwrite_with_shorter_data_truncates(fs):
    f = put_file(fs, "f", b"0123456789")
    f.upload_data(b"ab", overwrite=True)
    assert read_file(f) == b"ab"


def test_upload_data_with_content_settings_and_metadata(fs):
    f = fs.get_file_client("f")
    f.upload_data(
        b"hello",
        overwrite=True,
        content_settings=ContentSettings(content_type="text/plain", content_language="spanish"),
        metadata={"hello": "world"},
    )
    props = f.get_file_properties()
    assert props.content_settings.content_type == "text/plain"
    assert props.content_settings.content_language == "spanish"
    assert props.metadata == {"hello": "world"}


def test_upload_data_with_permissions_and_umask(fs):
    """Recorded: overwrite + permissions 0777 + umask 0000 + etag/IfNotModified -> rwxrwxrwx."""
    f = fs.get_file_client("f")
    etag = f.create_file()["etag"]
    data = rand_bytes(100)
    f.upload_data(
        data, overwrite=True, permissions="0777", umask="0000", etag=etag, match_condition=MatchConditions.IfNotModified
    )
    assert read_file(f) == data
    assert f.get_access_control()["permissions"] == "rwxrwxrwx"


def test_upload_data_from_stream(fs):
    f = fs.get_file_client("f")
    data = rand_bytes(5000)
    f.upload_data(io.BytesIO(data), overwrite=True, length=len(data))
    assert read_file(f) == data


def test_upload_data_str_is_utf8_encoded(fs):
    f = fs.get_file_client("f")
    f.upload_data("café", overwrite=True)
    assert read_file(f) == "café".encode("utf-8")


def test_upload_data_creates_missing_parents(fs):
    fs.get_file_client("a/b/c").upload_data(b"x", overwrite=True)
    assert fs.get_directory_client("a/b").exists()


# -- download_file -----------------------------------------------------------
def test_download_whole_file(fs):
    data = rand_bytes(4000)
    f = put_file(fs, "f", data)
    dl = f.download_file()
    assert dl.readall() == data


def test_download_properties_and_size(fs):
    f = put_file(fs, "f", b"hello")
    dl = f.download_file()
    assert dl.size == 5
    assert dl.properties.name == "f"
    assert dl.properties.size == 5
    assert dl.properties.etag


def test_download_with_offset_and_length(fs):
    f = put_file(fs, "f", b"0123456789")
    assert f.download_file(offset=2, length=5).readall() == b"23456"


def test_download_with_offset_only(fs):
    f = put_file(fs, "f", b"0123456789")
    assert f.download_file(offset=7).readall() == b"789"


def test_download_length_past_end_is_truncated(fs):
    f = put_file(fs, "f", b"0123456789")
    assert f.download_file(offset=8, length=100).readall() == b"89"


def test_download_offset_past_end_raises_invalid_range(fs):
    f = put_file(fs, "f", b"0123456789")
    with pytest.raises(HttpResponseError) as e:
        f.download_file(offset=100, length=10).readall()
    assert_error(e, "InvalidRange", 416)


def test_download_into_stream(fs):
    data = rand_bytes(1024)
    f = put_file(fs, "f", data)
    out = io.BytesIO()
    n = f.download_file().readinto(out)
    assert n == len(data)
    assert out.getvalue() == data


def test_download_multiple_range_requests(emu, fs):
    """Small max_single_get_size / max_chunk_get_size force several ranged GETs."""
    data = b"12345" * 205 * 5  # 5125 bytes
    put_file(fs, "f", data)
    client = DataLakeFileClient(
        emu.url("dfs", DEV_ACCOUNT),
        fs.file_system_name,
        "f",
        credential=DEV_KEY,
        max_single_get_size=1024,
        max_chunk_get_size=1024,
    )
    stream = client.download_file()
    out = bytearray()
    while True:
        chunk = stream.read(512)
        if not chunk:
            break
        out.extend(chunk)
    assert bytes(out) == data


def test_download_with_max_concurrency(emu, fs):
    data = rand_bytes(6000)
    put_file(fs, "f", data)
    client = DataLakeFileClient(
        emu.url("dfs", DEV_ACCOUNT),
        fs.file_system_name,
        "f",
        credential=DEV_KEY,
        max_single_get_size=1024,
        max_chunk_get_size=1024,
    )
    assert client.download_file(max_concurrency=3).readall() == data


def test_download_text_via_encoding(fs):
    f = put_file(fs, "f", "café デ".encode("utf-8"))
    assert f.download_file(encoding="utf-8").readall() == "café デ"


def test_download_missing_file_raises_not_found(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_file_client("nofile").download_file()
    assert_error(e, "BlobNotFound", 404)


def test_download_etag_matches_properties(fs):
    f = put_file(fs, "f", b"abc")
    assert f.download_file().properties.etag == f.get_file_properties().etag


def test_large_file_1mb_round_trip(fs):
    data = rand_bytes(1024 * 1024)
    f = fs.get_file_client("big")
    f.upload_data(data, overwrite=True, chunk_size=256 * 1024, max_concurrency=2)
    assert f.get_file_properties().size == len(data)
    assert read_file(f) == data
