"""Blob snapshots."""

import re
import time

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError

from blobutil import assert_error

SNAPSHOT_RE = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{7}Z$")


def _next_tick():
    """Azurite's snapshot ids have millisecond resolution (Azure's are 100 ns ticks),
    so two snapshots taken back to back can collide there. Wait it out."""
    time.sleep(0.02)


@pytest.fixture
def blob(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"version one", metadata={"v": "1"})
    return b


def _snap_client(service, container, blob, snap):
    return service.get_blob_client(container.container_name, blob.blob_name, snapshot=snap)


def test_create_snapshot_returns_id_etag_last_modified(blob):
    snap = blob.create_snapshot()
    assert SNAPSHOT_RE.match(snap["snapshot"]), snap["snapshot"]
    assert snap["etag"].startswith('"')
    assert snap["last_modified"] is not None


def test_two_snapshots_have_different_ids(blob):
    s1 = blob.create_snapshot()["snapshot"]
    _next_tick()
    s2 = blob.create_snapshot()["snapshot"]
    assert s1 != s2


def test_snapshot_keeps_old_content_after_blob_changes(service, container, blob):
    snap = blob.create_snapshot()["snapshot"]
    blob.upload_blob(b"version two", overwrite=True)
    assert blob.download_blob().readall() == b"version two"
    assert _snap_client(service, container, blob, snap).download_blob().readall() == b"version one"


def test_snapshot_keeps_metadata(service, container, blob):
    snap = blob.create_snapshot()["snapshot"]
    blob.set_blob_metadata({"v": "2"})
    props = _snap_client(service, container, blob, snap).get_blob_properties()
    assert props.metadata == {"v": "1"}
    assert props.snapshot == snap


def test_snapshot_with_new_metadata_overrides_it(service, container, blob):
    snap = blob.create_snapshot(metadata={"v": "snap"})["snapshot"]
    assert _snap_client(service, container, blob, snap).get_blob_properties().metadata == {"v": "snap"}
    assert blob.get_blob_properties().metadata == {"v": "1"}


def test_snapshot_of_missing_blob(container, blob_name):
    with pytest.raises(ResourceNotFoundError) as ei:
        container.get_blob_client(blob_name).create_snapshot()
    assert_error(ei, "BlobNotFound", 404)


def test_list_blobs_hides_snapshots_by_default(blob, container):
    blob.create_snapshot()
    assert len(list(container.list_blobs())) == 1


def test_list_blobs_include_snapshots(blob, container):
    snap = blob.create_snapshot()["snapshot"]
    items = list(container.list_blobs(include=["snapshots"]))
    assert len(items) == 2
    assert sorted(i.snapshot for i in items if i.snapshot) == [snap]
    assert [i.snapshot for i in items if not i.snapshot] == [None]
    assert {i.name for i in items} == {blob.blob_name}


def test_delete_blob_with_snapshots_requires_delete_snapshots_option(blob):
    blob.create_snapshot()
    with pytest.raises(HttpResponseError) as ei:
        blob.delete_blob()
    assert_error(ei, "SnapshotsPresent", 409)
    assert blob.exists()


def test_delete_snapshots_include_removes_blob_and_snapshots(blob, container):
    blob.create_snapshot()
    _next_tick()
    blob.create_snapshot()
    assert len(list(container.list_blobs(include=["snapshots"]))) == 3
    blob.delete_blob(delete_snapshots="include")
    assert list(container.list_blobs(include=["snapshots"])) == []


def test_delete_snapshots_only_keeps_base_blob(blob, container):
    blob.create_snapshot()
    blob.delete_blob(delete_snapshots="only")
    items = list(container.list_blobs(include=["snapshots"]))
    assert len(items) == 1 and items[0].snapshot is None
    assert blob.download_blob().readall() == b"version one"


def test_delete_single_snapshot_keeps_others(service, container, blob):
    s1 = blob.create_snapshot()["snapshot"]
    _next_tick()
    s2 = blob.create_snapshot()["snapshot"]
    _snap_client(service, container, blob, s1).delete_blob()
    assert not _snap_client(service, container, blob, s1).exists()
    assert _snap_client(service, container, blob, s2).exists()
    assert blob.exists()


def test_download_missing_snapshot_is_not_found(service, container, blob):
    c = _snap_client(service, container, blob, "2020-01-01T00:00:00.0000000Z")
    with pytest.raises(ResourceNotFoundError) as ei:
        c.download_blob()
    assert ei.value.status_code == 404


def test_snapshot_exists(service, container, blob):
    snap = blob.create_snapshot()["snapshot"]
    assert _snap_client(service, container, blob, snap).exists() is True


def test_blob_without_snapshots_deletes_normally_with_include(blob):
    blob.delete_blob(delete_snapshots="include")
    assert not blob.exists()


def test_copy_snapshot_to_new_blob_restores_old_content(service, container, blob, uniq):
    snap = blob.create_snapshot()["snapshot"]
    blob.upload_blob(b"version two", overwrite=True)
    dest = container.get_blob_client(uniq("restored"))
    dest.start_copy_from_url(_snap_client(service, container, blob, snap).url)
    assert dest.download_blob().readall() == b"version one"


@pytest.mark.not_on_azurite(reason="Azurite ignores x-ms-previous-snapshot / prevsnapshot and returns all valid ranges")
def test_page_blob_snapshot_diff(service, container, uniq):
    pb = container.get_blob_client(uniq("pb"))
    pb.create_page_blob(size=2048)
    pb.upload_page(b"A" * 512, offset=0, length=512)
    snap = pb.create_snapshot()["snapshot"]
    pb.upload_page(b"B" * 512, offset=1024, length=512)
    changed, cleared = pb.get_page_ranges(previous_snapshot_diff=snap)
    assert changed == [{"start": 1024, "end": 1535}]
    assert cleared == []
