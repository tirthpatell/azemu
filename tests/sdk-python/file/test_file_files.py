"""Files: create, content by ranges, download, ranges, properties, metadata, leases, copy, rename, delete.

Written from the REST reference and azure-storage-file-share 12.27.1; the answers that were checked against Azure in November
2026 say so.
"""

import hashlib
from datetime import datetime, timezone

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.storage.fileshare import ContentSettings, NTFSAttributes, generate_file_sas, FileSasPermissions

import fileutil
from fileutil import expect


def test_create_file_is_all_zeros(work):
    f = work.get_file_client("z.bin")
    f.create_file(1000)
    props = f.get_file_properties()
    assert props.size == 1000
    assert props.content_settings.content_type == "application/octet-stream"
    assert props.file_attributes == "Archive"
    assert f.download_file().readall() == bytes(1000)
    assert list(f.get_ranges()) == []


def test_upload_file_and_download(work):
    f = work.get_file_client("hello.txt")
    f.upload_file(b"hello world", content_settings=ContentSettings(content_type="text/plain"))
    assert f.download_file().readall() == b"hello world"
    assert f.get_file_properties().content_settings.content_type == "text/plain"
    with pytest.raises(Exception):
        f.get_file_properties().content_settings.content_md5.hex()  # no MD5 unless the client gave one
    # the file takes the new size when it is uploaded again
    f.upload_file(b"hi")
    assert f.download_file().readall() == b"hi" and f.get_file_properties().size == 2


def test_download_a_range(work):
    f = work.get_file_client("r.txt")
    f.upload_file(b"0123456789")
    assert f.download_file(offset=2, length=3).readall() == b"234"
    assert f.download_file(offset=7).readall() == b"789"


def test_download_range_md5(work):
    f = work.get_file_client("m.txt")
    f.upload_file(b"0123456789")
    d = f.download_file(offset=0, length=4, validate_content=True)
    assert d.readall() == b"0123"


def test_range_past_the_end_is_416(work):
    f = work.get_file_client("e.txt")
    f.upload_file(b"abc")
    with pytest.raises(HttpResponseError) as e:
        f.download_file(offset=10, length=5).readall()
    expect(e, 416, "InvalidRange")


def test_upload_range_and_clear_range(work):
    f = work.get_file_client("sparse.bin")
    f.create_file(4096)
    f.upload_range(b"A" * 100, offset=1024, length=100)
    data = f.download_file().readall()
    assert data[1024:1124] == b"A" * 100 and data[:1024] == bytes(1024) and len(data) == 4096
    # Azure reports whole 512 byte pages that hold data (checked in November 2026)
    assert [(r["start"], r["end"]) for r in f.get_ranges()] == [(1024, 1535)]
    f.clear_range(offset=1024, length=512)
    assert f.download_file(offset=1024, length=100).readall() == bytes(100)
    assert list(f.get_ranges()) == []


def test_range_must_fit_in_the_file(work):
    f = work.get_file_client("small.bin")
    f.create_file(100)
    with pytest.raises(HttpResponseError) as e:
        f.upload_range(b"x" * 10, offset=95, length=10)
    expect(e, 416, "InvalidRange")


def test_get_ranges_diff_against_a_snapshot(share):
    f = share.get_file_client("diff.bin")
    f.create_file(2048)
    f.upload_range(b"a" * 512, offset=0, length=512)
    snap = share.create_snapshot()["snapshot"]
    f.upload_range(b"b" * 512, offset=1024, length=512)
    f.clear_range(offset=0, length=512)
    changed, cleared = f.get_ranges_diff(previous_sharesnapshot=snap)
    assert [(r["start"], r["end"]) for r in changed] == [(1024, 1535)]
    assert [(r["start"], r["end"]) for r in cleared] == [(0, 511)]


def test_resize_and_set_http_headers(work):
    f = work.get_file_client("p.bin")
    f.upload_file(b"x" * 100)
    f.set_http_headers(ContentSettings(content_type="text/x-test", content_language="en", content_disposition="attachment", cache_control="no-cache", content_encoding="identity"), file_attributes="preserve", size=50)
    props = f.get_file_properties()
    assert props.size == 50
    cs = props.content_settings
    assert (cs.content_type, cs.content_language, cs.content_disposition, cs.cache_control, cs.content_encoding) == (
        "text/x-test", "en", "attachment", "no-cache", "identity")
    assert f.download_file().readall() == b"x" * 50
    f.resize_file(120)
    assert f.download_file().readall() == b"x" * 50 + bytes(70)


def test_set_http_headers_clears_what_it_does_not_send(work):
    f = work.get_file_client("c.bin")
    f.create_file(10, content_settings=ContentSettings(content_type="text/plain", content_language="fr"))
    f.set_http_headers(ContentSettings(content_type="application/json"), file_attributes="preserve")
    cs = f.get_file_properties().content_settings
    assert cs.content_type == "application/json" and cs.content_language is None


def test_file_metadata(work):
    f = work.get_file_client("md.txt")
    f.create_file(1, metadata={"a": "1"})
    assert f.get_file_properties().metadata == {"a": "1"}
    f.set_file_metadata({"b": "2"})
    assert f.get_file_properties().metadata == {"b": "2"}


def test_file_attributes_and_times(work):
    created = datetime(2020, 1, 2, 3, 4, 5, tzinfo=timezone.utc)
    f = work.get_file_client("smb.txt")
    f.create_file(5, file_attributes=NTFSAttributes(read_only=True, archive=True), file_creation_time=created, file_last_write_time=created)
    props = f.get_file_properties()
    assert props.file_attributes == "ReadOnly | Archive"
    assert fileutil.as_utc(props.creation_time) == created and fileutil.as_utc(props.last_write_time) == created
    assert props.permission_key and props.file_id and props.parent_id


def test_create_over_a_file_replaces_it(work):
    f = work.get_file_client("over.txt")
    f.upload_file(b"first, long")
    first = f.get_file_properties()
    f.create_file(3)
    second = f.get_file_properties()
    assert second.size == 3 and second.etag != first.etag
    assert f.download_file().readall() == bytes(3)
    assert second.file_id == first.file_id


def test_missing_file_and_missing_parent(work):
    with pytest.raises(ResourceNotFoundError) as e:
        work.get_file_client("nope.txt").get_file_properties()
    expect(e, 404, "ResourceNotFound")
    with pytest.raises(ResourceNotFoundError) as e:
        work.get_subdirectory_client("nodir").get_file_client("f.txt").get_file_properties()
    expect(e, 404, "ParentNotFound")
    with pytest.raises(ResourceNotFoundError) as e:
        work.get_subdirectory_client("nodir").get_file_client("f.txt").create_file(1)
    expect(e, 404, "ParentNotFound")


def test_delete_file(work):
    f = work.get_file_client("gone.txt")
    f.create_file(1)
    f.delete_file()
    with pytest.raises(ResourceNotFoundError) as e:
        f.delete_file()
    expect(e, 404, "ResourceNotFound")


def test_file_size_limit(work):
    with pytest.raises(HttpResponseError) as e:
        work.get_file_client("huge.bin").create_file(4 * 1024**4 + 1)
    expect(e, 400, "OutOfRangeInput")


def test_a_directory_is_not_a_file(work):
    work.create_subdirectory("dd")
    with pytest.raises(HttpResponseError) as e:
        work.get_file_client("dd").create_file(1)
    expect(e, 409, "ResourceTypeMismatch")
    with pytest.raises(ResourceNotFoundError) as e:
        work.get_file_client("dd").get_file_properties()
    expect(e, 404, "ResourceNotFound")


def test_file_lease(work):
    f = work.get_file_client("leased.txt")
    f.create_file(10)
    lease = f.acquire_lease(lease_id="11111111-1111-1111-1111-111111111111")
    props = f.get_file_properties()
    assert (props.lease.state, props.lease.status, props.lease.duration) == ("leased", "locked", "infinite")
    with pytest.raises(HttpResponseError) as e:
        f.upload_range(b"x", offset=0, length=1)
    expect(e, 412, "LeaseIdMissing")
    with pytest.raises(HttpResponseError) as e:
        f.set_file_metadata({"a": "b"}, lease="22222222-2222-2222-2222-222222222222")
    expect(e, 412, "LeaseIdMismatchWithFileOperation")
    f.upload_range(b"x", offset=0, length=1, lease=lease)
    assert f.download_file().readall()[:1] == b"x"  # a read needs no lease ID
    with pytest.raises(HttpResponseError) as e:
        f.delete_file()
    expect(e, 412, "LeaseIdMissing")
    lease.release()
    assert f.get_file_properties().lease.state == "available"
    f.delete_file()


def test_file_lease_break_is_immediate(work):
    f = work.get_file_client("br.txt")
    f.create_file(10)
    lease = f.acquire_lease()
    lease.break_lease()
    assert f.get_file_properties().lease.state == "broken"
    f.upload_range(b"x", offset=0, length=1)


def test_file_lease_without_a_lease_id_on_the_file(work):
    f = work.get_file_client("nolease.txt")
    f.create_file(10)
    with pytest.raises(HttpResponseError) as e:
        f.set_file_metadata({"a": "b"}, lease="22222222-2222-2222-2222-222222222222")
    expect(e, 412, "LeaseNotPresentWithFileOperation")


def test_copy_file_within_a_share(shared, work, account, conn_str, emu):
    src = work.get_file_client("src.bin")
    src.upload_file(b"copy me" * 100, content_settings=ContentSettings(content_type="text/x-src"), metadata={"m": "1"})
    dst = work.get_file_client("dst.bin")
    res = dst.start_copy_from_url(_source_url(emu, shared, src))
    assert res["copy_status"] == "success" and res["copy_id"]
    props = dst.get_file_properties()
    assert dst.download_file().readall() == b"copy me" * 100
    assert props.copy.status == "success" and props.copy.id == res["copy_id"]
    assert props.content_settings.content_type == "text/x-src"
    assert props.metadata == {"m": "1"}


def test_copy_from_a_missing_file(shared, work, emu):
    src = work.get_file_client("nosrc.bin")
    with pytest.raises(HttpResponseError) as e:
        work.get_file_client("d.bin").start_copy_from_url(_source_url(emu, shared, src))
    assert e.value.status_code == 404


def test_abort_copy_when_nothing_is_pending(shared, work, emu):
    src = work.get_file_client("s.bin")
    src.upload_file(b"x")
    dst = work.get_file_client("d.bin")
    res = dst.start_copy_from_url(_source_url(emu, shared, src))
    with pytest.raises(HttpResponseError) as e:
        dst.abort_copy(res["copy_id"])
    expect(e, 409, "NoPendingCopyOperation")


def test_upload_range_from_url(shared, work, emu):
    src = work.get_file_client("from.bin")
    src.upload_file(b"0123456789")
    dst = work.get_file_client("to.bin")
    dst.create_file(20)
    dst.upload_range_from_url(_source_url(emu, shared, src), offset=5, length=4, source_offset=2)
    assert dst.download_file(offset=5, length=4).readall() == b"2345"


def test_copy_gets_the_archive_attribute_and_new_times(shared, work, emu):
    # checked against Azure in November 2026: a copy takes the attributes asked for plus Archive, and times of now
    created = datetime(2020, 1, 2, 3, 4, 5, tzinfo=timezone.utc)
    src = work.get_file_client("cs.txt")
    src.create_file(5, file_attributes=NTFSAttributes(read_only=True, archive=True), file_creation_time=created, file_last_write_time=created)
    dst = work.get_file_client("cd.txt")
    dst.start_copy_from_url(_source_url(emu, shared, src))
    props = dst.get_file_properties()
    assert props.file_attributes == "Archive"
    assert fileutil.as_utc(props.creation_time) > created
    other = work.get_file_client("cd2.txt")
    other.start_copy_from_url(_source_url(emu, shared, src), file_attributes="Hidden", file_creation_time=created, metadata={"m": "dst"})
    props = other.get_file_properties()
    assert props.file_attributes == "Hidden | Archive" and fileutil.as_utc(props.creation_time) == created and props.metadata == {"m": "dst"}


def test_copy_over_a_read_only_file(shared, work, emu):
    src = work.get_file_client("s.txt")
    src.upload_file(b"x")
    dst = work.get_file_client("d.txt")
    dst.create_file(1, file_attributes="ReadOnly")
    with pytest.raises(HttpResponseError) as e:
        dst.start_copy_from_url(_source_url(emu, shared, src))
    expect(e, 409, "ReadOnlyAttribute")
    dst.start_copy_from_url(_source_url(emu, shared, src), ignore_read_only=True)
    assert dst.download_file().readall() == b"x"


def test_file_attributes_none(work):
    # checked against Azure in November 2026: None on create is Archive, on Set Properties it stays None; None cannot be combined
    f = work.get_file_client("a.txt")
    f.create_file(1, file_attributes="None")
    assert f.get_file_properties().file_attributes == "Archive"
    f.set_http_headers(ContentSettings(), file_attributes="None")
    assert f.get_file_properties().file_attributes == "None"
    with pytest.raises(HttpResponseError) as e:
        f.set_http_headers(ContentSettings(), file_attributes="Hidden|None")
    expect(e, 400, "InvalidHeaderValue")
    with pytest.raises(HttpResponseError) as e:
        f.set_http_headers(ContentSettings(), file_attributes="Directory")
    expect(e, 400, "InvalidHeaderValue")


def test_now_is_not_a_time_for_set_properties(work):
    f = work.get_file_client("n.txt")
    f.create_file(1, file_creation_time="now", file_last_write_time="now")
    with pytest.raises(HttpResponseError) as e:
        f.set_http_headers(ContentSettings(), file_creation_time="now")
    expect(e, 400, "InvalidHeaderValue")


def test_a_range_of_a_small_file_ends_at_its_size(work):
    f = work.get_file_client("small.txt")
    f.create_file(11)
    f.upload_range(b"hello", offset=0, length=5)
    assert [(r["start"], r["end"]) for r in f.get_ranges()] == [(0, 10)]


def test_rename_ignores_restype_and_follows_the_source(work):
    # checked against Azure in November 2026: Rename File and Rename Directory are one operation; the source decides
    f = work.get_file_client("f.txt")
    f.upload_file(b"x")
    d = work.create_subdirectory("d")
    moved = f.rename_file(f"{work.directory_path}/moved.txt")
    assert moved.download_file().readall() == b"x"
    with pytest.raises(HttpResponseError) as e:
        d.rename_directory(f"{work.directory_path}/moved.txt")  # a directory over a file needs overwrite
    expect(e, 409, "ResourceAlreadyExists")
    d.rename_directory(f"{work.directory_path}/moved.txt", overwrite=True)
    assert [i.name for i in work.list_directories_and_files()] == ["moved.txt"]
    assert next(iter(work.list_directories_and_files())).is_directory


def test_rename_refusals(work):
    a = work.create_subdirectory("a")
    b = work.create_subdirectory("b")
    with pytest.raises(HttpResponseError) as e:
        a.rename_directory(f"{work.directory_path}/a/inside")
    expect(e, 409, "RenameCycle")
    with pytest.raises(HttpResponseError) as e:
        a.rename_directory(f"{work.directory_path}/b", overwrite=True)
    expect(e, 400, "RenameDirectoryOperationNotSupported")
    with pytest.raises(ResourceNotFoundError) as e:
        work.get_file_client("nope.txt").rename_file(f"{work.directory_path}/x.txt")
    expect(e, 404, "ResourceNotFound")
    with pytest.raises(ResourceNotFoundError) as e:
        work.get_file_client("nope.txt").rename_file(f"{work.directory_path}/missing/x.txt")
    assert e.value.status_code == 404
    b.rename_directory(f"{work.directory_path}/b")  # onto itself


def test_rename_file(work):
    f = work.get_file_client("before.txt")
    f.upload_file(b"payload")
    moved = f.rename_file(f"{work.directory_path}/after.txt")
    assert moved.file_name == "after.txt"
    assert moved.download_file().readall() == b"payload"
    with pytest.raises(ResourceNotFoundError):
        f.get_file_properties()


def test_rename_file_overwrite(work):
    a = work.get_file_client("a.txt")
    a.upload_file(b"A")
    b = work.get_file_client("b.txt")
    b.upload_file(b"B")
    with pytest.raises(HttpResponseError) as e:
        a.rename_file(f"{work.directory_path}/b.txt")
    expect(e, 409, "ResourceAlreadyExists")
    moved = a.rename_file(f"{work.directory_path}/b.txt", overwrite=True)
    assert moved.download_file().readall() == b"A"


def test_upload_a_file_over_the_range_limit(work):
    f = work.get_file_client("big.bin")
    data = b"z" * (5 * 1024 * 1024)
    f.upload_file(data)  # the SDK sends it in ranges of 4 MiB
    assert f.get_file_properties().size == len(data)
    assert f.download_file(offset=4 * 1024 * 1024, length=10).readall() == b"z" * 10


def _source_url(emu, share, file_client):
    """The URL of a file as a copy source: it needs a SAS on Azure (the source is checked there), the emulator takes the plain URL."""
    sas = generate_file_sas(
        account_name=file_client.account_name, share_name=file_client.share_name, file_path=file_client.file_path,
        account_key=fileutil.account_key(emu), permission=FileSasPermissions(read=True),
        expiry=datetime.now(timezone.utc).replace(year=datetime.now().year + 1))
    return f"{file_client.url}?{sas}"
