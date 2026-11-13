"""Queue lifecycle, properties, metadata, stored access policies and queue naming rules."""

import random
import string
import uuid
from datetime import timedelta

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.storage.queue import AccessPolicy, QueueSasPermissions

from queue_support import as_utc, assert_storage_error, utc_now


# -- create / delete ----------------------------------------------------------


# Real Azure answers a repeated Create Queue with identical (or no) metadata with
# 204 and no error (recording test_create_queue_fail_on_exist: 201, then 204; the
# raw-HTTP tests in test_queue_rest.py check that). azure-storage-queue 12.18.1
# turns ANY 204 on create into ResourceExistsError (deserialize_queue_creation in
# azure/storage/queue/_deserialize.py), so through the SDK a repeated create
# always raises, even though the service itself did not fail it.


def test_sdk_create_queue_twice_without_metadata_raises_resource_exists_on_204(service, queue_name):
    service.create_queue(queue_name)

    with pytest.raises(ResourceExistsError) as exc:
        service.create_queue(queue_name)

    assert_storage_error(exc.value, 204, "QueueAlreadyExists")
    assert "Queue already exists" in str(exc.value)


def test_sdk_create_queue_twice_with_same_metadata_raises_resource_exists_on_204(service, queue_name):
    service.create_queue(queue_name, metadata={"env": "test"})

    with pytest.raises(ResourceExistsError) as exc:
        service.create_queue(queue_name, metadata={"env": "test"})

    assert_storage_error(exc.value, 204, "QueueAlreadyExists")


def test_sdk_repeated_create_leaves_the_queue_and_its_metadata_unchanged(service, queue_name):
    client = service.create_queue(queue_name, metadata={"env": "test"})

    with pytest.raises(ResourceExistsError):
        service.create_queue(queue_name, metadata={"env": "test"})

    assert client.get_queue_properties().metadata == {"env": "test"}


def test_create_existing_queue_with_different_metadata_raises_queue_already_exists(
    service, queue_name
):
    # Recording test_create_queue_fail_on_exist_different_metadata: 201, then 409.
    service.create_queue(queue_name)

    with pytest.raises(ResourceExistsError) as exc:
        service.create_queue(queue_name, metadata={"val": "value"})

    assert_storage_error(exc.value, 409, "QueueAlreadyExists")


def test_delete_queue_then_properties_raise_queue_not_found(service, queue_name):
    client = service.create_queue(queue_name)
    client.delete_queue()

    with pytest.raises(ResourceNotFoundError) as exc:
        client.get_queue_properties()

    assert_storage_error(exc.value, 404, "QueueNotFound")


def test_delete_missing_queue_raises_queue_not_found(service, queue_name):
    # Recording test_delete_non_existing_queue: 404 QueueNotFound.
    with pytest.raises(ResourceNotFoundError) as exc:
        service.delete_queue(queue_name)

    assert_storage_error(exc.value, 404, "QueueNotFound")


# -- queue naming rules ------------------------------------------------------
# Rules: 3-63 characters, lowercase letters, digits and hyphens, first and last
# character alphanumeric, no consecutive hyphens.
# Characters that break the rules: 400 InvalidResourceName (recording
# test_unicode_create_queue_unicode_name). Length violations: 400 OutOfRangeInput.
# No recording covers the length case; OutOfRangeInput is the common-error-table
# code for an input outside its range. Flagged for the lead to confirm.


@pytest.mark.parametrize(
    "bad_name",
    [
        pytest.param("UpperCase", id="uppercase"),
        pytest.param("my--queue", id="consecutive-hyphens"),
        pytest.param("啊齄丂狛", id="non-ascii"),
    ],
)
def test_create_queue_rejects_invalid_characters_or_hyphens_400(service, bad_name):
    with pytest.raises(HttpResponseError) as exc:
        service.create_queue(bad_name)

    assert_storage_error(exc.value, 400, "InvalidResourceName")


@pytest.mark.parametrize(
    "bad_name",
    [
        pytest.param("ab", id="too-short"),
        pytest.param("a" * 64, id="too-long"),
    ],
)
def test_create_queue_rejects_name_length_outside_3_to_63_400(service, bad_name):
    with pytest.raises(HttpResponseError) as exc:
        service.create_queue(bad_name)

    assert_storage_error(exc.value, 400, "OutOfRangeInput")


def _boundary_names():
    rng = random.Random()
    alnum = string.ascii_lowercase + string.digits
    return {
        "three-characters": "".join(rng.choices(alnum, k=3)),
        "sixty-three-characters": ("q" + uuid.uuid4().hex * 2)[:63],
    }


@pytest.mark.parametrize("case", ["three-characters", "sixty-three-characters"])
def test_create_queue_accepts_names_at_the_rule_boundaries(service, case):
    name = _boundary_names()[case]
    client = service.create_queue(name)
    try:
        assert client.get_queue_properties() is not None
    finally:
        client.delete_queue()


# -- properties and metadata ---------------------------------------------------


def test_approximate_message_count_reflects_sent_messages(queue):
    for i in range(3):
        queue.send_message(f"m{i}")

    props = queue.get_queue_properties()

    assert props.approximate_message_count == 3


def test_metadata_given_at_create_is_returned_by_get_properties(service, queue_name):
    client = service.create_queue(queue_name, metadata={"k1": "v1", "k2": "v2"})

    assert client.get_queue_properties().metadata == {"k1": "v1", "k2": "v2"}


def test_set_queue_metadata_replaces_existing_metadata(queue):
    queue.set_queue_metadata(metadata={"x": "1"})
    assert queue.get_queue_properties().metadata == {"x": "1"}

    queue.set_queue_metadata(metadata={"y": "2"})
    assert queue.get_queue_properties().metadata == {"y": "2"}


# -- stored access policies ----------------------------------------------------


def test_access_policy_round_trips_id_permission_and_times(queue):
    start = utc_now()
    expiry = start + timedelta(hours=1)
    queue.set_queue_access_policy(
        {"reader": AccessPolicy(permission=QueueSasPermissions(read=True, process=True), start=start, expiry=expiry)}
    )

    policies = queue.get_queue_access_policy()

    assert set(policies) == {"reader"}
    policy = policies["reader"]
    assert policy.permission == "rp"
    assert as_utc(policy.start) == start
    assert as_utc(policy.expiry) == expiry


def test_access_policy_accepts_five_identifiers(queue):
    now = utc_now()
    identifiers = {
        f"id{i}": AccessPolicy(permission="r", start=now, expiry=now + timedelta(hours=1))
        for i in range(5)
    }

    queue.set_queue_access_policy(identifiers)

    assert set(queue.get_queue_access_policy()) == set(identifiers)


def test_access_policy_with_six_identifiers_is_rejected_400(queue):
    # Set Queue ACL: "If more than five access policies are passed in the request
    # body, the service returns status code 400 (Bad Request)." The SDK only
    # refuses more than 15 on the client, so the service must enforce five.
    now = utc_now()
    identifiers = {
        f"id{i}": AccessPolicy(permission="r", start=now, expiry=now + timedelta(hours=1))
        for i in range(6)
    }

    with pytest.raises(HttpResponseError) as exc:
        queue.set_queue_access_policy(identifiers)

    assert exc.value.status_code == 400


def test_set_empty_access_policy_clears_stored_policies(queue):
    now = utc_now()
    queue.set_queue_access_policy({"p": AccessPolicy(permission="r", start=now, expiry=now + timedelta(hours=1))})

    queue.set_queue_access_policy({})

    assert dict(queue.get_queue_access_policy()) == {}


def test_get_access_policy_on_missing_queue_raises_queue_not_found(service, queue_name):
    # Recording test_get_queue_acl_with_non_existing_queue: 404 QueueNotFound.
    client = service.get_queue_client(queue_name)

    with pytest.raises(ResourceNotFoundError) as exc:
        client.get_queue_access_policy()

    assert_storage_error(exc.value, 404, "QueueNotFound")

