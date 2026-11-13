"""Table access policies (stored ACL) and table service properties."""

from datetime import datetime, timezone

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.data.tables import (
    TableAnalyticsLogging,
    TableCorsRule,
    TableMetrics,
    TableRetentionPolicy,
    TableSasPermissions,
    TableAccessPolicy,
)

from tableutil import code

START = datetime(2020, 10, 11, 0, 0, 0, tzinfo=timezone.utc)
EXPIRY = datetime(2031, 10, 12, 0, 0, 0, tzinfo=timezone.utc)


def policy(perm="r"):
    perms = TableSasPermissions.from_string(perm)
    return TableAccessPolicy(permission=perms, start=START, expiry=EXPIRY)


# -- access policy ------------------------------------------------------------


def test_new_table_has_no_access_policies(table):
    assert table.get_table_access_policy() == {}


def test_set_access_policy_round_trips_id_permission_and_times(table):
    table.set_table_access_policy({"reader": policy("r")})
    got = table.get_table_access_policy()
    assert list(got) == ["reader"]
    assert str(got["reader"].permission) == "r"
    assert got["reader"].start == START
    assert got["reader"].expiry == EXPIRY


def test_set_access_policy_keeps_all_permission_letters(table):
    table.set_table_access_policy({"all": policy("raud")})
    assert sorted(str(table.get_table_access_policy()["all"].permission)) == sorted("raud")


def test_set_access_policy_accepts_five_identifiers(table):
    ids = {f"id{i}": policy("r") for i in range(5)}
    table.set_table_access_policy(ids)
    assert sorted(table.get_table_access_policy()) == sorted(ids)


def test_set_access_policy_with_six_identifiers_is_rejected(table):
    # The service answers 400 InvalidXmlDocument; the SDK reports it as a ValueError.
    ids = {f"id{i}": policy("r") for i in range(6)}
    with pytest.raises(ValueError, match="5 access policies"):
        table.set_table_access_policy(ids)


def test_set_access_policy_replaces_previous_set(table):
    table.set_table_access_policy({"first": policy("r")})
    table.set_table_access_policy({"second": policy("r")})
    assert list(table.get_table_access_policy()) == ["second"]


def test_set_empty_access_policy_clears_identifiers(table):
    table.set_table_access_policy({"first": policy("r")})
    table.set_table_access_policy({})
    assert table.get_table_access_policy() == {}


@pytest.mark.not_on_azurite(reason="Azurite rejects a SignedIdentifier that has no AccessPolicy (400); Azure accepts it with 204")
def test_set_access_policy_identifier_without_policy_is_allowed(table):
    table.set_table_access_policy({"bare": None})
    assert "bare" in table.get_table_access_policy()


def test_access_policy_is_per_table(service, uniq):
    a = service.create_table(uniq("aclA"))
    b = service.create_table(uniq("aclB"))
    a.set_table_access_policy({"only-a": policy("r")})
    assert b.get_table_access_policy() == {}


def test_get_access_policy_of_missing_table_is_404(service, table_name):
    with pytest.raises(ResourceNotFoundError) as exc:
        service.get_table_client(table_name).get_table_access_policy()
    assert exc.value.status_code == 404


def test_set_access_policy_of_missing_table_is_404(service, table_name):
    with pytest.raises(ResourceNotFoundError) as exc:
        service.get_table_client(table_name).set_table_access_policy({"x": policy("r")})
    assert exc.value.status_code == 404


# -- service properties -------------------------------------------------------


def test_get_service_properties_has_all_sections(service):
    props = service.get_service_properties()
    assert set(props) >= {"analytics_logging", "hour_metrics", "minute_metrics", "cors"}
    assert props["analytics_logging"].version == "1.0"
    assert props["hour_metrics"].version == "1.0"
    assert props["minute_metrics"].version == "1.0"
    assert isinstance(props["cors"], list)


def test_set_logging_round_trips(service):
    logging = TableAnalyticsLogging(
        read=True, write=True, delete=True, retention_policy=TableRetentionPolicy(enabled=True, days=5)
    )
    service.set_service_properties(analytics_logging=logging)
    got = service.get_service_properties()["analytics_logging"]
    assert (got.read, got.write, got.delete) == (True, True, True)
    assert got.retention_policy.enabled is True
    assert got.retention_policy.days == 5


def test_set_hour_metrics_round_trips(service):
    metrics = TableMetrics(
        enabled=True, include_apis=True, retention_policy=TableRetentionPolicy(enabled=True, days=7)
    )
    service.set_service_properties(hour_metrics=metrics)
    got = service.get_service_properties()["hour_metrics"]
    assert got.enabled is True
    assert got.include_apis is True
    assert got.retention_policy.days == 7


def test_set_minute_metrics_round_trips(service):
    metrics = TableMetrics(
        enabled=True, include_apis=False, retention_policy=TableRetentionPolicy(enabled=True, days=3)
    )
    service.set_service_properties(minute_metrics=metrics)
    got = service.get_service_properties()["minute_metrics"]
    assert got.enabled is True
    assert got.include_apis is False
    assert got.retention_policy.days == 3


def test_set_cors_round_trips_a_rule_with_every_field(service):
    rules = [
        TableCorsRule(
            allowed_origins=["www.xyz.com", "www.ab.com"],
            allowed_methods=["GET", "PUT"],
            allowed_headers=["x-ms-meta-data*", "x-ms-meta-foo"],
            exposed_headers=["x-ms-meta-abc"],
            max_age_in_seconds=500,
        ),
    ]
    service.set_service_properties(cors=rules)
    got = service.get_service_properties()["cors"]
    assert len(got) == 1
    assert got[0].allowed_origins == ["www.xyz.com", "www.ab.com"]
    assert got[0].allowed_methods == ["GET", "PUT"]
    # Azure answers the names first and the prefixes ending in `*` last (recording
    # `test_table_service_properties.pyTestTableServicePropertiestest_set_cors.json`, entries 1 and 2: set
    # `x-ms-meta-data*,x-ms-meta-target*,x-ms-meta-xyz,x-ms-meta-foo`, answer `x-ms-meta-xyz,x-ms-meta-foo,x-ms-meta-data*,x-ms-meta-target*`)
    assert got[0].allowed_headers == ["x-ms-meta-foo", "x-ms-meta-data*"]
    assert got[0].exposed_headers == ["x-ms-meta-abc"]
    assert got[0].max_age_in_seconds == 500


@pytest.mark.not_on_azurite(reason="Azurite rejects a CORS rule with empty AllowedHeaders/ExposedHeaders (400, empty body)")
def test_set_cors_round_trips_two_rules_with_empty_header_lists(service):
    rules = [
        TableCorsRule(allowed_origins=["www.xyz.com"], allowed_methods=["GET"]),
        TableCorsRule(allowed_origins=["www.ab.com", "www.bc.com"], allowed_methods=["GET", "PUT"],
                      max_age_in_seconds=60),
    ]
    service.set_service_properties(cors=rules)
    got = service.get_service_properties()["cors"]
    assert len(got) == 2
    assert got[0].allowed_origins == ["www.xyz.com"]
    assert got[0].allowed_methods == ["GET"]
    assert got[0].allowed_headers == []
    assert got[1].allowed_origins == ["www.ab.com", "www.bc.com"]
    assert got[1].max_age_in_seconds == 60


def test_set_empty_cors_list_removes_rules(service):
    service.set_service_properties(
        cors=[TableCorsRule(allowed_origins=["*"], allowed_methods=["GET"],
                            allowed_headers=["x-ms-*"], exposed_headers=["x-ms-*"])]
    )
    service.set_service_properties(cors=[])
    assert service.get_service_properties()["cors"] == []


def test_setting_one_section_leaves_the_others_untouched(service):
    service.set_service_properties(
        analytics_logging=TableAnalyticsLogging(read=True, write=False, delete=False),
        cors=[TableCorsRule(allowed_origins=["www.keep.com"], allowed_methods=["GET"],
                            allowed_headers=["x-ms-*"], exposed_headers=["x-ms-*"])],
    )
    service.set_service_properties(
        minute_metrics=TableMetrics(enabled=True, include_apis=True,
                                    retention_policy=TableRetentionPolicy(enabled=True, days=2))
    )
    got = service.get_service_properties()
    assert got["analytics_logging"].read is True
    assert got["cors"][0].allowed_origins == ["www.keep.com"]
    assert got["minute_metrics"].enabled is True


@pytest.mark.not_on_azurite(reason="Azurite rejects more than 5 CORS rules with an empty error code and body")
def test_more_than_five_cors_rules_is_invalid_xml_document(service):
    rules = [TableCorsRule(allowed_origins=["www.xyz.com"], allowed_methods=["GET"],
                           allowed_headers=["x-ms-*"], exposed_headers=["x-ms-*"]) for _ in range(6)]
    with pytest.raises(HttpResponseError) as exc:
        service.set_service_properties(cors=rules)
    assert exc.value.status_code == 400
    assert code(exc.value) == "InvalidXmlDocument"


@pytest.mark.not_on_azurite(reason="Azurite accepts retention of 366 days; Azure allows 1-365")
def test_retention_of_366_days_is_invalid_xml_document(service):
    metrics = TableMetrics(
        enabled=True, include_apis=True, retention_policy=TableRetentionPolicy(enabled=True, days=366)
    )
    with pytest.raises(HttpResponseError) as exc:
        service.set_service_properties(minute_metrics=metrics)
    assert exc.value.status_code == 400
    assert code(exc.value) == "InvalidXmlDocument"
