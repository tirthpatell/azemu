"""Event Grid custom topic with Entra tokens: every refusal is 401, also a missing role (`docs/strict-auth.md`, Azure in November 2026)."""

import json
import re

import pytest
import requests
from azure.core.exceptions import ClientAuthenticationError
from azure.core.messaging import CloudEvent
from azure.eventgrid import EventGridEvent, EventGridPublisherClient

SCOPE = "https://eventgrid.azure.net/.default"


@pytest.fixture(autouse=True)
def _trust_the_emulator(world, monkeypatch):
    # the Basic client does not pass `connection_verify` on: the CA goes in through the environment
    if not world.azure:
        monkeypatch.setenv("REQUESTS_CA_BUNDLE", world.emu.ca_file)


def publisher(w, who):
    return EventGridPublisherClient(w.eventgrid_url(), who, retry_total=0)


def event():
    return EventGridEvent(subject="auth/test", event_type="Azemu.Auth.Test", data={"n": 1}, data_version="1.0")


def raw_publish(w, token):
    body = [{"id": "1", "subject": "auth/test", "eventType": "Azemu.Auth.Test", "eventTime": "2026-11-24T00:00:00Z", "data": {"n": 1}, "dataVersion": "1.0"}]
    return requests.post(
        f"{w.eventgrid_url()}?api-version=2018-01-01",
        data=json.dumps(body),
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
        timeout=60,
        verify=w.emu.ca_file if w.emu else True,
    )


def text_of(r):
    return re.sub(r" Report '.*", "", r.json()["error"]["message"])


def test_a_sender_publishes(world, main):
    publisher(world, main).send([event()])


@pytest.mark.parametrize("who", ["norole", "reader", "scoped"])
def test_every_other_principal_is_401_not_403(world, who):
    p = world.principals[who]
    r = raw_publish(world, p.token(SCOPE))
    assert r.status_code == 401
    assert "www-authenticate" not in r.headers
    assert re.fullmatch(
        r"The principal associated with access token presented with the incoming request does not have permission to send data to /subscriptions/[^/]+/resourceGroups/[^/]+/providers/Microsoft\.EventGrid/topics/[^/]+\.",
        text_of(r),
    ), text_of(r)
    assert r.json()["error"]["code"] == "Unauthorized"
    assert r.json()["error"]["details"][0]["code"] == "Unauthorized"
    with pytest.raises(ClientAuthenticationError) as e:
        publisher(world, p).send([event()])
    assert e.value.status_code == 401


def test_a_token_that_is_wrong_is_401_with_its_text(world, main):
    good = main.token(SCOPE)
    wrong_audience = raw_publish(world, main.token("https://vault.azure.net/.default"))
    assert text_of(wrong_audience) == "Invalid access token. Please ensure that the token is not expired and has the right audience."
    bad_sig = raw_publish(world, good[:-4] + ("AAAA" if not good.endswith("AAAA") else "BBBB"))
    assert text_of(bad_sig) == "Invalid access token. Please ensure that the token is not expired and has the right audience."
    garbage = raw_publish(world, "not-a-token")
    assert text_of(garbage) == "Unable to read access token."
    for r in (wrong_audience, bad_sig, garbage):
        assert r.status_code == 401
