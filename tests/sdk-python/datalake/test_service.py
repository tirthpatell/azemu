"""DataLakeServiceClient: listing filesystems, service properties, client factories."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError
from azure.storage.filedatalake import (
    AnalyticsLogging,
    CorsRule,
    DataLakeDirectoryClient,
    DataLakeFileClient,
    DataLakeServiceClient,
    FileSystemClient,
    Metrics,
    PublicAccess,
    RetentionPolicy,
    StaticWebsite,
)

from dlutil import DEV_ACCOUNT, DEV_KEY, assert_error, uniq


# -- client construction (no emulator traffic) -------------------------------
def test_service_client_urls_derive_blob_endpoint_from_dfs(emu):
    svc = DataLakeServiceClient(account_url=emu.url("dfs", DEV_ACCOUNT), credential=DEV_KEY)
    assert svc.account_name == DEV_ACCOUNT
    assert svc.url == f"http://{DEV_ACCOUNT}.dfs.core.localhost:{emu.port}/"
    assert svc.primary_hostname == f"{DEV_ACCOUNT}.dfs.core.localhost:{emu.port}"
    # the SDK swaps ".dfs." for ".blob." in the host for blob-endpoint operations
    assert svc._blob_service_client.url == f"http://{DEV_ACCOUNT}.blob.core.localhost:{emu.port}/"


def test_get_file_system_client_url(service, emu):
    fsc = service.get_file_system_client("myfs")
    assert isinstance(fsc, FileSystemClient)
    assert fsc.file_system_name == "myfs"
    assert fsc.url == f"http://{DEV_ACCOUNT}.dfs.core.localhost:{emu.port}/myfs"
    assert fsc._container_client.url == f"http://{DEV_ACCOUNT}.blob.core.localhost:{emu.port}/myfs"


def test_get_directory_client_url_and_names(service, emu):
    d = service.get_directory_client("myfs", "a/b")
    assert isinstance(d, DataLakeDirectoryClient)
    assert d.file_system_name == "myfs"
    assert d.path_name == "a/b"
    assert d.url == f"http://{DEV_ACCOUNT}.dfs.core.localhost:{emu.port}/myfs/a/b"


def test_get_file_client_url_encodes_special_characters(service, emu):
    f = service.get_file_client("myfs", "dir/a b+c#d%e.txt")
    assert isinstance(f, DataLakeFileClient)
    assert f.path_name == "dir/a b+c#d%e.txt"
    assert f.file_system_name == "myfs"
    assert f.url.endswith("/myfs/dir/a%20b%2Bc%23d%25e.txt") or f.url.endswith("/myfs/dir/a%20b+c%23d%25e.txt")


def test_clients_from_clients_share_account(service):
    fsc = service.get_file_system_client("myfs")
    d = fsc.get_directory_client("d1")
    f = d.get_file_client("f1.txt")
    sub = d.get_sub_directory_client("sub")
    assert f.path_name == "d1/f1.txt"
    assert sub.path_name == "d1/sub"
    assert f.account_name == DEV_ACCOUNT


# -- listing filesystems -----------------------------------------------------
def test_list_file_systems_contains_new_filesystem(service, uniq):
    created = service.create_file_system(uniq("fs"))
    listed = [f.name for f in service.list_file_systems()]
    assert created.file_system_name in listed


def test_list_file_systems_properties(service, uniq):
    created = service.create_file_system(uniq("fs"))
    item = next(f for f in service.list_file_systems() if f.name == created.file_system_name)
    assert item.etag
    assert item.last_modified is not None
    assert item.has_immutability_policy is not None
    assert item.has_legal_hold is not None
    assert item.lease.state == "available"
    assert item.lease.status == "unlocked"


def test_list_file_systems_name_starts_with(service, uniq):
    p = uniq("pfx")
    a = service.create_file_system(p + "a")
    b = service.create_file_system(p + "b")
    service.create_file_system(uniq("other"))
    got = [f.name for f in service.list_file_systems(name_starts_with=p)]
    assert got == [a.file_system_name, b.file_system_name]


def test_list_file_systems_prefix_without_match_is_empty(service, uniq):
    assert list(service.list_file_systems(name_starts_with=uniq("nomatch"))) == []


def test_list_file_systems_is_sorted_by_name(service, uniq):
    p = uniq("srt")
    for suffix in ("c", "a", "b"):
        service.create_file_system(p + suffix)
    got = [f.name for f in service.list_file_systems(name_starts_with=p)]
    assert got == [p + "a", p + "b", p + "c"]


def test_list_file_systems_by_page_and_continuation(service, uniq):
    p = uniq("pg")
    expected = [p + str(i) for i in range(5)]
    for n in expected:
        service.create_file_system(n)
    pager = service.list_file_systems(name_starts_with=p, results_per_page=2).by_page()
    pages = [[f.name for f in page] for page in pager]
    assert [len(x) for x in pages] == [2, 2, 1]
    assert [n for page in pages for n in page] == expected


def test_list_file_systems_resume_from_continuation_token(service, uniq):
    p = uniq("ct")
    expected = [p + str(i) for i in range(4)]
    for n in expected:
        service.create_file_system(n)
    first = service.list_file_systems(name_starts_with=p, results_per_page=2).by_page()
    page1 = [f.name for f in next(first)]
    token = first.continuation_token
    assert token
    rest = service.list_file_systems(name_starts_with=p, results_per_page=2).by_page(continuation_token=token)
    page2 = [f.name for f in next(rest)]
    assert page1 == expected[:2]
    assert page2 == expected[2:]


def test_list_file_systems_include_metadata(service, uniq):
    name = uniq("md")
    service.create_file_system(name, metadata={"hello": "world", "number": "42"})
    items = list(service.list_file_systems(name_starts_with=name, include_metadata=True))
    assert len(items) == 1
    assert items[0].metadata == {"hello": "world", "number": "42"}


def test_list_file_systems_without_include_metadata_has_no_metadata(service, uniq):
    name = uniq("md")
    service.create_file_system(name, metadata={"hello": "world"})
    items = list(service.list_file_systems(name_starts_with=name))
    assert len(items) == 1
    assert not items[0].metadata


def test_list_file_systems_public_access(service, uniq):
    name = uniq("pa")
    service.create_file_system(name, public_access=PublicAccess.FILESYSTEM)
    other = uniq("pb")
    service.create_file_system(other)
    by_name = {f.name: f for f in service.list_file_systems(name_starts_with="p")}
    assert by_name[name].public_access == PublicAccess.FILESYSTEM
    assert by_name[other].public_access is None


def test_service_create_file_system_returns_client(service, uniq):
    name = uniq("fs")
    fsc = service.create_file_system(name)
    assert isinstance(fsc, FileSystemClient)
    assert fsc.file_system_name == name
    assert fsc.exists()


def test_service_create_file_system_twice_raises_already_exists(service, uniq):
    name = uniq("fs")
    service.create_file_system(name)
    with pytest.raises(ResourceExistsError) as e:
        service.create_file_system(name)
    # created through the blob endpoint, so the code is the container one (recorded)
    assert_error(e, "ContainerAlreadyExists", 409)


def test_service_delete_file_system(service, uniq):
    name = uniq("fs")
    service.create_file_system(name)
    service.delete_file_system(name)
    assert name not in [f.name for f in service.list_file_systems(name_starts_with=name)]
    assert not service.get_file_system_client(name).exists()


# -- service properties (blob endpoint, shared with Blob service) ------------
def test_get_service_properties_shape(service):
    props = service.get_service_properties()
    for key in ("analytics_logging", "hour_metrics", "minute_metrics", "cors"):
        assert key in props


def test_set_service_properties_default_round_trip(service):
    resp = service.set_service_properties(
        analytics_logging=AnalyticsLogging(),
        hour_metrics=Metrics(),
        minute_metrics=Metrics(),
        cors=[],
        target_version="2014-02-14",
    )
    assert resp is None
    props = service.get_service_properties()
    assert props["analytics_logging"].read is False
    assert props["analytics_logging"].write is False
    assert props["analytics_logging"].delete is False
    assert props["hour_metrics"].enabled is False
    assert props["minute_metrics"].enabled is False
    assert props["cors"] == []
    assert props["target_version"] == "2014-02-14"


def test_set_service_properties_empty_call_raises_value_error(service):
    with pytest.raises(ValueError):
        service.set_service_properties()


def test_set_service_properties_logging(service):
    logging = AnalyticsLogging(read=True, write=True, delete=True, retention_policy=RetentionPolicy(enabled=True, days=5))
    service.set_service_properties(analytics_logging=logging)
    got = service.get_service_properties()["analytics_logging"]
    assert (got.version, got.read, got.write, got.delete) == (logging.version, True, True, True)
    assert got.retention_policy.enabled is True
    assert got.retention_policy.days == 5


def test_set_service_properties_hour_metrics(service):
    m = Metrics(include_apis=False, enabled=True, retention_policy=RetentionPolicy(enabled=True, days=5))
    service.set_service_properties(hour_metrics=m)
    got = service.get_service_properties()["hour_metrics"]
    assert got.enabled is True
    assert got.include_apis is False
    assert got.retention_policy.days == 5


def test_set_service_properties_minute_metrics(service):
    m = Metrics(include_apis=True, enabled=True, retention_policy=RetentionPolicy(enabled=True, days=5))
    service.set_service_properties(minute_metrics=m)
    got = service.get_service_properties()["minute_metrics"]
    assert got.enabled is True
    assert got.include_apis is True
    assert got.retention_policy.days == 5


def test_set_service_properties_cors_round_trip(service):
    rule1 = CorsRule(["www.xyz.com"], ["GET"])
    rule2 = CorsRule(
        ["www.xyz.com", "www.ab.com", "www.bc.com"],
        ["GET", "PUT"],
        max_age_in_seconds=500,
        exposed_headers=["x-ms-meta-data*", "x-ms-meta-source*", "x-ms-meta-abc", "x-ms-meta-bcd"],
        allowed_headers=["x-ms-meta-data*", "x-ms-meta-target*", "x-ms-meta-xyz", "x-ms-meta-foo"],
    )
    service.set_service_properties(cors=[rule1, rule2])
    got = service.get_service_properties()["cors"]
    def items(value):
        # the SDK hands these back as the comma-separated string stored by the service
        return sorted(value.split(",")) if isinstance(value, str) else sorted(value)

    assert len(got) == 2
    assert items(got[0].allowed_origins) == ["www.xyz.com"]
    assert items(got[1].allowed_origins) == ["www.ab.com", "www.bc.com", "www.xyz.com"]
    assert items(got[1].allowed_methods) == ["GET", "PUT"]
    assert got[1].max_age_in_seconds == 500
    assert len(items(got[1].exposed_headers)) == 4
    assert len(items(got[1].allowed_headers)) == 4


def test_set_service_properties_delete_retention_policy(service):
    service.set_service_properties(delete_retention_policy=RetentionPolicy(enabled=True, days=2))
    got = service.get_service_properties()["delete_retention_policy"]
    assert got.enabled is True
    assert got.days == 2


@pytest.mark.parametrize("days", [0, 366])
def test_set_delete_retention_policy_out_of_range_is_rejected(service, days):
    service.set_service_properties(delete_retention_policy=RetentionPolicy(enabled=True, days=1))
    with pytest.raises(HttpResponseError) as e:
        service.set_service_properties(delete_retention_policy=RetentionPolicy(enabled=True, days=days))
    # recorded: 400 InvalidXmlDocument from the blob endpoint
    assert_error(e, "InvalidXmlDocument", 400)
    got = service.get_service_properties()["delete_retention_policy"]
    assert got.days == 1


def test_set_delete_retention_policy_edge_values_accepted(service):
    for days in (1, 365):
        service.set_service_properties(delete_retention_policy=RetentionPolicy(enabled=True, days=days))
        assert service.get_service_properties()["delete_retention_policy"].days == days


def test_set_static_website_properties(service):
    sw = StaticWebsite(enabled=True, index_document="index.html", error_document404_path="errors/error/404error.html")
    service.set_service_properties(static_website=sw)
    got = service.get_service_properties()["static_website"]
    assert got.enabled is True
    assert got.index_document == "index.html"
    assert got.error_document404_path == "errors/error/404error.html"


def test_disabled_static_website_drops_documents(service):
    sw = StaticWebsite(enabled=False, index_document="index.html", error_document404_path="errors/error/404error.html")
    service.set_service_properties(static_website=sw)
    got = service.get_service_properties()["static_website"]
    assert got.enabled is False
    assert not got.index_document
    assert not got.error_document404_path


def test_set_static_website_does_not_change_cors(service):
    cors = [CorsRule(["www.xyz.com"], ["GET"])]
    service.set_service_properties(cors=cors)
    service.set_service_properties(static_website=StaticWebsite(enabled=True, index_document="index.html"))
    props = service.get_service_properties()
    assert props["static_website"].enabled is True
    assert len(props["cors"]) == 1
