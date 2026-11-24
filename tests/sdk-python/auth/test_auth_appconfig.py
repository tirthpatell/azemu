"""App Configuration with Entra tokens: 403 and 401 with no body at all (what Azure answered in November 2026, `docs/strict-auth.md`)."""

import pytest
import requests
from azure.appconfiguration import AzureAppConfigurationClient, ConfigurationSetting
from azure.core.exceptions import ClientAuthenticationError, HttpResponseError, ResourceNotFoundError

from authutil import unique

API = "api-version=2023-11-01"


def client(w, who):
    return AzureAppConfigurationClient(w.appconfig_url(), who, **w.sdk_kw())


@pytest.fixture(scope="module")
def key(world, main):
    k = unique("k")
    client(world, main).set_configuration_setting(ConfigurationSetting(key=k, value="v1"))
    yield k
    try:
        client(world, main).delete_configuration_setting(k)
    except Exception:
        pass


def raw_get(w, who, path, token=None):
    t = token if token is not None else who.token(f"{w.appconfig_url()}/.default")
    return requests.get(f"{w.appconfig_url()}{path}{'&' if '?' in path else '?'}{API}", headers={"Authorization": f"Bearer {t}"}, timeout=60, verify=w.emu.ca_file if w.emu else True)


def test_norole_gets_403_with_no_body_and_no_content_type_for_everything(world, norole, key):
    for path in (f"/kv/{key}", f"/kv/{unique('nothere')}", "/kv", "/keys", "/labels", "/snapshots", "/revisions"):
        r = raw_get(world, norole, path)
        assert r.status_code == 403, path
        assert r.content == b""
        assert "content-type" not in r.headers and "www-authenticate" not in r.headers
    c = client(world, norole)
    for call in (lambda: c.get_configuration_setting(key), lambda: list(c.list_configuration_settings()), lambda: c.set_configuration_setting(ConfigurationSetting(key=unique("k"), value="x"))):
        with pytest.raises(HttpResponseError) as e:
            call()
        assert e.value.status_code == 403
        assert "Operation returned an invalid status 'Forbidden'" in str(e.value)


def test_data_reader_reads_everything_and_a_missing_key_is_404(world, reader, key):
    c = client(world, reader)
    assert c.get_configuration_setting(key).value == "v1"
    assert key in [s.key for s in c.list_configuration_settings()]
    assert len(list(c.list_revisions(key_filter=key))) >= 1
    with pytest.raises(ResourceNotFoundError):
        c.get_configuration_setting(unique("nothere"))
    for path in ("/keys", "/labels", "/snapshots", "/revisions"):
        assert raw_get(world, reader, path).status_code == 200, path


def test_data_reader_may_not_write_delete_or_lock(world, reader, key):
    c = client(world, reader)
    for call in (
        lambda: c.set_configuration_setting(ConfigurationSetting(key=key, value="x")),
        lambda: c.set_configuration_setting(ConfigurationSetting(key=unique("k"), value="x")),
        lambda: c.delete_configuration_setting(key),
        lambda: c.set_read_only(ConfigurationSetting(key=key, value="v1"), True),
    ):
        with pytest.raises(HttpResponseError) as e:
            call()
        assert e.value.status_code == 403


def test_a_token_that_is_wrong_is_401_with_the_challenge(world, main, key):
    good = main.token(f"{world.appconfig_url()}/.default")
    cases = {
        "audience": main.token("https://vault.azure.net/.default"),
        "signature": good[:-4] + ("AAAA" if not good.endswith("AAAA") else "BBBB"),
        "garbage": "not-a-token",
    }
    for name, token in cases.items():
        r = raw_get(world, main, f"/kv/{key}", token=token)
        assert r.status_code == 401, name
        assert r.content == b""
        assert r.headers["www-authenticate"] == 'HMAC-SHA256, Bearer error="invalid_token", error_description="Authorization token failed validation"', name
    r = requests.get(f"{world.appconfig_url()}/kv/{key}?{API}", timeout=60, verify=world.emu.ca_file if world.emu else True)
    assert r.status_code == 401 and r.headers["www-authenticate"] == "HMAC-SHA256, Bearer"
    from azure.appconfiguration import AzureAppConfigurationClient as C

    class Bad:
        def get_token(self, *a, **k):
            from azure.core.credentials import AccessToken

            return AccessToken(cases["audience"], 4102444800)

    with pytest.raises(ClientAuthenticationError):
        C(world.appconfig_url(), Bad(), **world.sdk_kw()).get_configuration_setting(key)
