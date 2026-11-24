"""Shares: create, properties, quota, access tier, metadata, snapshots, stored access policies, statistics, leases, listing, names.

Written from the REST reference (learn.microsoft.com, "File service REST API") and azure-storage-file-share 12.27.1; the answers
that were checked against Azure in November 2026 say so.
"""

from datetime import datetime, timedelta, timezone

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.storage.fileshare import AccessPolicy, ShareSasPermissions

import fileutil
from fileutil import expect


def test_create_share_has_defaults(service, share_name):
    client = service.create_share(share_name)
    try:
        props = client.get_share_properties()
        assert props.name == share_name
        assert props.quota == 102400  # a new share may grow to 100 TiB
        assert props.access_tier == "TransactionOptimized"
        assert props.metadata == {}
        assert props.etag.startswith('"0x') and props.last_modified
        assert props.protocols == ["SMB"]
        assert props.lease.state == "available" and props.lease.status == "unlocked"
    finally:
        fileutil.delete_share(client)


def test_create_share_with_quota_tier_and_metadata(service, share_name):
    client = service.create_share(share_name, quota=7, access_tier="Hot", metadata={"owner": "me"})
    try:
        props = client.get_share_properties()
        assert (props.quota, props.access_tier, props.metadata) == (7, "Hot", {"owner": "me"})
    finally:
        fileutil.delete_share(client)


def test_create_share_twice_is_a_conflict(service, share):
    with pytest.raises(ResourceExistsError) as e:
        service.create_share(share.share_name)
    expect(e, 409, "ShareAlreadyExists")


def test_missing_share_is_404(service, share_name):
    with pytest.raises(ResourceNotFoundError) as e:
        service.get_share_client(share_name).get_share_properties()
    expect(e, 404, "ShareNotFound")


@pytest.mark.parametrize("name,status,code", [
    ("ab", 400, "OutOfRangeInput"),
    ("a" * 64, 400, "OutOfRangeInput"),
    ("Upper", 400, "InvalidResourceName"),
    ("two--hyphens", 400, "InvalidResourceName"),
    ("-leading", 400, "InvalidResourceName"),
    ("trailing-", 400, "InvalidResourceName"),
    ("under_score", 400, "InvalidResourceName"),
    ("dot.ted", 400, "InvalidResourceName"),
])
def test_share_names(service, name, status, code):
    # checked against Azure in November 2026: a name of the wrong length is OutOfRangeInput, bad characters InvalidResourceName
    with pytest.raises(HttpResponseError) as e:
        service.create_share(name)
    expect(e, status, code)


def test_quota_range(service, share):
    share.set_share_quota(5)
    assert share.get_share_properties().quota == 5
    for bad in (0, 102401):
        with pytest.raises(HttpResponseError) as e:
            share.set_share_quota(bad)
        expect(e, 400, "InvalidHeaderValue")


def test_premium_tier_and_nfs_are_for_premium_accounts(service, share_name):
    # checked against Azure in November 2026, on a standard account
    with pytest.raises(HttpResponseError) as e:
        service.create_share(share_name, access_tier="Premium")
    expect(e, 400, "InvalidHeaderValue")
    with pytest.raises(HttpResponseError) as e:
        service.create_share(share_name, protocols="NFS")
    expect(e, 400, "InvalidHeaderValue")


def test_every_name_is_checked_before_anything_is_looked_for(service, share):
    with pytest.raises(HttpResponseError) as e:
        service.get_share_client("Bad_Name").get_share_properties()
    expect(e, 400, "InvalidResourceName")
    with pytest.raises(HttpResponseError) as e:
        share.get_file_client("a:b").get_file_properties()
    expect(e, 400, "InvalidResourceName")
    with pytest.raises(HttpResponseError) as e:
        share.get_directory_client("/".join(["d"] * 251)).get_directory_properties()
    expect(e, 400, "InvalidFileOrDirectoryPathName")
    with pytest.raises(HttpResponseError) as e:
        share.get_directory_client("/".join(["x" * 200] * 11)).get_directory_properties()
    expect(e, 400, "InvalidFileOrDirectoryPathName")


def test_a_snapshot_is_read_only_through_every_door(share):
    share.create_directory("d").upload_file("f.txt", b"x")
    snap = share.create_snapshot()["snapshot"]
    view = type(share).from_share_url(share.url, snapshot=snap, credential=share.credential)
    with pytest.raises(HttpResponseError) as e:
        view.get_directory_client("new").create_directory()
    expect(e, 400, "InvalidQueryParameterValue")
    with pytest.raises(HttpResponseError) as e:
        view.get_file_client("d/f.txt").delete_file()
    expect(e, 400, "InvalidQueryParameterValue")
    with pytest.raises(HttpResponseError) as e:
        view.get_share_stats()
    expect(e, 400, "InvalidQueryParameterValue")


def test_deleting_a_share_whose_snapshot_is_leased(service, share_name):
    client = service.create_share(share_name)
    snap = client.create_snapshot()["snapshot"]
    view = type(client).from_share_url(client.url, snapshot=snap, credential=client.credential)
    lease = view.acquire_lease(lease_id="11111111-1111-1111-1111-111111111111")
    with pytest.raises(HttpResponseError) as e:
        client.delete_share(delete_snapshots=True)
    expect(e, 409, "DeleteShareWhenSnapshotLeased")
    lease.release()
    client.delete_share(delete_snapshots=True)


def test_access_tier_change(share):
    share.set_share_properties(access_tier="Cool")
    props = share.get_share_properties()
    assert props.access_tier == "Cool"
    with pytest.raises(HttpResponseError) as e:
        share.set_share_properties(access_tier="Bogus")
    expect(e, 400, "InvalidHeaderValue")


def test_set_properties_changes_the_etag(share):
    before = share.get_share_properties().etag
    after = share.set_share_properties(quota=10)["etag"]
    assert after != before
    assert share.get_share_properties().etag == after


def test_metadata_is_replaced(share):
    share.set_share_metadata({"a": "1", "b": "2"})
    assert share.get_share_properties().metadata == {"a": "1", "b": "2"}
    share.set_share_metadata({"c": "3"})
    assert share.get_share_properties().metadata == {"c": "3"}
    share.set_share_metadata({})
    assert share.get_share_properties().metadata == {}


def test_invalid_metadata_name(share):
    with pytest.raises(HttpResponseError) as e:
        share.set_share_metadata({"1abc": "x"})
    expect(e, 400, "InvalidMetadata")


def test_list_shares_prefix_metadata_and_paging(service):
    prefix = fileutil.unique("lp")
    names = [f"{prefix}{i}" for i in range(3)]
    clients = [service.create_share(n, metadata={"n": n[-1]}) for n in names]
    try:
        listed = list(service.list_shares(name_starts_with=prefix, include_metadata=True))
        assert [s.name for s in listed] == names
        assert [s.metadata for s in listed] == [{"n": "0"}, {"n": "1"}, {"n": "2"}]
        assert all(s.quota == 102400 and s.etag for s in listed)
        pages = list(service.list_shares(name_starts_with=prefix, results_per_page=2).by_page())
        assert [len(list(p)) for p in pages] == [2, 1]
        assert list(service.list_shares(name_starts_with=prefix + "zz")) == []
    finally:
        for c in clients:
            fileutil.delete_share(c)


def test_snapshots_keep_the_content_of_the_moment(share):
    d = share.create_directory("dir")
    d.upload_file("a.txt", b"before")
    snap = share.create_snapshot(metadata={"why": "test"})
    assert snap["snapshot"].endswith("Z") and snap["etag"]
    d.upload_file("a.txt", b"after, longer")
    d.upload_file("b.txt", b"new")

    snapshot_share = type(share).from_share_url(share.url, snapshot=snap["snapshot"], credential=share.credential)
    assert snapshot_share.get_file_client("dir/a.txt").download_file().readall() == b"before"
    assert [f.name for f in snapshot_share.list_directories_and_files("dir")] == ["a.txt"]
    assert snapshot_share.get_share_properties().metadata == {"why": "test"}
    assert share.get_file_client("dir/a.txt").download_file().readall() == b"after, longer"
    # a snapshot cannot be written
    with pytest.raises(HttpResponseError):
        snapshot_share.set_share_metadata({"x": "y"})


def test_list_shares_includes_snapshots_when_asked(service, share):
    snap = share.create_snapshot()["snapshot"]
    plain = [s.name for s in service.list_shares(name_starts_with=share.share_name)]
    assert plain == [share.share_name]
    both = list(service.list_shares(name_starts_with=share.share_name, include_snapshots=True))
    assert sorted(str(s.snapshot) for s in both) == sorted([snap, "None"])


def test_delete_share_with_snapshots_needs_the_flag(service, share_name):
    client = service.create_share(share_name)
    client.create_snapshot()
    with pytest.raises(HttpResponseError) as e:
        client.delete_share()
    expect(e, 409, "ShareHasSnapshots")
    client.delete_share(delete_snapshots=True)
    with pytest.raises(ResourceNotFoundError):
        client.get_share_properties()


def test_delete_one_snapshot(share):
    snap = share.create_snapshot()["snapshot"]
    snap_client = type(share).from_share_url(share.url, snapshot=snap, credential=share.credential)
    snap_client.delete_share()
    with pytest.raises(ResourceNotFoundError) as e:
        snap_client.get_share_properties()
    expect(e, 404, "ShareNotFound")
    share.get_share_properties()


def test_stored_access_policies(share):
    start = datetime(2026, 1, 1, tzinfo=timezone.utc)
    expiry = start + timedelta(days=3650)
    policy = AccessPolicy(permission=ShareSasPermissions(read=True, list=True), start=start, expiry=expiry)
    share.set_share_access_policy({"reader": policy})
    got = share.get_share_access_policy()["signed_identifiers"]
    assert [p.id for p in got] == ["reader"]
    assert str(got[0].access_policy.permission) == "rl"
    assert fileutil.as_utc(got[0].access_policy.expiry) == expiry
    share.set_share_access_policy({})
    assert share.get_share_access_policy()["signed_identifiers"] == []


def test_share_stats_of_an_empty_share(share):
    assert share.get_share_stats() == 0


def test_share_lease_cycle(share):
    lease = share.acquire_lease(lease_duration=-1)
    assert share.get_share_properties().lease.state == "leased"
    assert share.get_share_properties(lease=lease).quota
    with pytest.raises(HttpResponseError) as e:
        share.delete_share()
    expect(e, 412, "LeaseIdMissing")
    lease.change("11111111-1111-1111-1111-111111111111")
    assert lease.id == "11111111-1111-1111-1111-111111111111"
    lease.release()
    assert share.get_share_properties().lease.state == "available"


def test_share_lease_break_and_wrong_ids(share):
    lease = share.acquire_lease(lease_duration=15)
    assert share.get_share_properties().lease.duration == "fixed"
    with pytest.raises(HttpResponseError) as e:
        share.set_share_metadata({"a": "b"}, lease="22222222-2222-2222-2222-222222222222")
    expect(e, 412, "LeaseIdMismatchWithContainerOperation")
    share.set_share_metadata({"a": "b"}, lease=lease)
    assert lease.break_lease(lease_break_period=0) == 0
    assert share.get_share_properties().lease.state == "broken"


def test_set_properties_with_a_lease_id_on_a_share_without_a_lease(share):
    with pytest.raises(HttpResponseError) as e:
        share.set_share_metadata({"a": "b"}, lease="22222222-2222-2222-2222-222222222222")
    expect(e, 412, "LeaseNotPresentWithContainerOperation")


def test_deleted_share_can_be_listed_and_restored(service, share_name):
    client = service.create_share(share_name, metadata={"keep": "me"})
    client.get_directory_client("d").create_directory()
    client.delete_share()
    gone = [s for s in service.list_shares(name_starts_with=share_name, include_deleted=True) if s.deleted]
    assert len(gone) == 1 and gone[0].version and gone[0].deleted_time
    assert [s.name for s in service.list_shares(name_starts_with=share_name)] == []
    restored = fileutil.eventually(lambda: service.undelete_share(share_name, gone[0].version), ResourceExistsError, "ShareBeingDeleted")
    try:
        assert restored.get_share_properties().metadata == {"keep": "me"}
        assert [f.name for f in restored.list_directories_and_files()] == ["d"]
    finally:
        fileutil.delete_share(restored)
