"""Key Vault and App Configuration: azure-core's retry policy waits for the header (`Retry-After`, `retry-after-ms`) of a 429."""

import pytest
from azure.appconfiguration import AzureAppConfigurationClient, ConfigurationSetting
from azure.core.exceptions import HttpResponseError, ServiceResponseError

import kvutil
from appconfigutil import conn_string, uid
from faultutil import Stopwatch


def secret_client(emu, **kw):
    from azure.keyvault.secrets import SecretClient

    return SecretClient(kvutil.vault_url(emu, "faultvault"), kvutil.FakeCredential(), **kvutil.client_kwargs(emu), **kw)


def test_key_vault_waits_for_retry_after_and_then_succeeds(emu, faults):
    with secret_client(emu) as c:
        c.set_secret("s1", "v1")
        rule = faults.add(service="keyvault", method="GET", times=2, fault={"kind": "throttle", "retryAfterSeconds": 1})
        with Stopwatch() as t:
            assert c.get_secret("s1").value == "v1"
    assert faults.fired(rule["id"]) == 2
    assert 1.9 <= t.seconds < 6, f"two waits of 1 s: {t.seconds:.2f}"


def test_key_vault_with_retries_off_raises_the_429(emu, faults):
    with secret_client(emu, retry_total=0) as c:
        faults.add(service="keyvault", fault={"kind": "throttle", "retryAfterSeconds": 7})
        with pytest.raises(HttpResponseError) as e:
            c.get_secret("s1")
    assert e.value.status_code == 429
    assert e.value.error.code == "Throttled"
    assert e.value.response.headers["Retry-After"] == "7"


def test_key_vault_error_rule_and_a_dropped_connection(emu, faults):
    with secret_client(emu, retry_total=3, retry_backoff_factor=0.1) as c:
        c.set_secret("s1", "v1")
        faults.add(service="keyvault", method="GET", times=1, fault={"kind": "drop"})
        assert c.get_secret("s1").value == "v1", "the dropped connection is retried"
    with secret_client(emu, retry_total=0) as c:
        faults.add(service="keyvault", method="GET", fault={"kind": "error", "status": 500, "code": "InternalError", "message": "made up"})
        with pytest.raises(HttpResponseError) as e:
            c.get_secret("s1")
    assert (e.value.status_code, e.value.error.code, e.value.error.message) == (500, "InternalError", "made up")


def test_key_vault_latency_longer_than_the_timeout_fails(emu, faults):
    with secret_client(emu, retry_total=0, read_timeout=1) as c:
        c.set_secret("s1", "v1")
        faults.add(service="keyvault", method="GET", times=1, fault={"kind": "latency", "ms": 3000})
        with Stopwatch() as t:
            with pytest.raises(ServiceResponseError):
                c.get_secret("s1")
    assert t.seconds < 2.9, "the client gave up at its own timeout"


def appconfig_client(emu, store, **kw):
    return AzureAppConfigurationClient.from_connection_string(conn_string(emu.url("azconfig", store)), **kw)


def test_app_configuration_waits_for_retry_after_ms_and_then_succeeds(emu, faults):
    store = uid("store")
    with appconfig_client(emu, store, retry_total=5, retry_backoff_factor=0.1) as c:
        c.set_configuration_setting(ConfigurationSetting(key="k", value="v"))
        rule = faults.add(service="appconfig", account=store, method="GET", times=2, fault={"kind": "throttle", "retryAfterMs": 700})
        with Stopwatch() as t:
            assert c.get_configuration_setting("k").value == "v"
    assert faults.fired(rule["id"]) == 2
    assert 1.3 <= t.seconds < 6, f"two waits of 0.7 s: {t.seconds:.2f}"


def test_app_configuration_with_retries_off_raises_the_429_problem(emu, faults):
    store = uid("store")
    with appconfig_client(emu, store, retry_total=0) as c:
        faults.add(service="appconfig", account=store, fault={"kind": "throttle", "retryAfterMs": 40})
        with pytest.raises(HttpResponseError) as e:
            c.get_configuration_setting("k")
    assert e.value.status_code == 429
    assert e.value.response.headers["retry-after-ms"] == "40"
    assert e.value.response.headers["Content-Type"].startswith("application/problem+json")
    assert e.value.response.json()["type"] == "https://azconfig.io/errors/too-many-requests"


def test_app_configuration_rule_for_another_store_does_not_apply(emu, faults):
    store = uid("store")
    faults.add(service="appconfig", account="some-other-store", fault={"kind": "throttle"})
    with appconfig_client(emu, store, retry_total=0) as c:
        c.set_configuration_setting(ConfigurationSetting(key="k", value="v"))
        assert c.get_configuration_setting("k").value == "v"
