"""App Configuration state across restarts, through the official SDK: key-values, labels, revisions, locks,
snapshots and ETags."""

import pathlib
import sys

import pytest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "appconfig"))
from appconfigutil import conn_string  # noqa: E402

from azure.appconfiguration import AzureAppConfigurationClient, ConfigurationSetting, ConfigurationSettingsFilter  # noqa: E402
from azure.core import MatchConditions  # noqa: E402
from azure.core.exceptions import HttpResponseError, ResourceModifiedError, ResourceNotFoundError  # noqa: E402


def client(p, store="persiststore"):
    return AzureAppConfigurationClient.from_connection_string(conn_string(p.emu.url("azconfig", store)), retry_total=0)


@pytest.mark.parametrize("how", ["sigterm", "sigkill_after_save"])
def test_settings_revisions_locks_and_snapshots_survive(persistent, how):
    p = persistent()
    c = client(p)
    c.set_configuration_setting(ConfigurationSetting(key="app/a", value="a1", content_type="text/x", tags={"t": "1"}))
    c.set_configuration_setting(ConfigurationSetting(key="app/a", value="a2", tags={"t": "2"}))
    c.set_configuration_setting(ConfigurationSetting(key="app/b", label="prod", value="b1"))
    c.set_configuration_setting(ConfigurationSetting(key="app/b", label="dev", value="b-dev"))
    c.set_configuration_setting(ConfigurationSetting(key="gone", value="x"))
    c.delete_configuration_setting(key="gone")
    c.set_configuration_setting(ConfigurationSetting(key="locked", label="L", value="keep"))
    c.set_read_only(ConfigurationSetting(key="locked", label="L"), True)
    c.set_configuration_setting(ConfigurationSetting(key="日本語", label="ラベル", value="unicode"))
    c.begin_create_snapshot("snap1", [ConfigurationSettingsFilter(key="app/*")], retention_period=3600).wait()
    c.set_configuration_setting(ConfigurationSetting(key="app/a", value="a3-after-snapshot"))
    etag = c.get_configuration_setting("app/a").etag
    settings_before = {(s.key, s.label): (s.value, s.etag, s.read_only) for s in c.list_configuration_settings()}
    revisions_before = [(r.key, r.label, r.value, r.etag) for r in c.list_revisions()]
    other = client(p, "otherstore")
    other.set_configuration_setting(ConfigurationSetting(key="app/a", value="in the other store"))

    if how == "sigterm":
        assert p.stop() == 0
    else:
        p.save()
        p.stop(kill=True)
    p.start()

    c = client(p)
    assert {(s.key, s.label): (s.value, s.etag, s.read_only) for s in c.list_configuration_settings()} == settings_before
    assert [(r.key, r.label, r.value, r.etag) for r in c.list_revisions()] == revisions_before
    assert c.get_configuration_setting("日本語", label="ラベル").value == "unicode"
    with pytest.raises(ResourceNotFoundError):
        c.get_configuration_setting("gone")
    assert client(p, "otherstore").get_configuration_setting("app/a").value == "in the other store"
    # the lock still holds; an ETag condition from before the restart works
    with pytest.raises(HttpResponseError):
        c.set_configuration_setting(ConfigurationSetting(key="locked", label="L", value="no"))
    s = c.get_configuration_setting("app/a")
    s.value = "a4"
    c.set_configuration_setting(s, match_condition=MatchConditions.IfNotModified)
    stale = ConfigurationSetting(key="app/a", value="a5", etag=etag)
    with pytest.raises(ResourceModifiedError):
        c.set_configuration_setting(stale, match_condition=MatchConditions.IfNotModified)
    # the snapshot still holds the setting as it was
    snap = c.get_snapshot("snap1")
    assert snap.status == "ready" and snap.items_count == 1
    items = {(i.key, i.label): i.value for i in c.list_configuration_settings(snapshot_name="snap1")}
    assert items[("app/a", None)] == "a2"
