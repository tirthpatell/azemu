"""Administration client (HTTPS): topics and subscriptions.

Defaults from the real recordings (topic create by name, subscription create): topic MaxSizeInMegabytes 1024,
DuplicateDetectionHistoryTimeWindow PT10M, SupportOrdering true, Status Active; subscription LockDuration PT1M,
DeadLetteringOnFilterEvaluationExceptions true, MaxDeliveryCount 10, EnableBatchedOperations true.
"""

from datetime import timedelta

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.servicebus.management import EntityStatus, SubscriptionProperties, TopicProperties

from sbutil import uniq

HUGE = timedelta(days=10675198)


# -- topics -------------------------------------------------------------------------------------
def test_create_topic_returns_real_azure_defaults(admin):
    name = uniq("t")
    t = admin.create_topic(name)
    assert isinstance(t, TopicProperties)
    assert t.name == name
    assert t.default_message_time_to_live > HUGE
    assert t.max_size_in_megabytes == 1024
    assert t.requires_duplicate_detection is False
    assert t.duplicate_detection_history_time_window == timedelta(minutes=10)
    assert t.enable_batched_operations is True
    assert t.size_in_bytes == 0
    assert t.filtering_messages_before_publishing is False
    assert t.status == "Active"
    assert t.support_ordering is True
    assert t.auto_delete_on_idle > HUGE
    assert t.enable_partitioning is False
    assert t.enable_express is False
    assert t.max_message_size_in_kilobytes == 256


def test_get_topic(admin):
    name = uniq("t")
    admin.create_topic(name, max_size_in_megabytes=2048)
    assert admin.get_topic(name).max_size_in_megabytes == 2048


def test_create_duplicate_topic_raises_resource_exists(admin):
    name = uniq("t")
    admin.create_topic(name)
    with pytest.raises(ResourceExistsError):
        admin.create_topic(name)


def test_get_and_delete_missing_topic_raise_resource_not_found(admin):
    with pytest.raises(ResourceNotFoundError):
        admin.get_topic("no-such-topic")
    with pytest.raises(ResourceNotFoundError):
        admin.delete_topic("no-such-topic")


def test_delete_topic_removes_it_and_its_subscriptions(admin):
    t = uniq("t")
    admin.create_topic(t)
    admin.create_subscription(t, "s1")
    admin.delete_topic(t)
    with pytest.raises(ResourceNotFoundError):
        admin.get_topic(t)
    admin.create_topic(t)
    assert list(admin.list_subscriptions(t)) == []


def test_list_topics(admin):
    names = sorted(uniq("lt") for _ in range(3))
    for n in names:
        admin.create_topic(n)
    assert sorted(t.name for t in admin.list_topics()) == names


def test_list_topics_empty_and_after_delete(admin):
    assert list(admin.list_topics()) == []
    n = uniq("t")
    admin.create_topic(n)
    admin.delete_topic(n)
    assert list(admin.list_topics()) == []


@pytest.mark.parametrize(
    "kwarg, value, expected",
    [
        ("default_message_time_to_live", timedelta(minutes=11), timedelta(minutes=11)),
        ("default_message_time_to_live", "PT11M2S", timedelta(minutes=11, seconds=2)),
        ("max_size_in_megabytes", 3072, 3072),
        ("requires_duplicate_detection", True, True),
        ("duplicate_detection_history_time_window", timedelta(minutes=12), timedelta(minutes=12)),
        ("enable_batched_operations", False, False),
        ("status", EntityStatus.SEND_DISABLED, "SendDisabled"),
        ("status", EntityStatus.DISABLED, "Disabled"),
        ("auto_delete_on_idle", timedelta(minutes=10), timedelta(minutes=10)),
        ("user_metadata", "topic metadata", "topic metadata"),
        ("enable_partitioning", True, True),
        ("enable_express", True, True),
        # filtering_messages_before_publishing: the SDK never sends it, so it cannot round-trip
        ("support_ordering", False, False),
    ],
    ids=lambda v: v if isinstance(v, str) and len(v) < 40 else None,
)
def test_topic_property_round_trip(admin, kwarg, value, expected):
    name = uniq("t")
    created = admin.create_topic(name, **{kwarg: value})
    assert getattr(created, kwarg) == expected
    assert getattr(admin.get_topic(name), kwarg) == expected


def test_update_topic(admin):
    name = uniq("t")
    t = admin.create_topic(name)
    t.default_message_time_to_live = timedelta(minutes=11)
    t.duplicate_detection_history_time_window = timedelta(minutes=12)
    t.enable_batched_operations = False
    t.max_size_in_megabytes = 3072
    t.auto_delete_on_idle = timedelta(minutes=10)
    t.user_metadata = "changed"
    admin.update_topic(t)
    got = admin.get_topic(name)
    assert got.default_message_time_to_live == timedelta(minutes=11)
    assert got.duplicate_detection_history_time_window == timedelta(minutes=12)
    assert got.enable_batched_operations is False
    assert got.max_size_in_megabytes == 3072
    assert got.auto_delete_on_idle == timedelta(minutes=10)
    assert got.user_metadata == "changed"


def test_update_topic_from_dict(admin):
    name = uniq("t")
    t = admin.create_topic(name)
    d = {k: v for k, v in vars(t).items() if not k.startswith("_")}
    d["max_size_in_megabytes"] = 4096
    admin.update_topic(d)
    assert admin.get_topic(name).max_size_in_megabytes == 4096


def test_update_missing_topic_is_refused(admin):
    t = admin.create_topic(uniq("t"))
    t.name = "iewdm-missing"
    with pytest.raises(HttpResponseError):
        admin.update_topic(t)


def test_update_topic_with_wrong_argument_types_raises_type_error(admin):
    with pytest.raises(TypeError):
        admin.update_topic(None)
    with pytest.raises(TypeError):
        admin.update_topic(Exception("x"))


# -- subscriptions ------------------------------------------------------------------------------
def test_create_subscription_returns_real_azure_defaults(admin):
    t, s = uniq("t"), uniq("s")
    admin.create_topic(t)
    sub = admin.create_subscription(t, s)
    assert isinstance(sub, SubscriptionProperties)
    assert sub.name == s
    assert sub.lock_duration == timedelta(minutes=1)
    assert sub.requires_session is False
    assert sub.default_message_time_to_live > HUGE
    assert sub.dead_lettering_on_message_expiration is False
    assert sub.dead_lettering_on_filter_evaluation_exceptions is True
    assert sub.max_delivery_count == 10
    assert sub.enable_batched_operations is True
    assert sub.status == "Active"
    assert sub.forward_to is None and sub.forward_dead_lettered_messages_to is None
    assert sub.auto_delete_on_idle > HUGE
    assert sub.availability_status == "Available"


def test_get_subscription(admin):
    t, s = uniq("t"), uniq("s")
    admin.create_topic(t)
    admin.create_subscription(t, s, max_delivery_count=14)
    assert admin.get_subscription(t, s).max_delivery_count == 14


def test_create_duplicate_subscription_raises_resource_exists(admin):
    t, s = uniq("t"), uniq("s")
    admin.create_topic(t)
    admin.create_subscription(t, s)
    with pytest.raises(ResourceExistsError):
        admin.create_subscription(t, s)


def test_same_subscription_name_in_two_topics(admin):
    t1, t2 = uniq("t"), uniq("t")
    admin.create_topic(t1)
    admin.create_topic(t2)
    admin.create_subscription(t1, "shared", max_delivery_count=3)
    admin.create_subscription(t2, "shared", max_delivery_count=4)
    assert admin.get_subscription(t1, "shared").max_delivery_count == 3
    assert admin.get_subscription(t2, "shared").max_delivery_count == 4


def test_create_subscription_on_missing_topic_raises_resource_not_found(admin):
    with pytest.raises(ResourceNotFoundError):
        admin.create_subscription("no-such-topic", "s")


def test_get_and_delete_missing_subscription_raise_resource_not_found(admin):
    t = uniq("t")
    admin.create_topic(t)
    with pytest.raises(ResourceNotFoundError):
        admin.get_subscription(t, "nope")
    with pytest.raises(ResourceNotFoundError):
        admin.delete_subscription(t, "nope")


def test_delete_subscription(admin):
    t, s = uniq("t"), uniq("s")
    admin.create_topic(t)
    admin.create_subscription(t, s)
    admin.delete_subscription(t, s)
    with pytest.raises(ResourceNotFoundError):
        admin.get_subscription(t, s)
    assert list(admin.list_subscriptions(t)) == []


def test_list_subscriptions_only_lists_those_of_the_topic(admin):
    t1, t2 = uniq("t"), uniq("t")
    admin.create_topic(t1)
    admin.create_topic(t2)
    for n in ("a", "b", "c"):
        admin.create_subscription(t1, n)
    admin.create_subscription(t2, "z")
    assert sorted(s.name for s in admin.list_subscriptions(t1)) == ["a", "b", "c"]
    assert [s.name for s in admin.list_subscriptions(t2)] == ["z"]


def test_list_subscriptions_of_missing_topic_raises_resource_not_found(admin):
    with pytest.raises(ResourceNotFoundError):
        list(admin.list_subscriptions("no-such-topic"))


@pytest.mark.parametrize(
    "kwarg, value, expected",
    [
        ("lock_duration", timedelta(seconds=13), timedelta(seconds=13)),
        ("lock_duration", "PT13S", timedelta(seconds=13)),
        ("requires_session", True, True),
        ("default_message_time_to_live", timedelta(minutes=11), timedelta(minutes=11)),
        ("dead_lettering_on_message_expiration", True, True),
        ("dead_lettering_on_filter_evaluation_exceptions", False, False),
        ("max_delivery_count", 14, 14),
        ("enable_batched_operations", False, False),
        ("status", EntityStatus.DISABLED, "Disabled"),
        ("status", EntityStatus.RECEIVE_DISABLED, "ReceiveDisabled"),
        ("auto_delete_on_idle", timedelta(minutes=10), timedelta(minutes=10)),
        ("user_metadata", "sub metadata", "sub metadata"),
    ],
    ids=lambda v: v if isinstance(v, str) and len(v) < 40 else None,
)
def test_subscription_property_round_trip(admin, kwarg, value, expected):
    t, s = uniq("t"), uniq("s")
    admin.create_topic(t)
    created = admin.create_subscription(t, s, **{kwarg: value})
    assert getattr(created, kwarg) == expected
    assert getattr(admin.get_subscription(t, s), kwarg) == expected


def test_subscription_forward_to_queue(admin):
    t, s, target = uniq("t"), uniq("s"), uniq("target")
    admin.create_topic(t)
    admin.create_queue(target)
    admin.create_subscription(t, s, forward_to=target, forward_dead_lettered_messages_to=target)
    sub = admin.get_subscription(t, s)
    assert sub.forward_to.endswith("/" + target)
    assert sub.forward_dead_lettered_messages_to.endswith("/" + target)


def test_update_subscription(admin):
    t, s = uniq("t"), uniq("s")
    admin.create_topic(t)
    sub = admin.create_subscription(t, s)
    sub.lock_duration = timedelta(seconds=13)
    sub.max_delivery_count = 14
    sub.default_message_time_to_live = timedelta(minutes=11)
    sub.dead_lettering_on_message_expiration = True
    sub.enable_batched_operations = False
    sub.auto_delete_on_idle = timedelta(minutes=10)
    admin.update_subscription(t, sub)
    got = admin.get_subscription(t, s)
    assert got.lock_duration == timedelta(seconds=13)
    assert got.max_delivery_count == 14
    assert got.default_message_time_to_live == timedelta(minutes=11)
    assert got.dead_lettering_on_message_expiration is True
    assert got.enable_batched_operations is False
    assert got.auto_delete_on_idle == timedelta(minutes=10)


def test_update_subscription_from_dict(admin):
    t, s = uniq("t"), uniq("s")
    admin.create_topic(t)
    sub = admin.create_subscription(t, s)
    d = {k: v for k, v in vars(sub).items() if not k.startswith("_")}
    d["max_delivery_count"] = 21
    admin.update_subscription(t, d)
    assert admin.get_subscription(t, s).max_delivery_count == 21


def test_update_subscription_requires_session_cannot_be_changed(admin):
    t, s = uniq("t"), uniq("s")
    admin.create_topic(t)
    sub = admin.create_subscription(t, s)
    sub.requires_session = True
    with pytest.raises(HttpResponseError):
        admin.update_subscription(t, sub)


def test_update_missing_subscription_is_refused(admin):
    t, s = uniq("t"), uniq("s")
    admin.create_topic(t)
    sub = admin.create_subscription(t, s)
    sub.name = "iewdm-missing"
    with pytest.raises(HttpResponseError):
        admin.update_subscription(t, sub)


def test_update_subscription_with_wrong_argument_types_raises_type_error(admin):
    with pytest.raises(TypeError):
        admin.update_subscription("t", None)
    with pytest.raises(TypeError):
        admin.update_subscription("t", Exception("x"))
