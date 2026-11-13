"""Administration client (HTTPS): queues.

Expected values come from the real recordings in refs/recordings/.../mgmt_tests (create by name returns
LockDuration PT1M, MaxSizeInMegabytes 1024, MaxDeliveryCount 10, DuplicateDetectionHistoryTimeWindow PT10M,
EnableBatchedOperations true, Status Active, MaxMessageSizeInKilobytes 256, ...) and Microsoft's test_mgmt_queues.py.
Error mapping (HTTP status -> SDK exception): 409 -> ResourceExistsError, 404 -> ResourceNotFoundError,
400/401 -> HttpResponseError.
"""

from datetime import timedelta

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.servicebus.management import AccessRights, AuthorizationRule, EntityStatus, QueueProperties

from sbutil import uniq

HUGE = timedelta(days=10675198)  # real default TTL / auto-delete: P10675199DT2H48M5.4775807S


def test_create_queue_returns_properties_with_real_azure_defaults(admin):
    name = uniq("q")
    q = admin.create_queue(name)
    assert isinstance(q, QueueProperties)
    assert q.name == name
    assert q.lock_duration == timedelta(minutes=1)
    assert q.max_size_in_megabytes == 1024
    assert q.requires_duplicate_detection is False
    assert q.requires_session is False
    assert q.default_message_time_to_live > HUGE
    assert q.dead_lettering_on_message_expiration is False
    assert q.duplicate_detection_history_time_window == timedelta(minutes=10)
    assert q.max_delivery_count == 10
    assert q.enable_batched_operations is True
    assert q.status == "Active"
    assert q.auto_delete_on_idle > HUGE
    assert q.enable_partitioning is False
    assert q.enable_express is False
    assert q.forward_to is None and q.forward_dead_lettered_messages_to is None
    assert q.max_message_size_in_kilobytes == 256
    assert q.availability_status == "Available"


def test_get_queue_matches_created_queue(admin):
    name = uniq("q")
    created = admin.create_queue(name, max_delivery_count=7)
    got = admin.get_queue(name)
    assert got.name == name
    assert got.max_delivery_count == 7
    assert got.lock_duration == created.lock_duration


def test_create_queue_with_same_name_raises_resource_exists(admin):
    name = uniq("q")
    admin.create_queue(name)
    with pytest.raises(ResourceExistsError):
        admin.create_queue(name)


def test_get_missing_queue_raises_resource_not_found(admin):
    with pytest.raises(ResourceNotFoundError):
        admin.get_queue("non_existing_queue")


def test_delete_missing_queue_raises_resource_not_found(admin):
    with pytest.raises(ResourceNotFoundError):
        admin.delete_queue("non_existing_queue")


def test_delete_queue_then_get_raises_resource_not_found(admin):
    name = uniq("q")
    admin.create_queue(name)
    admin.delete_queue(name)
    with pytest.raises(ResourceNotFoundError):
        admin.get_queue(name)
    with pytest.raises(ResourceNotFoundError):
        admin.delete_queue(name)


def test_queue_can_be_created_again_after_delete(admin):
    name = uniq("q")
    admin.create_queue(name, max_delivery_count=3)
    admin.delete_queue(name)
    assert admin.create_queue(name).max_delivery_count == 10


def test_create_queue_with_empty_name_is_refused(admin):
    with pytest.raises(HttpResponseError):
        admin.create_queue("")


def test_list_queues_is_empty_without_queues(admin):
    assert list(admin.list_queues()) == []


def test_list_queues_returns_created_queues(admin):
    names = sorted(uniq("lq") for _ in range(3))
    for n in names:
        admin.create_queue(n)
    assert sorted(q.name for q in admin.list_queues()) == names


def test_list_queues_forgets_deleted_queue(admin):
    a, b = uniq("a"), uniq("b")
    admin.create_queue(a)
    admin.create_queue(b)
    admin.delete_queue(a)
    assert [q.name for q in admin.list_queues()] == [b]


def test_queue_name_with_slash_dot_dash_underscore(admin):
    # Microsoft's test_mgmt_queue_list_with_special_chars: "txt/.-_123"
    name = "txt/.-_123"
    admin.create_queue(name)
    assert [q.name for q in admin.list_queues()] == [name]
    assert admin.get_queue(name).name == name
    admin.delete_queue(name)
    assert list(admin.list_queues()) == []


def test_list_queues_pages_through_more_than_100_queues(admin):
    # the SDK reads 100 at a time ($skip/$top); 105 queues need a second page
    names = {f"pg-{i:03d}" for i in range(105)}
    for n in names:
        admin.create_queue(n)
    assert {q.name for q in admin.list_queues()} == names


def test_queue_names_are_case_insensitive(admin):
    # Microsoft's test comment: "Queue names are also case-insensitive."
    admin.create_queue("MixedCaseQueue")
    with pytest.raises(ResourceExistsError):
        admin.create_queue("mixedcasequeue")


@pytest.mark.parametrize(
    "kwarg, value, attr, expected",
    [
        ("lock_duration", timedelta(seconds=13), "lock_duration", timedelta(seconds=13)),
        ("lock_duration", "PT13S", "lock_duration", timedelta(seconds=13)),
        ("lock_duration", timedelta(minutes=5), "lock_duration", timedelta(minutes=5)),
        ("max_size_in_megabytes", 2048, "max_size_in_megabytes", 2048),
        ("max_size_in_megabytes", 5120, "max_size_in_megabytes", 5120),
        ("default_message_time_to_live", timedelta(minutes=11), "default_message_time_to_live", timedelta(minutes=11)),
        ("default_message_time_to_live", "PT11M2S", "default_message_time_to_live", timedelta(minutes=11, seconds=2)),
        ("dead_lettering_on_message_expiration", True, "dead_lettering_on_message_expiration", True),
        ("requires_duplicate_detection", True, "requires_duplicate_detection", True),
        ("duplicate_detection_history_time_window", timedelta(minutes=12), "duplicate_detection_history_time_window", timedelta(minutes=12)),
        ("duplicate_detection_history_time_window", "PT12M3S", "duplicate_detection_history_time_window", timedelta(minutes=12, seconds=3)),
        ("max_delivery_count", 14, "max_delivery_count", 14),
        ("max_delivery_count", 1, "max_delivery_count", 1),
        ("enable_batched_operations", False, "enable_batched_operations", False),
        ("requires_session", True, "requires_session", True),
        ("auto_delete_on_idle", timedelta(minutes=10), "auto_delete_on_idle", timedelta(minutes=10)),
        ("auto_delete_on_idle", "PT10M1S", "auto_delete_on_idle", timedelta(minutes=10, seconds=1)),
        ("status", EntityStatus.DISABLED, "status", "Disabled"),
        ("status", EntityStatus.SEND_DISABLED, "status", "SendDisabled"),
        ("status", EntityStatus.RECEIVE_DISABLED, "status", "ReceiveDisabled"),
        ("user_metadata", "some metadata ✓", "user_metadata", "some metadata ✓"),
        ("enable_partitioning", True, "enable_partitioning", True),
        ("enable_express", True, "enable_express", True),
    ],
    ids=lambda v: v if isinstance(v, str) and len(v) < 40 else None,
)
def test_queue_property_round_trip(admin, kwarg, value, attr, expected):
    name = uniq("q")
    created = admin.create_queue(name, **{kwarg: value})
    assert getattr(created, attr) == expected
    assert getattr(admin.get_queue(name), attr) == expected


def test_queue_with_all_settings_at_once(admin):
    name = uniq("q")
    admin.create_queue(
        name,
        auto_delete_on_idle=timedelta(minutes=10),
        dead_lettering_on_message_expiration=True,
        default_message_time_to_live=timedelta(minutes=11),
        duplicate_detection_history_time_window=timedelta(minutes=12),
        enable_batched_operations=True,
        lock_duration=timedelta(seconds=13),
        max_delivery_count=14,
        max_size_in_megabytes=3072,
        requires_session=True,
        user_metadata="meta",
    )
    q = admin.get_queue(name)
    assert q.auto_delete_on_idle == timedelta(minutes=10)
    assert q.dead_lettering_on_message_expiration is True
    assert q.default_message_time_to_live == timedelta(minutes=11)
    assert q.duplicate_detection_history_time_window == timedelta(minutes=12)
    assert q.lock_duration == timedelta(seconds=13)
    assert q.max_delivery_count == 14
    assert q.max_size_in_megabytes == 3072
    assert q.requires_session is True
    assert q.user_metadata == "meta"


def test_queue_forward_to_is_stored_as_the_target_entity(admin):
    target, name = uniq("target"), uniq("src")
    admin.create_queue(target)
    admin.create_queue(name, forward_to=target, forward_dead_lettered_messages_to=target)
    q = admin.get_queue(name)
    assert q.forward_to.endswith("/" + target)
    assert q.forward_dead_lettered_messages_to.endswith("/" + target)


def test_queue_authorization_rule_round_trip(admin):
    rule = AuthorizationRule(
        type="SharedAccessAuthorizationRule",
        key_name="test_key",
        claim_type="SharedAccessKey",
        claim_value="None",
        rights=[AccessRights.MANAGE, AccessRights.SEND, AccessRights.LISTEN],
        primary_key="cHJpbWFyeWtleXByaW1hcnlrZXlwcmltYXJ5a2V5MDE=",
        secondary_key="c2Vjb25kYXJ5a2V5c2Vjb25kYXJ5a2V5c2Vjb25kMDE=",
    )
    name = uniq("q")
    admin.create_queue(name, authorization_rules=[rule])
    got = admin.get_queue(name).authorization_rules
    assert got[0].key_name == "test_key"
    assert len(got[0].rights) == 3


def test_queue_lock_duration_over_five_minutes_is_refused(admin):
    with pytest.raises(HttpResponseError):
        admin.create_queue(uniq("q"), lock_duration=timedelta(minutes=6))


# -- update -------------------------------------------------------------------------------------
def test_update_queue_changes_property(admin):
    name = uniq("q")
    q = admin.create_queue(name)
    q.lock_duration = timedelta(minutes=2)
    admin.update_queue(q)
    assert admin.get_queue(name).lock_duration == timedelta(minutes=2)


def test_update_queue_changes_all_updatable_settings(admin):
    name = uniq("q")
    q = admin.create_queue(name)
    q.auto_delete_on_idle = timedelta(minutes=10)
    q.dead_lettering_on_message_expiration = True
    q.default_message_time_to_live = timedelta(minutes=11)
    q.duplicate_detection_history_time_window = timedelta(minutes=12)
    q.enable_batched_operations = False
    q.lock_duration = timedelta(seconds=13)
    q.max_delivery_count = 14
    q.max_size_in_megabytes = 3072
    q.user_metadata = "updated"
    admin.update_queue(q)
    got = admin.get_queue(name)
    assert got.auto_delete_on_idle == timedelta(minutes=10)
    assert got.dead_lettering_on_message_expiration is True
    assert got.default_message_time_to_live == timedelta(minutes=11)
    assert got.duplicate_detection_history_time_window == timedelta(minutes=12)
    assert got.enable_batched_operations is False
    assert got.lock_duration == timedelta(seconds=13)
    assert got.max_delivery_count == 14
    assert got.max_size_in_megabytes == 3072
    assert got.user_metadata == "updated"


def test_update_queue_from_dict(admin):
    # Microsoft's test_mgmt_queue_update_dict_success
    name = uniq("q")
    q = admin.create_queue(name)
    d = {**vars(q), "max_delivery_count": 21, "lock_duration": timedelta(seconds=30)}
    d = {k: v for k, v in d.items() if not k.startswith("_")}
    admin.update_queue(d)
    got = admin.get_queue(name)
    assert got.max_delivery_count == 21
    assert got.lock_duration == timedelta(seconds=30)


def test_update_queue_sets_and_clears_forwarding(admin):
    target, name = uniq("target"), uniq("src")
    admin.create_queue(target)
    q = admin.create_queue(name)
    q.forward_to = target
    q.forward_dead_lettered_messages_to = target
    admin.update_queue(q)
    q = admin.get_queue(name)
    assert q.forward_to.endswith("/" + target)
    assert q.forward_dead_lettered_messages_to.endswith("/" + target)
    q.forward_to = None
    q.forward_dead_lettered_messages_to = None
    admin.update_queue(q)
    q = admin.get_queue(name)
    assert q.forward_to is None and q.forward_dead_lettered_messages_to is None


def test_update_queue_status_to_disabled_and_back(admin):
    name = uniq("q")
    q = admin.create_queue(name)
    q.status = EntityStatus.DISABLED
    admin.update_queue(q)
    q = admin.get_queue(name)
    assert q.status == "Disabled"
    q.status = EntityStatus.ACTIVE
    admin.update_queue(q)
    assert admin.get_queue(name).status == "Active"


def test_update_queue_requires_session_cannot_be_changed(admin):
    # recording: 400 "The value for the RequiresSession property of an existing Queue cannot be changed."
    q = admin.create_queue(uniq("q"))
    q.requires_session = True
    with pytest.raises(HttpResponseError):
        admin.update_queue(q)


def test_update_missing_queue_is_refused(admin):
    q = admin.create_queue(uniq("q"))
    q.name = "iewdm-missing"
    with pytest.raises(HttpResponseError):
        admin.update_queue(q)


def test_update_queue_with_lock_duration_of_25_days_is_refused(admin):
    q = admin.create_queue(uniq("q"))
    q.lock_duration = timedelta(days=25)
    with pytest.raises(HttpResponseError):
        admin.update_queue(q)


def test_update_queue_with_wrong_argument_types_raises_type_error(admin):
    with pytest.raises(TypeError):
        admin.update_queue(None)
    with pytest.raises(TypeError):
        admin.update_queue(Exception("test"))


def test_queue_properties_cannot_be_constructed_directly():
    with pytest.raises(TypeError):
        QueueProperties("randomname")


def test_update_queue_partitioning_cannot_be_changed(admin):
    name = uniq("q")
    q = admin.create_queue(name)
    q.enable_partitioning = True
    with pytest.raises(HttpResponseError):
        admin.update_queue(q)
