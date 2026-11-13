"""Data Lake Storage Gen2 events (the `dfs` endpoint shares the Blob data): file and directory events."""

import time

import seutil as su


def sub_all(emu, hook):
    return su.subscribe(emu, "all", {"webhookUrl": hook.url})


def by_type(events):
    out = {}
    for e in events:
        out.setdefault(e["eventType"], []).append(e)
    return out


def blob_url_ok(url, fs, path):
    return ".blob." in url and url.endswith(f"/{fs}/{path}")


def dfs_url_ok(url, fs, path):
    return ".dfs." in url and url.endswith(f"/{fs}/{path}")


def test_create_directory_and_file(emu, hook, dl_service):
    fs = dl_service.create_file_system(su.uniq("fs"))
    sub_all(emu, hook)
    fs.create_directory("dir1")
    f = fs.get_file_client("dir1/file.txt")
    f.create_file()
    f.append_data(b"abcdef", 0, 6)
    f.flush_data(6, close=True)
    events = hook.wait_events(3)
    ev = by_type(events)
    (dc,) = ev["Microsoft.Storage.DirectoryCreated"]
    su.check_eg_envelope(dc, "Microsoft.Storage.DirectoryCreated", su.subject_of(fs.file_system_name, "dir1"), "1")
    d = dc["data"]
    assert d["api"] == "CreateDirectory"
    assert dfs_url_ok(d["url"], fs.file_system_name, "dir1") and blob_url_ok(d["blobUrl"], fs.file_system_name, "dir1")
    assert d["eTag"].startswith("0x")
    su.check_sequencer(d)

    created = {e["data"]["api"]: e for e in ev["Microsoft.Storage.BlobCreated"]}
    assert set(created) == {"CreateFile", "FlushWithClose"}
    for e in created.values():
        # Learn (Data Lake Storage Gen2): dataVersion "2", contentOffset present, url on the dfs endpoint
        su.check_eg_envelope(e, "Microsoft.Storage.BlobCreated", su.subject_of(fs.file_system_name, "dir1/file.txt"), "2")
        assert "contentOffset" in e["data"]
        assert dfs_url_ok(e["data"]["url"], fs.file_system_name, "dir1/file.txt")
    assert created["CreateFile"]["data"]["contentLength"] == 0
    assert created["FlushWithClose"]["data"]["contentLength"] == 6
    assert created["FlushWithClose"]["data"]["contentOffset"] == 6


def test_flush_without_close_raises_nothing(emu, hook, dl_service):
    fs = dl_service.create_file_system(su.uniq("fs"))
    f = fs.get_file_client("f.txt")
    f.create_file()
    sub_all(emu, hook)
    f.append_data(b"123", 0, 3)
    f.flush_data(3, close=False)
    time.sleep(0.5)
    assert hook.events == []
    f.flush_data(3, close=True)
    (e,) = hook.wait_events(1)
    assert e["data"]["api"] == "FlushWithClose"


def test_rename_file_and_directory(emu, hook, dl_service):
    fs = dl_service.create_file_system(su.uniq("fs"))
    fs.create_directory("olddir")
    f = fs.get_file_client("olddir/a.txt")
    f.create_file()
    g = fs.get_file_client("b.txt")
    g.create_file()
    sub_all(emu, hook)
    g.rename_file(f"{fs.file_system_name}/c.txt")
    fs.get_directory_client("olddir").rename_directory(f"{fs.file_system_name}/newdir")
    events = hook.wait_events(2)
    ev = by_type(events)
    (fr,) = ev["Microsoft.Storage.BlobRenamed"]
    su.check_eg_envelope(fr, "Microsoft.Storage.BlobRenamed", su.subject_of(fs.file_system_name, "c.txt"), "1")
    d = fr["data"]
    assert d["api"] == "RenameFile"
    assert dfs_url_ok(d["sourceUrl"], fs.file_system_name, "b.txt") and dfs_url_ok(d["destinationUrl"], fs.file_system_name, "c.txt")
    assert blob_url_ok(d["sourceBlobUrl"], fs.file_system_name, "b.txt") and blob_url_ok(d["destinationBlobUrl"], fs.file_system_name, "c.txt")
    su.check_sequencer(d)
    (dr,) = ev["Microsoft.Storage.DirectoryRenamed"]
    su.check_eg_envelope(dr, "Microsoft.Storage.DirectoryRenamed", su.subject_of(fs.file_system_name, "newdir"), "1")
    assert dr["data"]["api"] == "RenameDirectory"
    assert dfs_url_ok(dr["data"]["sourceUrl"], fs.file_system_name, "olddir")


def test_delete_file_and_directory(emu, hook, dl_service):
    fs = dl_service.create_file_system(su.uniq("fs"))
    fs.create_directory("dd")
    fs.get_file_client("dd/x.txt").create_file()
    fs.get_file_client("top.txt").create_file()
    sub_all(emu, hook)
    fs.get_file_client("top.txt").delete_file()
    fs.get_directory_client("dd").delete_directory()  # the SDK deletes recursively
    events = hook.wait_events(2)
    ev = by_type(events)
    (fd,) = ev["Microsoft.Storage.BlobDeleted"]
    su.check_eg_envelope(fd, "Microsoft.Storage.BlobDeleted", su.subject_of(fs.file_system_name, "top.txt"), "2")
    assert fd["data"]["api"] == "DeleteFile"
    assert dfs_url_ok(fd["data"]["url"], fs.file_system_name, "top.txt")
    assert {"contentType", "blobType"} <= set(fd["data"])
    (dd,) = ev["Microsoft.Storage.DirectoryDeleted"]
    su.check_eg_envelope(dd, "Microsoft.Storage.DirectoryDeleted", su.subject_of(fs.file_system_name, "dd"), "1")
    assert dd["data"]["api"] == "DeleteDirectory"
    assert dd["data"]["recursive"] == "true"  # a string, as in the schema
    assert dfs_url_ok(dd["data"]["url"], fs.file_system_name, "dd")


def test_blob_endpoint_in_a_data_lake_container_uses_blob_events(emu, hook, dl_service, blob_service):
    fs = dl_service.create_file_system(su.uniq("fs"))
    sub_all(emu, hook)
    blob_service.get_container_client(fs.file_system_name).upload_blob("viablob.txt", b"1")
    (e,) = hook.wait_events(1)
    assert e["data"]["api"] == "PutBlob" and e["dataVersion"] == ""
