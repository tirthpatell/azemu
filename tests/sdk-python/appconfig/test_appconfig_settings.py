"""Settings CRUD through the official SDK: add / set / get / delete and conditional headers."""

import time
from datetime import datetime, timedelta, timezone

import pytest
from azure.appconfiguration import ConfigurationSetting
from azure.core import MatchConditions
from azure.core.exceptions import (
    ResourceExistsError,
    ResourceModifiedError,
    ResourceNotFoundError,
)

from appconfigutil import status_of

# --- add -------------------------------------------------------------------


def test_add_returns_all_fields(client, uniq):
    key, label = uniq("key"), uniq("lbl")
    got = client.add_configuration_setting(
        ConfigurationSetting(
            key=key, label=label, value="v1", content_type="text/plain", tags={"a": "b", "c": "d"}
        )
    )
    assert got.key == key
    assert got.label == label
    assert got.value == "v1"
    assert got.content_type == "text/plain"
    assert got.tags == {"a": "b", "c": "d"}
    assert got.read_only is False
    assert isinstance(got.etag, str) and got.etag
    assert isinstance(got.last_modified, datetime)


def test_add_without_label_has_label_none(client, uniq):
    got = client.add_configuration_setting(ConfigurationSetting(key=uniq(), value="v"))
    assert got.label is None


def test_add_with_only_a_key_has_no_value_content_type_or_tags(client, uniq):
    # Recordings: a setting created without value comes back as value=null,
    # content_type=null, tags={}.
    key = uniq()
    got = client.add_configuration_setting(ConfigurationSetting(key=key))
    assert got.value is None
    assert got.content_type is None
    assert got.tags == {}
    again = client.get_configuration_setting(key)
    assert again.value is None
    assert again.content_type is None
    assert again.tags == {}


def test_add_existing_key_raises_resource_exists_412(client, uniq):
    key = uniq()
    client.add_configuration_setting(ConfigurationSetting(key=key, value="first"))
    with pytest.raises(ResourceExistsError) as exc:
        client.add_configuration_setting(ConfigurationSetting(key=key, value="second"))
    assert status_of(exc) == 412
    assert client.get_configuration_setting(key).value == "first"


def test_add_existing_key_and_label_raises_resource_exists(client, uniq):
    key = uniq()
    client.add_configuration_setting(ConfigurationSetting(key=key, label="L", value="first"))
    with pytest.raises(ResourceExistsError):
        client.add_configuration_setting(ConfigurationSetting(key=key, label="L", value="second"))


def test_add_same_key_with_other_label_is_a_new_setting(client, uniq):
    key = uniq()
    client.add_configuration_setting(ConfigurationSetting(key=key, value="no label"))
    client.add_configuration_setting(ConfigurationSetting(key=key, label="L", value="labelled"))


def test_last_modified_is_utc_and_recent(client, uniq):
    got = client.add_configuration_setting(ConfigurationSetting(key=uniq(), value="v"))
    assert got.last_modified.tzinfo is not None
    assert got.last_modified.utcoffset() == timedelta(0)
    assert abs(datetime.now(timezone.utc) - got.last_modified) < timedelta(minutes=2)


def test_etag_has_no_surrounding_quotes_in_the_model(client, uniq):
    # The body carries the bare etag; only the ETag header is quoted.
    got = client.add_configuration_setting(ConfigurationSetting(key=uniq(), value="v"))
    assert not got.etag.startswith('"') and not got.etag.endswith('"')


# --- set -------------------------------------------------------------------


def test_set_creates_a_missing_setting(client, uniq):
    key = uniq()
    got = client.set_configuration_setting(ConfigurationSetting(key=key, value="v"))
    assert got.value == "v"
    assert client.get_configuration_setting(key).value == "v"


def test_set_overwrites_existing_setting(client, uniq):
    key = uniq()
    first = client.set_configuration_setting(ConfigurationSetting(key=key, value="v1"))
    second = client.set_configuration_setting(ConfigurationSetting(key=key, value="v2"))
    assert second.value == "v2"
    assert client.get_configuration_setting(key).value == "v2"
    assert second.etag != first.etag


def test_set_replaces_the_whole_setting_not_just_the_value(client, uniq):
    key = uniq()
    client.set_configuration_setting(
        ConfigurationSetting(key=key, value="v1", content_type="text/plain", tags={"old": "tag"})
    )
    client.set_configuration_setting(ConfigurationSetting(key=key, value="v2", tags={"new": "tag"}))
    got = client.get_configuration_setting(key)
    assert got.value == "v2"
    assert got.content_type is None
    assert got.tags == {"new": "tag"}


def test_etag_changes_on_every_write(client, uniq):
    key = uniq()
    etags = {client.set_configuration_setting(ConfigurationSetting(key=key, value=f"v{i}")).etag for i in range(4)}
    assert len(etags) == 4


def test_set_after_get_keeps_the_value_the_service_returned(client, uniq):
    # The SDK sends etag/last_modified/locked in the body of a PUT; the service must ignore them.
    key = uniq()
    s = client.set_configuration_setting(ConfigurationSetting(key=key, value="v1"))
    s.value = "v2"
    updated = client.set_configuration_setting(s)
    assert updated.value == "v2"
    assert updated.etag != s.etag
    assert updated.read_only is False


def test_set_if_not_modified_with_current_etag_succeeds(client, uniq):
    key = uniq()
    s = client.set_configuration_setting(ConfigurationSetting(key=key, value="v1"))
    s.value = "v2"
    got = client.set_configuration_setting(s, match_condition=MatchConditions.IfNotModified)
    assert got.value == "v2"


def test_set_if_not_modified_with_stale_etag_raises_resource_modified_412(client, uniq):
    key = uniq()
    stale = client.set_configuration_setting(ConfigurationSetting(key=key, value="v1"))
    client.set_configuration_setting(ConfigurationSetting(key=key, value="v2"))
    stale.value = "v3"
    with pytest.raises(ResourceModifiedError) as exc:
        client.set_configuration_setting(stale, match_condition=MatchConditions.IfNotModified)
    assert status_of(exc) == 412
    assert client.get_configuration_setting(key).value == "v2"


def test_set_if_not_modified_on_missing_setting_raises_412(client, uniq):
    ghost = ConfigurationSetting(key=uniq(), value="v", etag="never-existed")
    with pytest.raises(ResourceModifiedError) as exc:
        client.set_configuration_setting(ghost, match_condition=MatchConditions.IfNotModified)
    assert status_of(exc) == 412


def test_set_etag_keyword_overrides_the_setting_etag(client, uniq):
    key = uniq()
    current = client.set_configuration_setting(ConfigurationSetting(key=key, value="v1"))
    stale_model = ConfigurationSetting(key=key, value="v2", etag="stale-etag")
    got = client.set_configuration_setting(
        stale_model, match_condition=MatchConditions.IfNotModified, etag=current.etag
    )
    assert got.value == "v2"


def test_set_if_missing_on_existing_raises_resource_exists_412(client, uniq):
    key = uniq()
    client.set_configuration_setting(ConfigurationSetting(key=key, value="v1"))
    with pytest.raises(ResourceExistsError) as exc:
        client.set_configuration_setting(ConfigurationSetting(key=key, value="v2"), match_condition=MatchConditions.IfMissing)
    assert status_of(exc) == 412


def test_set_if_missing_on_missing_creates(client, uniq):
    key = uniq()
    got = client.set_configuration_setting(ConfigurationSetting(key=key, value="v"), match_condition=MatchConditions.IfMissing)
    assert got.value == "v"


def test_set_if_present_on_missing_raises_412(client, uniq):
    with pytest.raises(ResourceNotFoundError) as exc:
        client.set_configuration_setting(
            ConfigurationSetting(key=uniq(), value="v"), match_condition=MatchConditions.IfPresent
        )
    assert status_of(exc) == 412


def test_set_if_present_on_existing_overwrites(client, uniq):
    key = uniq()
    client.set_configuration_setting(ConfigurationSetting(key=key, value="v1"))
    client.set_configuration_setting(ConfigurationSetting(key=key, value="v2"), match_condition=MatchConditions.IfPresent)
    assert client.get_configuration_setting(key).value == "v2"


# --- get -------------------------------------------------------------------


def test_get_returns_what_was_stored(client, uniq):
    key, label = uniq(), uniq("lbl")
    client.set_configuration_setting(
        ConfigurationSetting(key=key, label=label, value="val", content_type="text/x", tags={"t": "1"})
    )
    got = client.get_configuration_setting(key, label=label)
    assert (got.key, got.label, got.value, got.content_type, got.tags) == (key, label, "val", "text/x", {"t": "1"})
    assert got.read_only is False


def test_get_missing_key_raises_not_found_404(client, uniq):
    with pytest.raises(ResourceNotFoundError) as exc:
        client.get_configuration_setting(uniq())
    assert status_of(exc) == 404


def test_get_wrong_label_raises_not_found(client, uniq):
    key = uniq()
    client.set_configuration_setting(ConfigurationSetting(key=key, label="A", value="v"))
    with pytest.raises(ResourceNotFoundError):
        client.get_configuration_setting(key, label="B")


def test_get_without_label_does_not_find_a_labelled_setting(client, uniq):
    # Recordings: GET /kv/{key} with no label is a 404 when only a labelled one exists.
    key = uniq()
    client.set_configuration_setting(ConfigurationSetting(key=key, label="A", value="v"))
    with pytest.raises(ResourceNotFoundError):
        client.get_configuration_setting(key)


def test_no_label_and_a_label_are_different_settings(client, uniq):
    key = uniq()
    client.set_configuration_setting(ConfigurationSetting(key=key, value="plain"))
    client.set_configuration_setting(ConfigurationSetting(key=key, label="L", value="labelled"))
    assert client.get_configuration_setting(key).value == "plain"
    assert client.get_configuration_setting(key, label="L").value == "labelled"
    client.set_configuration_setting(ConfigurationSetting(key=key, value="plain2"))
    assert client.get_configuration_setting(key, label="L").value == "labelled"


def test_keys_are_case_sensitive(client, uniq):
    key = uniq("CaseKey")
    client.set_configuration_setting(ConfigurationSetting(key=key, value="v"))
    with pytest.raises(ResourceNotFoundError):
        client.get_configuration_setting(key.lower())


def test_get_if_modified_with_current_etag_returns_none(client, uniq):
    # HTTP semantics (If-None-Match -> 304); the SDK turns 304 into None.
    key = uniq()
    s = client.set_configuration_setting(ConfigurationSetting(key=key, value="v"))
    assert client.get_configuration_setting(key, etag=s.etag, match_condition=MatchConditions.IfModified) is None


def test_get_if_modified_with_stale_etag_returns_the_setting(client, uniq):
    key = uniq()
    old = client.set_configuration_setting(ConfigurationSetting(key=key, value="v1"))
    client.set_configuration_setting(ConfigurationSetting(key=key, value="v2"))
    got = client.get_configuration_setting(key, etag=old.etag, match_condition=MatchConditions.IfModified)
    assert got is not None and got.value == "v2"


def test_get_if_not_modified_with_wrong_etag_raises_412(client, uniq):
    # Recordings: GET with If-Match: "wrong etag" -> 412.
    key = uniq()
    client.set_configuration_setting(ConfigurationSetting(key=key, value="v"))
    with pytest.raises(ResourceModifiedError) as exc:
        client.get_configuration_setting(key, etag="wrong etag", match_condition=MatchConditions.IfNotModified)
    assert status_of(exc) == 412


def test_get_if_not_modified_with_current_etag_returns_setting(client, uniq):
    key = uniq()
    s = client.set_configuration_setting(ConfigurationSetting(key=key, value="v"))
    got = client.get_configuration_setting(key, etag=s.etag, match_condition=MatchConditions.IfNotModified)
    assert got.value == "v"


def test_get_accept_datetime_before_the_setting_existed_is_404(client, uniq):
    key = uniq()
    client.set_configuration_setting(ConfigurationSetting(key=key, value="v"))
    long_ago = datetime(2000, 4, 1, 9, 30, 45, tzinfo=timezone.utc)
    with pytest.raises(ResourceNotFoundError):
        client.get_configuration_setting(key, accept_datetime=long_ago)


def test_get_accept_datetime_returns_the_value_at_that_time(client, uniq):
    # Revisions have one second resolution, so leave a second on each side of the probe time.
    key = uniq()
    client.set_configuration_setting(ConfigurationSetting(key=key, value="old"))
    time.sleep(1.1)
    probe = datetime.now(timezone.utc)
    time.sleep(1.1)
    client.set_configuration_setting(ConfigurationSetting(key=key, value="new"))
    assert client.get_configuration_setting(key, accept_datetime=probe).value == "old"
    assert client.get_configuration_setting(key).value == "new"


# --- delete ----------------------------------------------------------------


def test_delete_returns_the_deleted_setting(client, uniq):
    key = uniq()
    s = client.set_configuration_setting(ConfigurationSetting(key=key, value="v", tags={"a": "b"}))
    deleted = client.delete_configuration_setting(key)
    assert deleted is not None
    assert (deleted.key, deleted.value, deleted.tags, deleted.etag) == (key, "v", {"a": "b"}, s.etag)
    with pytest.raises(ResourceNotFoundError):
        client.get_configuration_setting(key)


def test_delete_missing_key_returns_none(client, uniq):
    # Recordings: DELETE of a missing key is 204, which the SDK reports as None.
    assert client.delete_configuration_setting(uniq()) is None


def test_delete_twice_second_returns_none(client, uniq):
    key = uniq()
    client.set_configuration_setting(ConfigurationSetting(key=key, value="v"))
    assert client.delete_configuration_setting(key) is not None
    assert client.delete_configuration_setting(key) is None


def test_delete_with_label_only_deletes_that_label(client, uniq):
    key = uniq()
    client.set_configuration_setting(ConfigurationSetting(key=key, value="plain"))
    client.set_configuration_setting(ConfigurationSetting(key=key, label="L", value="labelled"))
    deleted = client.delete_configuration_setting(key, label="L")
    assert deleted.label == "L"
    assert client.get_configuration_setting(key).value == "plain"
    with pytest.raises(ResourceNotFoundError):
        client.get_configuration_setting(key, label="L")


def test_delete_wrong_label_deletes_nothing(client, uniq):
    key = uniq()
    client.set_configuration_setting(ConfigurationSetting(key=key, label="A", value="v"))
    assert client.delete_configuration_setting(key, label="B") is None
    assert client.get_configuration_setting(key, label="A").value == "v"


def test_delete_if_not_modified_with_stale_etag_raises_412_and_keeps_setting(client, uniq):
    key = uniq()
    client.set_configuration_setting(ConfigurationSetting(key=key, value="v"))
    with pytest.raises(ResourceModifiedError) as exc:
        client.delete_configuration_setting(key, etag="wrong etag", match_condition=MatchConditions.IfNotModified)
    assert status_of(exc) == 412
    assert client.get_configuration_setting(key).value == "v"


def test_delete_if_not_modified_with_current_etag_deletes(client, uniq):
    key = uniq()
    s = client.set_configuration_setting(ConfigurationSetting(key=key, value="v"))
    deleted = client.delete_configuration_setting(key, etag=s.etag, match_condition=MatchConditions.IfNotModified)
    assert deleted is not None
    with pytest.raises(ResourceNotFoundError):
        client.get_configuration_setting(key)


def test_recreate_after_delete_gets_a_new_etag(client, uniq):
    key = uniq()
    first = client.set_configuration_setting(ConfigurationSetting(key=key, value="v"))
    client.delete_configuration_setting(key)
    second = client.add_configuration_setting(ConfigurationSetting(key=key, value="v"))
    assert second.etag != first.etag


# --- values, content type, tags --------------------------------------------


def test_empty_string_value_round_trips_as_empty_string(client, uniq):
    key = uniq()
    client.set_configuration_setting(ConfigurationSetting(key=key, value=""))
    assert client.get_configuration_setting(key).value == ""


def test_long_value_round_trips(client, uniq):
    key = uniq()
    value = "0123456789abcdef" * 500  # 8000 characters
    client.set_configuration_setting(ConfigurationSetting(key=key, value=value))
    assert client.get_configuration_setting(key).value == value


def test_multiline_json_value_is_stored_verbatim(client, uniq):
    key = uniq()
    value = '{\n  "a": [1, 2, 3],\n  "b": "q\\"uote",\n  "c": null\n}\n'
    client.set_configuration_setting(
        ConfigurationSetting(key=key, value=value, content_type="application/json")
    )
    assert client.get_configuration_setting(key).value == value


def test_content_type_round_trips(client, uniq):
    key = uniq()
    client.set_configuration_setting(ConfigurationSetting(key=key, value="v", content_type="application/x-custom; charset=utf-8"))
    assert client.get_configuration_setting(key).content_type == "application/x-custom; charset=utf-8"


def test_many_tags_round_trip(client, uniq):
    key = uniq()
    tags = {f"tag{i}": f"value{i}" for i in range(10)}
    client.set_configuration_setting(ConfigurationSetting(key=key, value="v", tags=tags))
    assert client.get_configuration_setting(key).tags == tags
