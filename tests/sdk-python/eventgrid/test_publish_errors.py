"""Publish errors, through the SDK and as raw HTTP."""

import json

import pytest
import requests
from azure.core.exceptions import ClientAuthenticationError, HttpResponseError, ResourceNotFoundError
from azure.core.messaging import CloudEvent
from azure.eventgrid import EventGridEvent, EventGridPublisherClient


def _post(emu, topic, body, content_type="application/json; charset=utf-8", headers=None):
    h = {"Content-Type": content_type, "aeg-sas-key": "c2VjcmV0"}
    h.update(headers or {})
    data = body if isinstance(body, (str, bytes)) else json.dumps(body)
    return requests.post(emu.url("eventgrid", topic) + "/api/events", params={"api-version": "2018-01-01"}, data=data, headers=h, timeout=10)


def test_large_payload_is_413_with_the_recorded_error(publisher):
    # the recording: a 2 MB request is refused with RequestEntityTooLarge / PayloadTooLarge
    big = {"type": "FeatureCollection", "features": ["x" * 1000] * 2100}
    with pytest.raises(HttpResponseError) as e:
        publisher.send(EventGridEvent(subject="sample", event_type="t", data=big, data_version="2.0"))
    assert e.value.status_code == 413
    assert "RequestEntityTooLarge" in str(e.value) or e.value.error.code == "RequestEntityTooLarge"


def test_large_payload_raw_body_shape(emu, topic):
    r = _post(emu, topic, [{"id": "1", "subject": "s", "eventType": "t", "eventTime": "2024-05-23T22:06:35Z", "dataVersion": "1", "data": "x" * 2_000_000}])
    assert r.status_code == 413
    err = r.json()["error"]
    assert err["code"] == "RequestEntityTooLarge"
    assert err["message"] == "The maximum size (1536000) has been exceeded."
    assert err["details"][0]["code"] == "PayloadTooLarge"


def test_missing_credential_maps_to_client_authentication_error(emu, topic, key):
    """A request without any credential header gets 401; the SDK reports ClientAuthenticationError."""
    endpoint = emu.url("eventgrid", topic) + "/api/events"
    client = EventGridPublisherClient(endpoint, key, retry_total=0)

    def strip_credentials(request):  # runs after the credential policy added its header
        for h in ("aeg-sas-token", "aeg-sas-key", "Authorization"):
            request.http_request.headers.pop(h, None)

    with pytest.raises(ClientAuthenticationError):
        client.send(
            EventGridEvent(subject="s", event_type="t", data="x", data_version="1"),
            raw_request_hook=strip_credentials,
        )


def test_endpoint_without_the_api_events_path_fails(emu, topic, key):
    """The Basic client needs the full URL, including /api/events (recorded: fails_without_full_url)."""
    client = EventGridPublisherClient(emu.url("eventgrid", topic), key, retry_total=0)
    with pytest.raises(HttpResponseError):
        client.send(EventGridEvent(subject="s", event_type="t", data="x", data_version="1"))


def test_event_grid_event_missing_required_property_is_400(emu, topic):
    r = _post(emu, topic, [{"id": "1", "eventType": "t", "eventTime": "2024-05-23T22:06:35Z", "dataVersion": "1", "data": {}}])
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["code"] == "BadRequest"
    assert "subject" in err["message"]
    assert "EventGridEvent" in err["message"]


def test_event_grid_event_bad_time_is_400(emu, topic):
    r = _post(emu, topic, [{"id": "1", "subject": "s", "eventType": "t", "eventTime": "not-a-time", "dataVersion": "1", "data": {}}])
    assert r.status_code == 400


def test_cloud_event_missing_source_is_400(emu, topic):
    r = _post(emu, topic, [{"specversion": "1.0", "id": "1", "type": "t"}], content_type="application/cloudevents-batch+json; charset=utf-8")
    assert r.status_code == 400
    assert "source" in r.json()["error"]["message"]


def test_cloud_event_wrong_specversion_is_400(emu, topic):
    r = _post(emu, topic, [{"specversion": "0.3", "id": "1", "source": "/s", "type": "t"}], content_type="application/cloudevents-batch+json")
    assert r.status_code == 400


def test_cloud_event_with_data_and_data_base64_is_400(emu, topic):
    ev = {"specversion": "1.0", "id": "1", "source": "/s", "type": "t", "data": "x", "data_base64": "eA=="}
    r = _post(emu, topic, [ev], content_type="application/cloudevents-batch+json")
    assert r.status_code == 400


def test_invalid_json_is_400(emu, topic):
    r = _post(emu, topic, "{this is not json")
    assert r.status_code == 400
    assert r.json()["error"]["code"] == "BadRequest"


def test_empty_array_is_400(emu, topic):
    r = _post(emu, topic, [], content_type="application/cloudevents-batch+json")
    assert r.status_code == 400


def test_error_bodies_are_json_with_error_code_and_message(emu, topic):
    r = _post(emu, topic, "[1, 2]")
    assert r.status_code == 400
    err = r.json()["error"]
    assert isinstance(err["code"], str) and isinstance(err["message"], str)


def test_sdk_raises_http_response_error_with_status_for_bad_request(emu, topic, key):
    client = EventGridPublisherClient(emu.url("eventgrid", topic) + "/api/events", key, retry_total=0)
    # an event dict that claims to be a CloudEvent but lacks `source` is sent as it is
    with pytest.raises(HttpResponseError) as e:
        client.send({"specversion": "1.0", "id": "1", "type": "t", "subject": "x"}, content_type="application/cloudevents-batch+json")
    assert e.value.status_code == 400


def test_operations_the_emulator_does_not_build_answer_501_naming_them(emu, topic):
    r = requests.delete(emu.url("eventgrid", topic) + "/api/events", headers={"aeg-sas-key": "x"}, timeout=10)
    assert r.status_code in (405, 501)
    r = requests.post(emu.url("eventgrid", topic) + "/topics/t/eventsubscriptions/s:frobnicate", headers={"aeg-sas-key": "x"}, timeout=10)
    assert r.status_code == 501
    assert "frobnicate" in r.json()["error"]["message"]
