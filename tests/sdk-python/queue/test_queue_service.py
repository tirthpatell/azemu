"""Queue service: listing queues and service properties (logging, metrics, CORS)."""

import pytest
from azure.core.exceptions import HttpResponseError
from azure.storage.queue import CorsRule, Metrics, QueueAnalyticsLogging, RetentionPolicy

from queue_support import assert_storage_error, unique_prefix, unique_queue_name


def _create_prefixed(service, prefix, count, metadata=None):
    names = [f"{prefix}-{i}" for i in range(count)]
    for name in names:
        service.create_queue(name, metadata=metadata)
    return names


# -- list queues ------------------------------------------------------------


def test_list_queues_returns_queues_matching_prefix(service):
    prefix = unique_prefix()
    created = _create_prefixed(service, prefix, 3)

    names = [q.name for q in service.list_queues(name_starts_with=prefix)]

    assert sorted(names) == sorted(created)


def test_list_queues_prefix_excludes_other_queues(service):
    prefix = unique_prefix()
    _create_prefixed(service, prefix, 2)
    other = unique_queue_name()
    service.create_queue(other)

    names = [q.name for q in service.list_queues(name_starts_with=prefix)]

    assert other not in names
    assert len(names) == 2


def test_list_queues_without_include_metadata_omits_metadata(service):
    prefix = unique_prefix()
    service.create_queue(f"{prefix}-a", metadata={"team": "alpha"})

    (props,) = list(service.list_queues(name_starts_with=prefix))

    assert not props.metadata


def test_list_queues_include_metadata_returns_metadata(service):
    prefix = unique_prefix()
    service.create_queue(f"{prefix}-a", metadata={"team": "alpha"})

    (props,) = list(service.list_queues(name_starts_with=prefix, include_metadata=True))

    assert props.metadata == {"team": "alpha"}


def test_list_queues_results_per_page_returns_every_queue_across_pages(service):
    prefix = unique_prefix()
    created = _create_prefixed(service, prefix, 5)

    pages = service.list_queues(name_starts_with=prefix, results_per_page=2).by_page()
    page_sizes = []
    names = []
    for page in pages:
        items = [q.name for q in page]
        page_sizes.append(len(items))
        names.extend(items)

    assert all(size <= 2 for size in page_sizes)
    assert len(page_sizes) >= 3
    assert sorted(names) == sorted(created)


def test_list_queues_continuation_token_resumes_listing(service):
    prefix = unique_prefix()
    created = _create_prefixed(service, prefix, 5)

    first = service.list_queues(name_starts_with=prefix, results_per_page=2).by_page()
    page_one = [q.name for q in next(first)]
    token = first.continuation_token
    assert token, "a listing with more queues than the page size must return a token"

    second = service.list_queues(name_starts_with=prefix, results_per_page=2).by_page(
        continuation_token=token
    )
    rest = [q.name for page in second for q in page]

    assert not set(page_one) & set(rest)
    assert sorted(page_one + rest) == sorted(created)


# -- service properties -----------------------------------------------------


def test_set_logging_properties_round_trip(service):
    service.set_service_properties(
        analytics_logging=QueueAnalyticsLogging(
            read=True,
            write=True,
            delete=True,
            retention_policy=RetentionPolicy(enabled=True, days=3),
        )
    )

    logging = service.get_service_properties()["analytics_logging"]

    assert logging.read is True
    assert logging.write is True
    assert logging.delete is True
    assert logging.retention_policy.enabled is True
    assert logging.retention_policy.days == 3


def test_set_hour_metrics_round_trip(service):
    service.set_service_properties(
        hour_metrics=Metrics(
            enabled=True,
            include_apis=True,
            retention_policy=RetentionPolicy(enabled=True, days=5),
        )
    )

    hour = service.get_service_properties()["hour_metrics"]

    assert hour.enabled is True
    assert hour.include_apis is True
    assert hour.retention_policy.days == 5


def test_set_minute_metrics_round_trip(service):
    service.set_service_properties(
        minute_metrics=Metrics(
            enabled=True,
            include_apis=False,
            retention_policy=RetentionPolicy(enabled=True, days=2),
        )
    )

    minute = service.get_service_properties()["minute_metrics"]

    assert minute.enabled is True
    assert minute.include_apis is False
    assert minute.retention_policy.days == 2


def test_set_cors_rules_round_trip(service):
    service.set_service_properties(
        cors=[
            CorsRule(
                ["www.xyz.com"],
                ["GET", "PUT"],
                allowed_headers=["x-ms-meta-data*"],
                exposed_headers=["x-ms-meta-abc"],
                max_age_in_seconds=500,
            )
        ]
    )

    (rule,) = service.get_service_properties()["cors"]

    assert "www.xyz.com" in rule.allowed_origins
    assert "GET" in rule.allowed_methods and "PUT" in rule.allowed_methods
    assert rule.max_age_in_seconds == 500


@pytest.mark.not_on_azurite(reason="Azurite accepts more than five CORS rules; Azure returns 400 InvalidXmlDocument")
def test_set_more_than_five_cors_rules_is_rejected_400(service):
    # Recording test_too_many_cors_rules: six rules -> 400 InvalidXmlDocument.
    rules = [CorsRule([f"https://site{i}.example"], ["GET"]) for i in range(6)]

    with pytest.raises(HttpResponseError) as exc:
        service.set_service_properties(cors=rules)

    assert_storage_error(exc.value, 400, "InvalidXmlDocument")


@pytest.mark.not_on_azurite(reason="Azurite accepts a retention of 366 days; Azure returns 400 InvalidXmlDocument")
def test_set_retention_days_above_365_is_rejected_400(service):
    # Recording test_retention_too_long: Days=366 -> 400 InvalidXmlDocument.
    with pytest.raises(HttpResponseError) as exc:
        service.set_service_properties(
            hour_metrics=Metrics(
                enabled=True,
                retention_policy=RetentionPolicy(enabled=True, days=366),
            )
        )

    assert_storage_error(exc.value, 400, "InvalidXmlDocument")
