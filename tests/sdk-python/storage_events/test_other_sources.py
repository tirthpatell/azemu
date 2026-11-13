"""Key Vault and App Configuration events (smaller than the storage ones)."""

import time
import uuid
from datetime import datetime, timezone

import seutil as su
from azure.appconfiguration import AzureAppConfigurationClient, ConfigurationSetting
from azure.core.credentials import AccessToken, AccessTokenInfo
from azure.keyvault.keys import KeyClient
from azure.keyvault.secrets import SecretClient


class Credential:
    def get_token(self, *scopes, **kwargs):
        return AccessToken("azemu-test-token", int(time.time()) + 3600)

    def get_token_info(self, *scopes, options=None):
        return AccessTokenInfo("azemu-test-token", int(time.time()) + 3600)


def vault_kwargs(emu):
    kw = {}
    if emu.ca_file:
        kw["connection_verify"] = emu.ca_file
    return kw


def vault_sub(emu, hook, vault, name="kv", **extra):
    return su.admin(emu, "PUT", f"/sources/keyvault/{vault}/subscriptions/{name}", {"webhookUrl": hook.url, **extra}, expect=[200, 201])[1]


def test_secret_new_version_created(emu, hook):
    vault = "evvault"
    vault_sub(emu, hook, vault)
    client = SecretClient(emu.https_url("vault", vault), Credential(), **vault_kwargs(emu))
    nbf = datetime(2030, 1, 1, tzinfo=timezone.utc)
    exp = datetime(2031, 1, 1, tzinfo=timezone.utc)
    first = client.set_secret("newsecret", "v1", not_before=nbf, expires_on=exp)
    second = client.set_secret("newsecret", "v2")
    events = hook.wait_events(2)
    by_version = {e["data"]["Version"]: e for e in events}
    assert set(by_version) == {first.properties.version, second.properties.version}
    ev = by_version[first.properties.version]
    # Learn "Azure Key Vault as Event Grid source", SecretNewVersionCreated
    assert set(ev) == {"id", "topic", "subject", "eventType", "eventTime", "data", "dataVersion", "metadataVersion"}
    assert ev["eventType"] == "Microsoft.KeyVault.SecretNewVersionCreated"
    assert ev["topic"].endswith(f"/providers/Microsoft.KeyVault/vaults/{vault}")
    assert ev["subject"] == "newsecret"
    assert ev["dataVersion"] == "1" and ev["metadataVersion"] == "1"
    d = ev["data"]
    assert d["Id"] == first.id
    assert (d["VaultName"], d["ObjectType"], d["ObjectName"]) == (vault, "Secret", "newsecret")
    assert d["NBF"] == str(int(nbf.timestamp())) and d["EXP"] == str(int(exp.timestamp()))
    assert "NBF" not in by_version[second.properties.version]["data"]  # no dates on that version


def test_key_new_version_created_by_create_and_rotate(emu, hook):
    vault = "evvault2"
    vault_sub(emu, hook, vault, filter={"includedEventTypes": ["Microsoft.KeyVault.KeyNewVersionCreated"]})
    client = KeyClient(emu.https_url("vault", vault), Credential(), **vault_kwargs(emu))
    key = client.create_rsa_key("mykey", size=2048)
    events = hook.wait_events(1)
    assert events[0]["data"]["ObjectType"] == "Key" and events[0]["data"]["ObjectName"] == "mykey"
    assert events[0]["data"]["Version"] == key.properties.version
    assert events[0]["subject"] == "mykey"
    # reads, updates and deletes are not new versions
    client.get_key("mykey")
    client.update_key_properties("mykey", enabled=True)
    time.sleep(0.4)
    assert len(hook.events) == 1


def test_vault_events_only_for_subscribed_vault(emu, hook):
    vault_sub(emu, hook, "subscribed")
    other = SecretClient(emu.https_url("vault", "unsubscribed"), Credential(), **vault_kwargs(emu))
    other.set_secret("s", "v")
    time.sleep(0.4)
    assert hook.events == []


def test_app_configuration_key_value_modified_and_deleted(emu, hook):
    store = "evstore"
    su.admin(emu, "PUT", f"/sources/appconfig/{store}/subscriptions/ac", {"webhookUrl": hook.url}, expect=[200, 201])
    client = AzureAppConfigurationClient.from_connection_string(f"Endpoint={emu.url('azconfig', store)};Id=azemu-id;Secret=YXplbXUtc2VjcmV0")
    saved = client.set_configuration_setting(ConfigurationSetting(key="Foo", label="FizzBuzz", value="1"))
    client.set_configuration_setting(ConfigurationSetting(key="NoLabel", value="2"))
    client.delete_configuration_setting(key="Foo", label="FizzBuzz")
    events = hook.wait_events(3)
    by_type = {}
    for e in events:
        by_type.setdefault((e["eventType"], e["data"]["key"]), e)
    mod = by_type[("Microsoft.AppConfiguration.KeyValueModified", "Foo")]
    assert mod["topic"].endswith(f"/providers/Microsoft.AppConfiguration/configurationStores/{store}")
    assert mod["subject"].endswith("/kv/Foo?label=FizzBuzz")
    assert mod["data"]["label"] == "FizzBuzz"
    assert mod["data"]["etag"] == saved.etag.strip('"')
    assert mod["data"]["syncToken"]
    assert set(mod["data"]) == {"key", "label", "etag", "syncToken"}
    nolabel = by_type[("Microsoft.AppConfiguration.KeyValueModified", "NoLabel")]
    assert nolabel["data"]["label"] is None
    assert nolabel["subject"].endswith("/kv/NoLabel")
    assert ("Microsoft.AppConfiguration.KeyValueDeleted", "Foo") in by_type
