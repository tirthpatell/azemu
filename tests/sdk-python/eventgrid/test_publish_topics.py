"""Publishing to custom topics with the sync EventGridPublisherClient (Event Grid Basic).

Shapes follow the recordings of azure-eventgrid: EventGridEvent, CloudEvent, dicts, custom
schema. A successful publish returns None (HTTP 200, empty body).
"""

import base64
import datetime
import json
import uuid

import pytest
import requests
from azure.core.credentials import AzureKeyCredential, AzureSasCredential
from azure.core.messaging import CloudEvent
from azure.eventgrid import EventGridEvent, EventGridPublisherClient, generate_sas

from egutil import FakeToken


def eg_event(**kw):
    args = dict(subject="sample", event_type="Sample.EventGrid.Event", data={"sample": "eventgridevent"}, data_version="2.0")
    args.update(kw)
    return EventGridEvent(**args)


# -- Event Grid schema ------------------------------------------------------------------

def test_send_event_grid_event_data_dict(publisher):
    assert publisher.send(eg_event()) is None


def test_send_event_grid_event_data_str(publisher):
    assert publisher.send(eg_event(data="eventgridevent")) is None


def test_send_event_grid_event_list(publisher):
    publisher.send([eg_event(subject="sample"), eg_event(subject="sample2", data="eventgridevent2")])


def test_send_event_grid_event_dict(publisher):
    publisher.send(
        {
            "id": str(uuid.uuid4()),
            "subject": "sample",
            "data": {"key1": "Sample.EventGrid.Event"},
            "eventType": "Sample.EventGrid.Event",
            "dataVersion": "2.0",
            "eventTime": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ"),
        }
    )


def test_send_event_grid_event_bytes_data_is_rejected_by_the_sdk(publisher):
    with pytest.raises(TypeError):
        publisher.send(eg_event(data=b"bytes"))


def test_send_many_event_grid_events(publisher):
    publisher.send([eg_event(subject=f"s{i}", data={"i": i}) for i in range(50)])


# -- CloudEvents ------------------------------------------------------------------------

def test_send_cloud_event_data_str(publisher):
    publisher.send(CloudEvent(source="/source", type="Sample.Cloud.Event", data="cloudevent"))


def test_send_cloud_event_data_dict(publisher):
    publisher.send(CloudEvent(source="/source", type="Sample.Cloud.Event", data={"sample": "cloudevent"}))


def test_send_cloud_event_data_none(publisher):
    publisher.send(CloudEvent(source="/source", type="Sample.Cloud.Event"))


def test_send_cloud_event_data_bytes(publisher):
    publisher.send(CloudEvent(source="/source", type="Sample.Cloud.Event", data=b"cloudevent"))


def test_send_cloud_event_data_with_extensions(publisher):
    publisher.send(CloudEvent(source="/source", type="Sample.Cloud.Event", data="cloudevent", extensions={"reasoncode": 204, "extension": "hello"}))


def test_send_cloud_event_list(publisher):
    publisher.send([CloudEvent(source="/s", type="t", data="a"), CloudEvent(source="/s", type="t", data="b")])


def test_send_cloud_event_dict(publisher):
    publisher.send({"id": "id", "source": "/source", "specversion": "1.0", "data": "cloudevent", "type": "Sample.Cloud.Event"})


def test_send_cloud_event_dict_with_data_base64(publisher):
    publisher.send(
        {"id": "id", "source": "/source", "specversion": "1.0", "type": "t", "data_base64": base64.b64encode(b"hello").decode()}
    )


def test_send_cloud_event_with_all_optional_attributes(publisher):
    publisher.send(
        CloudEvent(
            source="/source",
            type="t",
            data={"a": 1},
            subject="sub",
            dataschema="https://example.com/schema",
            datacontenttype="application/json",
            time=datetime.datetime.now(datetime.timezone.utc),
        )
    )


# -- custom schema ----------------------------------------------------------------------

CUSTOM = {
    "customSubject": "sample",
    "customEventType": "sample.event",
    "customDataVersion": "2.0",
    "customId": "1234",
    "customEventTime": "2024-05-23T22:06:55.749434+00:00",
    "customData": "sample data",
}


def test_send_custom_schema_event(publisher):
    publisher.send(CUSTOM)


def test_send_custom_schema_event_list(publisher):
    publisher.send([CUSTOM, {**CUSTOM, "customSubject": "sample2", "customId": "5678"}])


def test_custom_schema_accepts_arbitrary_shapes(publisher):
    publisher.send([{"a": 1}, {"nested": {"b": [1, 2, 3]}, "c": None}])


# -- credentials and endpoints ----------------------------------------------------------

def test_sas_credential(emu, topic):
    endpoint = emu.url("eventgrid", topic) + "/api/events"
    sas = generate_sas(endpoint, "c2VjcmV0", datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1))
    with EventGridPublisherClient(endpoint, AzureSasCredential(sas), retry_total=0) as c:
        c.send(eg_event())


def test_token_credential_over_https(emu, topic, monkeypatch):
    # The Basic client does not pass `connection_verify` on, so trust the CA through the environment.
    monkeypatch.setenv("REQUESTS_CA_BUNDLE", emu.ca_file)
    endpoint = emu.https_url("eventgrid", topic) + "/api/events"
    with EventGridPublisherClient(endpoint, FakeToken()) as c:
        c.send(eg_event())


def test_key_credential_over_https(emu, topic, key, monkeypatch):
    monkeypatch.setenv("REQUESTS_CA_BUNDLE", emu.ca_file)
    endpoint = emu.https_url("eventgrid", topic) + "/api/events"
    with EventGridPublisherClient(endpoint, key) as c:
        c.send([CloudEvent(source="/s", type="t", data="x")])


def test_regional_host_names_work_like_the_real_ones(emu, topic, key):
    """Real hosts look like {topic}.{region}-1.eventgrid.azure.net: only the first two labels count."""
    endpoint = f"http://{topic}.eventgrid.localhost:{emu.port}/api/events"
    publisher = EventGridPublisherClient(endpoint, key, retry_total=0)
    publisher.send(eg_event())


def test_partner_channel_name(publisher):
    publisher.send(CloudEvent(source="/s", type="t", data="x"), channel_name="mychannel")


def test_sending_with_a_context_manager_and_after_close(emu, topic, key):
    endpoint = emu.url("eventgrid", topic) + "/api/events"
    with EventGridPublisherClient(endpoint, key) as c:
        c.send(eg_event())


def test_none_credential_is_a_client_side_error(emu, topic):
    with pytest.raises(ValueError):
        EventGridPublisherClient(emu.url("eventgrid", topic) + "/api/events", None)


def test_unsupported_credential_type_is_a_client_side_error(emu, topic):
    with pytest.raises(ValueError):
        EventGridPublisherClient(emu.url("eventgrid", topic) + "/api/events", "a string")


# -- raw HTTP: what the service answers ---------------------------------------------------

def _raw(emu, topic, body, headers=None, params=None):
    h = {"Content-Type": "application/json; charset=utf-8", "aeg-sas-key": "c2VjcmV0"}
    h.update(headers or {})
    return requests.post(
        emu.url("eventgrid", topic) + "/api/events",
        params=params if params is not None else {"api-version": "2018-01-01"},
        data=json.dumps(body) if not isinstance(body, (str, bytes)) else body,
        headers=h,
        timeout=10,
    )


def test_success_response_headers_and_empty_body(emu, topic):
    r = _raw(emu, topic, [{"id": "1", "subject": "s", "eventType": "t", "eventTime": "2024-05-23T22:06:35Z", "dataVersion": "1", "data": {}}])
    assert r.status_code == 200
    assert r.content == b""
    assert r.headers["api-supported-versions"] == "2018-01-01"
    assert r.headers["Server"] == "Microsoft-HTTPAPI/2.0"
    assert r.headers["Strict-Transport-Security"] == "max-age=31536000; includeSubDomains"


@pytest.mark.parametrize(
    "headers",
    [
        {"aeg-sas-key": "c2VjcmV0"},
        {"aeg-sas-key": "", "aeg-sas-token": "r=x&e=2030-01-01&s=sig"},
        {"aeg-sas-key": "", "Authorization": "SharedAccessSignature r=x&e=2030-01-01&s=sig"},
        {"aeg-sas-key": "", "Authorization": "Bearer eyJhbGciOi.fake.token"},
    ],
)
def test_all_credential_forms_are_accepted(emu, topic, headers):
    r = _raw(emu, topic, [{"customId": "1"}], headers=headers)
    assert r.status_code == 200, r.text


def test_request_without_any_credential_is_401(emu, topic):
    r = requests.post(
        emu.url("eventgrid", topic) + "/api/events",
        params={"api-version": "2018-01-01"},
        json=[{"customId": "1"}],
        timeout=10,
    )
    assert r.status_code == 401
    body = r.json()
    assert body["error"]["code"] == "Unauthorized"
    assert body["error"]["details"][0]["code"] == "Unauthorized"
    assert "not authorized" in body["error"]["message"]


def test_publish_is_idempotent_for_resending_the_same_event_id(publisher):
    e = eg_event()
    publisher.send(e)
    publisher.send(e)  # Event Grid does not de-duplicate; both are accepted
