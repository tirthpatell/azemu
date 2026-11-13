"""Read-only (locked) settings: PUT/DELETE /locks/{key} and the 409 on writes."""

import pytest
from azure.appconfiguration import ConfigurationSetting, ResourceReadOnlyError
from azure.core import MatchConditions
from azure.core.exceptions import HttpResponseError, ResourceModifiedError, ResourceNotFoundError

from appconfigutil import KV_CT, PROBLEM_CT, status_of


def _locked(client, key, label=None, value="v"):
    s = client.set_configuration_setting(ConfigurationSetting(key=key, label=label, value=value))
    return client.set_read_only(s, True)


def test_set_read_only_true_returns_locked_setting(client, uniq):
    key = uniq()
    before = client.set_configuration_setting(ConfigurationSetting(key=key, value="v"))
    assert before.read_only is False
    locked = client.set_read_only(before, True)
    assert locked.read_only is True
    assert (locked.key, locked.value) == (key, "v")
    assert locked.etag != before.etag


def test_locked_state_is_visible_on_get(client, uniq):
    key = uniq()
    _locked(client, key)
    assert client.get_configuration_setting(key).read_only is True


def test_locked_state_is_visible_in_listing(client, uniq):
    key = uniq()
    _locked(client, key)
    [item] = list(client.list_configuration_settings(key_filter=key))
    assert item.read_only is True


def test_set_read_only_false_unlocks(client, uniq):
    key = uniq()
    locked = _locked(client, key)
    unlocked = client.set_read_only(locked, False)
    assert unlocked.read_only is False
    assert unlocked.etag != locked.etag
    assert client.get_configuration_setting(key).read_only is False


def test_set_on_locked_setting_raises_read_only_error_409(client, uniq):
    key = uniq()
    _locked(client, key, value="before")
    with pytest.raises(ResourceReadOnlyError) as exc:
        client.set_configuration_setting(ConfigurationSetting(key=key, value="after"))
    assert status_of(exc) == 409
    assert client.get_configuration_setting(key).value == "before"


def test_read_only_error_is_an_http_response_error(client, uniq):
    key = uniq()
    _locked(client, key)
    with pytest.raises(HttpResponseError) as exc:
        client.set_configuration_setting(ConfigurationSetting(key=key, value="after"))
    assert isinstance(exc.value, ResourceReadOnlyError)


def test_delete_on_locked_setting_raises_read_only_error_409(client, uniq):
    key = uniq()
    _locked(client, key)
    with pytest.raises(ResourceReadOnlyError) as exc:
        client.delete_configuration_setting(key)
    assert status_of(exc) == 409
    assert client.get_configuration_setting(key).read_only is True


def test_set_and_delete_work_again_after_unlock(client, uniq):
    key = uniq()
    locked = _locked(client, key, value="before")
    client.set_read_only(locked, False)
    client.set_configuration_setting(ConfigurationSetting(key=key, value="after"))
    assert client.get_configuration_setting(key).value == "after"
    assert client.delete_configuration_setting(key) is not None


def test_lock_applies_to_one_label_only(client, uniq):
    key = uniq()
    _locked(client, key, label="A")
    client.set_configuration_setting(ConfigurationSetting(key=key, label="B", value="free"))
    client.set_configuration_setting(ConfigurationSetting(key=key, value="free too"))
    with pytest.raises(ResourceReadOnlyError):
        client.set_configuration_setting(ConfigurationSetting(key=key, label="A", value="nope"))


def test_locked_setting_can_still_be_read(client, uniq):
    key = uniq()
    _locked(client, key, label="A", value="kept")
    assert client.get_configuration_setting(key, label="A").value == "kept"


def test_set_read_only_with_wrong_etag_raises_412(client, uniq):
    key = uniq()
    s = client.set_configuration_setting(ConfigurationSetting(key=key, value="v"))
    s.etag = "wrong etag"
    with pytest.raises(ResourceModifiedError) as exc:
        client.set_read_only(s, True, match_condition=MatchConditions.IfNotModified)
    assert status_of(exc) == 412
    assert client.get_configuration_setting(key).read_only is False


def test_unlock_with_wrong_etag_raises_412_and_stays_locked(client, uniq):
    key = uniq()
    locked = _locked(client, key)
    locked.etag = "wrong etag"
    with pytest.raises(ResourceModifiedError) as exc:
        client.set_read_only(locked, False, match_condition=MatchConditions.IfNotModified)
    assert status_of(exc) == 412
    assert client.get_configuration_setting(key).read_only is True


def test_set_read_only_with_current_etag_succeeds(client, uniq):
    key = uniq()
    s = client.set_configuration_setting(ConfigurationSetting(key=key, value="v"))
    locked = client.set_read_only(s, True, match_condition=MatchConditions.IfNotModified)
    assert locked.read_only is True


def test_set_read_only_on_missing_setting_raises_404(client, uniq):
    ghost = ConfigurationSetting(key=uniq(), value="v")
    with pytest.raises(ResourceNotFoundError) as exc:
        client.set_read_only(ghost, True)
    assert status_of(exc) == 404


# --- raw wire format (recordings: the 409 body is problem+json) -------------


def test_raw_409_on_locked_key_is_problem_json_with_key_locked_type(raw, uniq):
    key = uniq()
    assert raw.put_kv(key, value="v").status_code == 200
    assert raw.lock(key).status_code == 200
    r = raw.put_kv(key, value="w")
    assert r.status_code == 409
    assert r.headers["Content-Type"] == PROBLEM_CT
    body = r.json()
    assert body["type"] == "https://azconfig.io/errors/key-locked"
    assert body["status"] == 409
    assert body["name"] == key
    assert body["detail"] == "The key is read-only. To allow modification unlock it first."
    assert key in body["title"]


def test_raw_delete_on_locked_key_is_the_same_409(raw, uniq):
    key = uniq()
    raw.put_kv(key, value="v")
    raw.lock(key)
    r = raw.delete_kv(key)
    assert r.status_code == 409
    assert r.json()["type"] == "https://azconfig.io/errors/key-locked"


def test_raw_lock_and_unlock_return_the_setting_with_locked_flag(raw, uniq):
    key = uniq()
    raw.put_kv(key, value="v")
    locked = raw.lock(key)
    assert locked.status_code == 200
    assert locked.headers["Content-Type"] == KV_CT
    assert locked.json()["locked"] is True
    unlocked = raw.lock(key, delete=True)
    assert unlocked.status_code == 200
    assert unlocked.json()["locked"] is False
    assert locked.headers["ETag"] != unlocked.headers["ETag"]
