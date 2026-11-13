"""Service level operations: list containers, service properties, account info."""

import pytest
from azure.core.exceptions import HttpResponseError
from azure.storage.blob import (
    BlobAnalyticsLogging,
    BlobServiceClient,
    CorsRule,
    Metrics,
    RetentionPolicy,
    StaticWebsite,
)

from blobutil import assert_error


@pytest.fixture
def restore_props(service):
    """Put service properties back to something neutral after a test changed them."""
    yield
    try:
        service.set_service_properties(
            analytics_logging=BlobAnalyticsLogging(
                read=False, write=False, delete=False, retention_policy=RetentionPolicy(enabled=False)
            ),
            hour_metrics=Metrics(enabled=False, retention_policy=RetentionPolicy(enabled=False)),
            minute_metrics=Metrics(enabled=False, retention_policy=RetentionPolicy(enabled=False)),
            cors=[],
            delete_retention_policy=RetentionPolicy(enabled=False),
            static_website=StaticWebsite(enabled=False),
        )
    except Exception:
        pass


# --- list containers -------------------------------------------------------


def test_list_containers_contains_created_container(service, container):
    names = [c.name for c in service.list_containers()]
    assert container.container_name in names


def test_list_containers_prefix_returns_only_matching_sorted(service, uniq):
    prefix = uniq("pfx")
    names = [f"{prefix}{s}" for s in ("c", "a", "b")]
    for n in names:
        service.create_container(n)
    service.create_container(uniq("other"))
    got = [c.name for c in service.list_containers(name_starts_with=prefix)]
    assert got == sorted(names)


def test_list_containers_prefix_without_match_is_empty(service, uniq):
    assert list(service.list_containers(name_starts_with=uniq("nomatch"))) == []


def test_list_containers_paging_with_results_per_page(service, uniq):
    prefix = uniq("pg")
    names = sorted(f"{prefix}{i}" for i in range(5))
    for n in names:
        service.create_container(n)
    pages = [[c.name for c in p] for p in service.list_containers(name_starts_with=prefix, results_per_page=2).by_page()]
    assert [len(p) for p in pages] == [2, 2, 1]
    assert [n for p in pages for n in p] == names


def test_list_containers_continuation_token_resumes_listing(service, uniq):
    prefix = uniq("ct")
    names = sorted(f"{prefix}{i}" for i in range(4))
    for n in names:
        service.create_container(n)
    pager = service.list_containers(name_starts_with=prefix, results_per_page=2).by_page()
    first = [c.name for c in next(pager)]
    token = pager.continuation_token
    assert token  # more results remain
    resumed = service.list_containers(name_starts_with=prefix, results_per_page=2).by_page(
        continuation_token=token
    )
    rest = [c.name for p in resumed for c in p]
    assert first + rest == names


def test_list_containers_include_metadata(service, uniq):
    name = uniq("md")
    service.create_container(name, metadata={"owner": "alice", "Env": "dev"})
    got = [c for c in service.list_containers(name_starts_with=name, include_metadata=True)]
    assert len(got) == 1
    md = {k.lower(): v for k, v in got[0].metadata.items()}
    assert md == {"owner": "alice", "env": "dev"}


def test_list_containers_without_include_metadata_has_no_metadata(service, uniq):
    name = uniq("nm")
    service.create_container(name, metadata={"a": "b"})
    got = list(service.list_containers(name_starts_with=name))
    assert len(got) == 1
    assert not got[0].metadata


def test_list_containers_item_properties(service, uniq):
    name = uniq("pr")
    service.create_container(name)
    (item,) = list(service.list_containers(name_starts_with=name))
    assert item.etag
    assert item.last_modified is not None
    assert item.lease.state == "available"
    assert item.lease.status == "unlocked"
    assert item.public_access is None
    assert item.has_immutability_policy is False
    assert item.has_legal_hold is False


# --- service properties ----------------------------------------------------


def test_get_service_properties_has_expected_sections(service):
    props = service.get_service_properties()
    for key in ("analytics_logging", "hour_metrics", "minute_metrics", "cors", "delete_retention_policy", "static_website"):
        assert key in props


def test_set_cors_roundtrip(service, restore_props):
    rule = CorsRule(
        allowed_origins=["https://example.com", "https://other.example.org"],
        allowed_methods=["GET", "PUT"],
        allowed_headers=["x-ms-meta-*", "content-type"],
        exposed_headers=["x-ms-meta-data*"],
        max_age_in_seconds=500,
    )
    service.set_service_properties(cors=[rule])
    got = service.get_service_properties()["cors"]
    assert len(got) == 1
    # The SDK hands lists back as comma separated strings.
    assert sorted(got[0].allowed_origins.split(",")) == ["https://example.com", "https://other.example.org"]
    assert sorted(got[0].allowed_methods.split(",")) == ["GET", "PUT"]
    assert got[0].max_age_in_seconds == 500
    assert got[0].exposed_headers == "x-ms-meta-data*"


def test_set_empty_cors_clears_rules(service, restore_props):
    service.set_service_properties(cors=[CorsRule(["*"], ["GET"])])
    service.set_service_properties(cors=[])
    assert service.get_service_properties()["cors"] == []


@pytest.mark.not_on_azurite(reason="Azurite accepts more than 5 CORS rules")
def test_set_too_many_cors_rules_is_rejected(service, restore_props):
    rules = [CorsRule([f"https://o{i}.example.com"], ["GET"]) for i in range(6)]
    with pytest.raises(HttpResponseError) as ei:
        service.set_service_properties(cors=rules)
    assert_error(ei, "InvalidXmlDocument", 400)


def test_set_logging_roundtrip(service, restore_props):
    logging = BlobAnalyticsLogging(
        read=True, write=True, delete=False, version="1.0", retention_policy=RetentionPolicy(enabled=True, days=5)
    )
    service.set_service_properties(analytics_logging=logging)
    got = service.get_service_properties()["analytics_logging"]
    assert (got.read, got.write, got.delete) == (True, True, False)
    assert got.retention_policy.enabled is True
    assert got.retention_policy.days == 5


def test_set_hour_metrics_roundtrip(service, restore_props):
    service.set_service_properties(
        hour_metrics=Metrics(enabled=True, include_apis=True, retention_policy=RetentionPolicy(enabled=True, days=3))
    )
    got = service.get_service_properties()["hour_metrics"]
    assert got.enabled is True
    assert got.include_apis is True
    assert got.retention_policy.days == 3


def test_set_minute_metrics_roundtrip(service, restore_props):
    service.set_service_properties(
        minute_metrics=Metrics(enabled=True, include_apis=False, retention_policy=RetentionPolicy(enabled=True, days=2))
    )
    got = service.get_service_properties()["minute_metrics"]
    assert got.enabled is True
    assert got.include_apis is False
    assert got.retention_policy.days == 2


@pytest.mark.not_on_azurite(reason="Azurite does not validate retention days <= 365")
def test_retention_over_365_days_is_rejected(service, restore_props):
    with pytest.raises(HttpResponseError) as ei:
        service.set_service_properties(
            minute_metrics=Metrics(enabled=True, include_apis=True, retention_policy=RetentionPolicy(enabled=True, days=366))
        )
    assert_error(ei, "InvalidXmlDocument", 400)


def test_set_static_website_roundtrip(service, restore_props):
    service.set_service_properties(
        static_website=StaticWebsite(enabled=True, index_document="index.html", error_document404_path="err/404.html")
    )
    got = service.get_service_properties()["static_website"]
    assert got.enabled is True
    assert got.index_document == "index.html"
    assert got.error_document404_path == "err/404.html"


def test_setting_one_section_leaves_others_untouched(service, restore_props):
    service.set_service_properties(cors=[CorsRule(["*"], ["GET"], max_age_in_seconds=60)])
    service.set_service_properties(
        hour_metrics=Metrics(enabled=True, include_apis=True, retention_policy=RetentionPolicy(enabled=True, days=1))
    )
    props = service.get_service_properties()
    assert len(props["cors"]) == 1
    assert props["hour_metrics"].enabled is True


def test_set_default_service_version(service, restore_props):
    service.set_service_properties(target_version="2020-10-02")
    assert service.get_service_properties()["target_version"] == "2020-10-02"


def test_set_empty_service_properties_is_client_error(service):
    with pytest.raises(ValueError):
        service.set_service_properties()


# --- account information ---------------------------------------------------


def test_get_account_information(service):
    info = service.get_account_information()
    assert info["sku_name"]
    assert info["account_kind"]
    assert info["is_hns_enabled"] is False


def test_account_information_from_container_and_blob_clients(container, blob_name):
    assert container.get_account_information()["account_kind"]
    blob = container.get_blob_client(blob_name)
    blob.upload_blob(b"x")
    assert blob.get_account_information()["sku_name"]


def test_service_client_exposes_account_name(service, account_name):
    assert service.account_name == account_name
