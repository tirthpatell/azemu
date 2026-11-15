"""A custom topic that has access keys compares what a publisher sends with them, as Azure does (tried in November 2026).
The keys are given to the emulator's admin API (a topic made through Resource Manager has its `listKeys` keys: `tests/sdk-python/arm`);
a topic without keys takes any key. A bearer token is never looked at."""

import base64
import datetime
import uuid

import pytest
import requests
from azure.core.credentials import AzureKeyCredential, AzureSasCredential
from azure.core.exceptions import ClientAuthenticationError, HttpResponseError
from azure.eventgrid import EventGridEvent, EventGridPublisherClient, generate_sas

pytestmark = pytest.mark.emulator_only  # the keys are given through the emulator's admin API

KEY1 = base64.b64encode(b"key-one-0123456789012345678901234").decode()
KEY2 = base64.b64encode(b"key-two-0123456789012345678901234").decode()


@pytest.fixture
def keyed(admin, emu):
    name = "k" + uuid.uuid4().hex[:10]
    admin.put_topic(name, {"inputSchema": "EventGridSchema", "keys": [KEY1, KEY2]}, expect=(201,))
    return name, emu.url("eventgrid", name) + "/api/events"


def _event():
    return EventGridEvent(subject="s", event_type="t", data={"a": 1}, data_version="1")


def test_the_keys_of_the_topic_are_taken_and_others_are_not(keyed):
    name, endpoint = keyed
    for key in (KEY1, KEY2):
        EventGridPublisherClient(endpoint, AzureKeyCredential(key), retry_total=0).send(_event())
    with pytest.raises(ClientAuthenticationError) as exc:
        EventGridPublisherClient(endpoint, AzureKeyCredential("d3JvbmdrZXk="), retry_total=0).send(_event())
    assert exc.value.status_code == 401
    assert "The request authorization key is not authorized for" in exc.value.message


def test_a_signed_token_made_with_a_key(keyed):
    name, endpoint = keyed
    expires = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)
    token = generate_sas(endpoint, KEY1, expires)
    EventGridPublisherClient(endpoint, AzureSasCredential(token), retry_total=0).send(_event())
    bad = generate_sas(endpoint, KEY1, expires - datetime.timedelta(hours=2))
    with pytest.raises(ClientAuthenticationError):
        EventGridPublisherClient(endpoint, AzureSasCredential(bad), retry_total=0).send(_event())
    other = generate_sas(endpoint, base64.b64encode(b"not-the-key-0123456789012345678").decode(), expires)
    with pytest.raises(ClientAuthenticationError) as exc:
        EventGridPublisherClient(endpoint, AzureSasCredential(other), retry_total=0).send(_event())
    assert "Invalid signature" in exc.value.message


def test_a_version_that_is_not_2018_01_01_is_refused(keyed):
    name, endpoint = keyed
    for v, status in (("2018-01-01", 200), ("banana", 200), ("2099-01-01", 400), ("2019-06-01", 400)):
        r = requests.post(
            endpoint,
            params={"api-version": v},
            json=[{"id": "1", "subject": "s", "eventType": "t", "eventTime": "2026-11-14T00:00:00Z", "dataVersion": "1"}],
            headers={"aeg-sas-key": KEY1},
            timeout=10,
        )
        assert r.status_code == status, (v, r.text)
        if status == 400:
            assert r.json()["error"]["code"] == "UnsupportedApiVersion"


def test_an_event_of_more_than_about_a_megabyte_is_413(keyed):
    name, endpoint = keyed
    client = EventGridPublisherClient(endpoint, AzureKeyCredential(KEY1), retry_total=0)
    client.send(EventGridEvent(subject="s", event_type="t", data="x" * 1_000_000, data_version="1"))
    with pytest.raises(HttpResponseError) as exc:
        client.send(EventGridEvent(subject="s", event_type="t", data="x" * 1_100_000, data_version="1"))
    assert exc.value.status_code == 413
    assert "The maximum size for the JSON content (1049600) has been exceeded." in exc.value.message
