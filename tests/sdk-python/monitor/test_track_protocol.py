"""The Application Insights ingestion endpoint ("Breeze") with raw requests: the answers, the errors and the partial success.

These tests run on both sides: against the emulator, and against a real Application Insights resource with
`AZEMU_AZURE=1` (set `AZ_APPINSIGHTS_CONN_STR`, a connection string with an instrumentation key; without an endpoint in
it the global one, `dc.services.visualstudio.com`, is used). Every message below was recorded from Azure in November 2026.
Telemetry sent to a real resource is kept by it (named `azemu-probe`); there is no way to take it back, so run these rarely.
Tests that check what only the emulator has are `emulator_only`; tests about what Azure answers differently are
`azure_differs`.
"""

import gzip
import json
import os
import uuid
import urllib.error
import urllib.request
from datetime import datetime, timedelta, timezone

import pytest

import monitorutil as mu

# these tests send bad input on purpose
pytestmark = pytest.mark.expects_refusals


class Target:
    """Where the requests go: the emulator, or the resource named by `AZ_APPINSIGHTS_CONN_STR`."""

    def __init__(self, emu):
        self.azure = emu.azure
        if self.azure:
            parts = dict(p.split("=", 1) for p in os.environ["AZ_APPINSIGHTS_CONN_STR"].split(";") if "=" in p)
            parts = {k.lower(): v for k, v in parts.items()}
            self.ikey = parts["instrumentationkey"]
            self.ingest = parts.get("ingestionendpoint", "https://dc.services.visualstudio.com").rstrip("/")
            self.live = parts.get("liveendpoint", "https://rt.services.visualstudio.com").rstrip("/")
        else:
            self.ikey = mu.IKEY
            self.ingest = self.live = f"http://127.0.0.1:{emu.port}"

    def send(self, method, url, body=None, headers=None):
        """(status, headers, body bytes) whatever the status."""
        data = body
        h = dict(headers or {})
        if body is not None and not isinstance(body, (bytes, bytearray)):
            data = json.dumps(body).encode()
            h.setdefault("Content-Type", "application/json")
        req = urllib.request.Request(url, data=data, method=method, headers=h)
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                return r.status, r.headers, r.read()
        except urllib.error.HTTPError as e:
            return e.code, e.headers, e.read()

    def track(self, body, headers=None, path="/v2/track"):
        status, h, raw = self.send("POST", self.ingest + path, body, headers)
        return status, h, json.loads(raw) if raw else None

    def envelope(self, base_type="EventData", drop=(), **over):
        e = {
            "ver": 1,
            "name": "Microsoft.ApplicationInsights.Event",
            "time": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f0Z"),
            "sampleRate": 100.0,
            "iKey": self.ikey,
            "tags": {"ai.operation.id": "azemuprobe" + uuid.uuid4().hex[:8], "ai.cloud.role": "azemu-probe"},
            "data": {"baseType": base_type, "baseData": {"ver": 2, "name": "azemu-probe"}},
        }
        e.update(over)
        for d in drop:
            e.pop(d, None)
        return e


@pytest.fixture
def target(emu):
    return Target(emu)


def first_error(body):
    return body["errors"][0]["message"]


def test_a_good_batch(target):
    status, headers, body = target.track([target.envelope(), target.envelope()])
    assert status == 200
    assert body == {"itemsReceived": 2, "itemsAccepted": 2, "appId": None, "errors": []}
    assert headers["Content-Type"] == "application/json; charset=utf-8"
    assert headers["X-Content-Type-Options"] == "nosniff"


def test_other_forms_of_the_body(target):
    gz = gzip.compress(json.dumps([target.envelope()]).encode())
    assert target.track(gz, {"Content-Type": "application/json", "Content-Encoding": "gzip"})[2]["itemsAccepted"] == 1
    lines = "\n".join(json.dumps(target.envelope()) for _ in range(2)).encode()
    assert target.track(lines, {"Content-Type": "application/x-json-stream"})[2]["itemsAccepted"] == 2
    assert target.track(target.envelope())[2]["itemsAccepted"] == 1, "one object, no array"
    # text that is not JSON, and values that are not objects, are skipped
    assert target.track(b"this is not json")[:1] == (200,)
    assert target.track(b"this is not json")[2]["itemsReceived"] == 0
    assert target.track([1, 2])[2]["itemsReceived"] == 0
    for empty in (b"", b"[]"):
        assert target.track(empty)[2] == {"itemsReceived": 0, "itemsAccepted": 0, "appId": None, "errors": []}


def test_partial_success_is_206_with_the_index_of_each_refused_item(target):
    old = target.envelope(time=(datetime.now(timezone.utc) - timedelta(hours=100)).strftime("%Y-%m-%dT%H:%M:%S.%f0Z"))
    status, _, body = target.track([target.envelope(), target.envelope(drop=("name",)), target.envelope(), old])
    assert status == 206
    assert (body["itemsReceived"], body["itemsAccepted"]) == (4, 2)
    assert [(e["index"], e["statusCode"]) for e in body["errors"]] == [(1, 400), (3, 400)]
    assert body["errors"][0]["message"] == "109: Field 'name' on type 'Envelope' is required but missing or empty. Expected: string"
    assert body["errors"][1]["message"].startswith("103: Field 'time' on type 'Envelope' is older than the allowed min date.")


@pytest.mark.parametrize(
    "mutate, message",
    [
        (lambda e: e.pop("name"), "109: Field 'name' on type 'Envelope' is required but missing or empty. Expected: string"),
        (lambda e: e.update(name=""), "109: Field 'name' on type 'Envelope' is required but missing or empty. Expected: string"),
        (lambda e: e.pop("time"), "109: Field 'time' on type 'Envelope' is required but missing or empty. Expected: date"),
        (lambda e: e.update(time="yesterday"), "102: Field 'time' on type 'Envelope' is not a valid time string. Expected: date"),
        (lambda e: e.update(time=12345), "106: Field 'time' on type 'Envelope' is of incorrect type. Expected: date"),
        (lambda e: e.update(sampleRate="x"), "106: Field 'sampleRate' on type 'Envelope' is of incorrect type. Expected: double"),
        (lambda e: e.update(tags="x"), "106: Field 'tags' on type 'Envelope' is of incorrect type. Expected: object"),
        (lambda e: e.update(ver="x"), "EnvelopeSchemaVersionUnsupported: Envelope Schema version 'x' is not supported."),
        (lambda e: e.pop("iKey"), "Invalid instrumentation key"),
        (lambda e: e.update(iKey="abc"), "Invalid instrumentation key"),
        (lambda e: e.update(iKey=5), "Invalid instrumentation key"),
        (lambda e: e.pop("data"), "InvalidJsonObject: Not a valid JSON object"),
        (lambda e: e.update(data="x"), "InvalidJsonObject: Not a valid JSON object"),
        (lambda e: e["data"].pop("baseData"), "InvalidJsonObject: Not a valid JSON object"),
        (lambda e: e["data"].pop("baseType"), 'Unknown domain type ""'),
        (lambda e: e["data"].update(baseType="NopeData"), 'Unknown domain type "NopeData"'),
        (lambda e: e["data"].update(baseType="eventdata"), 'Unknown domain type "eventdata"'),
        (lambda e: e["data"]["baseData"].pop("name"), "109: Field 'name' on type 'EventData' is required but missing or empty. Expected: string"),
        (lambda e: e["data"]["baseData"].update(properties="x"), "106: Field 'properties' on type 'EventData' is of incorrect type. Expected: object"),
    ],
)
def test_an_envelope_that_is_refused(target, mutate, message):
    e = target.envelope()
    mutate(e)
    status, _, body = target.track([e])
    assert status == 400
    assert (body["itemsReceived"], body["itemsAccepted"]) == (1, 0)
    assert body["errors"] == [{"index": 0, "statusCode": 400, "message": message}]


def test_the_first_problem_in_azures_order_is_the_one_reported(target):
    old = (datetime.now(timezone.utc) - timedelta(hours=100)).strftime("%Y-%m-%dT%H:%M:%S.%f0Z")
    # key, then data, then name, then time
    assert first_error(target.track([target.envelope(drop=("iKey", "name"), time=old)])[2]) == "Invalid instrumentation key"
    assert first_error(target.track([target.envelope(drop=("data", "name"), time=old)])[2]) == "InvalidJsonObject: Not a valid JSON object"
    assert first_error(target.track([target.envelope(drop=("name",), time=old)])[2]).startswith("109: Field 'name'")


def test_time_window(target):
    def at(delta):
        return (datetime.now(timezone.utc) + delta).strftime("%Y-%m-%dT%H:%M:%S.%f0Z")

    assert target.track([target.envelope(time=at(-timedelta(hours=47)))])[0] == 200
    assert target.track([target.envelope(time=at(timedelta(minutes=110)))])[0] == 200
    status, _, body = target.track([target.envelope(time=at(-timedelta(hours=49)))])
    assert (status, first_error(body)) == (400, "103: Field 'time' on type 'Envelope' is older than the allowed min date. Expected: now - 172800000ms")
    status, _, body = target.track([target.envelope(time=at(timedelta(hours=3)))])
    assert (status, first_error(body)) == (400, "104: Field 'time' on type 'Envelope' is newer than the allowed max date. Expected: now + 7200000ms")
    # no fraction, no zone: taken
    now = datetime.now(timezone.utc)
    for t in (now.strftime("%Y-%m-%dT%H:%M:%SZ"), now.strftime("%Y-%m-%dT%H:%M:%S")):
        assert target.track([target.envelope(time=t)])[0] == 200


def test_any_form_of_a_guid_is_a_key(target):
    k = target.ikey
    for form in (k.upper(), k.replace("-", ""), "{" + k + "}"):
        assert target.track([target.envelope(iKey=form)])[0] == 200, form


@pytest.mark.parametrize(
    "base_type, base, message",
    [
        ("RequestData", {}, "109: Field 'id' on type 'RequestData' is required but missing or empty. Expected: string"),
        ("RequestData", {"id": "1"}, "109: Field 'duration' on type 'RequestData' is required but missing or empty. Expected: timespan"),
        ("RequestData", {"id": "1", "duration": "abc"}, "108: Field 'duration' on type 'RequestData' is not a valid duration string. Expected: dd.hh:mm:ss.fffffff format not exceeding 1000 days"),
        ("RequestData", {"id": "1", "duration": "00:00:01"}, "109: Field 'responseCode' on type 'RequestData' is required but missing or empty. Expected: string"),
        ("RequestData", {"id": "1", "duration": "00:00:01", "responseCode": "200", "success": "yes"}, "106: Field 'success' on type 'RequestData' is of incorrect type. Expected: boolean"),
        ("RemoteDependencyData", {}, "109: Field 'name' on type 'RemoteDependencyData' is required but missing or empty. Expected: string"),
        ("PageViewData", {}, "109: Field 'name' on type 'PageViewData' is required but missing or empty. Expected: string"),
        ("AvailabilityData", {}, "109: Field 'duration' on type 'AvailabilityData' is required but missing or empty. Expected: timespan"),
        ("MessageData", {}, "109: Field 'message' on type 'MessageData' is required but missing or empty. Expected: string"),
        ("MessageData", {"message": "m", "severityLevel": 9}, "107: Field 'severityLevel' on type 'MessageData' contains invalid enum value. Expected: one of 0,1,2,3,4"),
        ("ExceptionData", {}, "106: Field 'exceptions' on type 'ExceptionData' is of incorrect type. Expected: array"),
        ("ExceptionData", {"exceptions": [{}]}, "109: Field 'typeName' on type 'ExceptionDetails' is required but missing or empty. Expected: string"),
        ("ExceptionData", {"exceptions": [{"typeName": "T"}]}, "109: Field 'message' on type 'ExceptionDetails' is required but missing or empty. Expected: string"),
        ("MetricData", {}, "106: Field 'metrics' on type 'MetricData' is of incorrect type. Expected: array"),
        ("MetricData", {"metrics": [{}]}, "109: Field 'name' on type 'DataPoint' is required but missing or empty. Expected: string"),
        ("MetricData", {"metrics": [{"name": "m"}]}, "106: Field 'value' on type 'DataPoint' is of incorrect type. Expected: double"),
    ],
)
def test_what_each_type_needs(target, base_type, base, message):
    e = target.envelope(base_type)
    e["data"]["baseData"] = base
    status, _, body = target.track([e])
    assert (status, first_error(body)) == (400, message)


def test_what_each_type_accepts(target):
    ok = [
        ("RequestData", {"id": "1", "duration": "0.00:00:00.012", "responseCode": "200"}),
        ("RequestData", {"id": "1", "duration": "00:00:01", "responseCode": 200, "success": True}),
        ("RemoteDependencyData", {"name": "x"}),
        ("PageViewData", {"name": "x"}),
        ("AvailabilityData", {"duration": "00:00:01"}),
        ("MessageData", {"message": "m", "severityLevel": "Warning"}),
        ("EventData", {"name": "e", "properties": {"a": 5}}),
        ("ExceptionData", {"exceptions": []}),
        ("ExceptionData", {"exceptions": [{"typeName": "T", "message": "m"}]}),
        # (a MetricData with an empty list is not here: Azure took it alone and answered 500 for it inside a batch)
        ("MetricData", {"metrics": [{"name": "m", "value": 1}]}),
    ]
    envelopes = []
    for base_type, base in ok:
        e = target.envelope(base_type)
        e["data"]["baseData"] = base
        envelopes.append(e)
    status, _, body = target.track(envelopes)
    assert (status, body["itemsAccepted"], body["errors"]) == (200, len(ok), [])


def test_a_body_that_cannot_be_read(target):
    status, _, body = target.track(b'[{"name":"x"')
    assert status == 400
    assert body["errors"] == [{"index": 0, "statusCode": 400, "message": "Invalid JSON specified in the request"}]
    status, _, body = target.track(b"plain text", {"Content-Type": "application/json", "Content-Encoding": "gzip"})
    assert (status, body) == (400, {"message": "Malformed content", "statusCode": 400})


def test_an_item_that_is_too_long(target):
    e = target.envelope()
    e["data"]["baseData"]["properties"] = {"p": "x" * 1_300_000}
    status, _, body = target.track([e])
    assert (status, first_error(body)) == (400, "Telemetry item length must not exceed 1258291")


def test_other_methods(target):
    for method in ("GET", "PUT"):
        status, headers, raw = target.send(method, target.ingest + "/v2/track")
        assert (status, headers["Allow"], raw) == (405, "POST", b"")
    status, headers, raw = target.send("OPTIONS", target.ingest + "/v2/track", None, {"Origin": "https://example.com", "Access-Control-Request-Method": "POST"})
    assert status == 204
    assert headers["Access-Control-Allow-Origin"] == "*"
    assert headers["Access-Control-Allow-Methods"] == "POST"
    status, headers, _ = target.send("POST", target.ingest + "/v2/track", [target.envelope()], {"Origin": "https://example.com"})
    assert (status, headers["Access-Control-Allow-Origin"]) == (200, "*")


def test_the_application_id_lookup(target):
    status, headers, raw = target.send("GET", f"{target.ingest}/api/profiles/{target.ikey}/appId")
    assert status == 200
    assert headers["Content-Type"] == "text/plain; charset=utf-8"
    assert uuid.UUID(raw.decode().strip())
    assert target.send("GET", f"{target.ingest}/api/profiles/abc/appId")[::2] == (404, b"Profile Not Found")
    status, headers, raw = target.send("POST", f"{target.ingest}/api/profiles/{target.ikey}/appId", b"")
    assert (status, headers["Allow"], raw) == (405, "GET", b"")


def qps_headers():
    return {
        "x-ms-qps-transmission-time": "1",
        "x-ms-qps-stream-id": uuid.uuid4().hex,
        "x-ms-qps-machine-name": "m",
        "x-ms-qps-instance-name": "i",
        "x-ms-qps-role-name": "r",
        "x-ms-qps-invariant-version": "1",
    }


def test_live_metrics_ping_and_post_say_nobody_is_watching(target):
    for query in (f"ikey={target.ikey}", f"api-version=2024-04-01-preview&ikey={target.ikey}"):
        status, headers, raw = target.send("POST", f"{target.live}/QuickPulseService.svc/ping?{query}", b"", qps_headers())
        assert (status, headers["x-ms-qps-subscribed"], raw) == (200, "false", b"")
        status, headers, raw = target.send("POST", f"{target.live}/QuickPulseService.svc/ping?{query}", {"InvariantVersion": 1, "StreamId": "s"})
        assert (status, headers["x-ms-qps-subscribed"], raw) == (200, "false", b"")
        status, headers, raw = target.send("POST", f"{target.live}/QuickPulseService.svc/post?{query}", [], {"x-ms-qps-transmission-time": "1"})
        assert (status, headers["x-ms-qps-subscribed"], raw) == (200, "false", b"")


def test_live_metrics_calls_azure_cannot_read(target):
    status, _, raw = target.send("POST", f"{target.live}/QuickPulseService.svc/ping", b"", qps_headers())
    body = json.loads(raw)
    assert (status, body["Code"], body["Message"]) == (400, "InvalidInput", "iKey is garbage or not in a valid GUID format")
    status, _, raw = target.send("POST", f"{target.live}/QuickPulseService.svc/ping?ikey={target.ikey}", b"")
    body = json.loads(raw)
    assert (status, body["Code"], body["Message"]) == (400, "InvalidOperation", "Failed to parse monitoring data points from the ping request.")
    status, _, raw = target.send("POST", f"{target.live}/QuickPulseService.svc/post?ikey={target.ikey}", b"zzz")
    body = json.loads(raw)
    assert (status, body["Code"], body["Message"]) == (400, "InvalidOperation", "Failed to parse monitoring data points.")


# -- where the emulator cannot do what Azure does -------------------------------------------------------------------
@pytest.mark.azure_differs("Azure knows which keys exist; the emulator takes every GUID")
def test_a_key_nobody_owns_is_taken_by_the_emulator(target):
    status, _, body = target.track([target.envelope(iKey=str(uuid.uuid4()))])
    assert (status, body["itemsAccepted"]) == (200, 1)
    status, _, raw = target.send("GET", f"{target.ingest}/api/profiles/{uuid.uuid4()}/appId")
    assert status == 200 and uuid.UUID(raw.decode().strip())


@pytest.mark.azure_differs("Azure refuses a bearer token it cannot check (`Authorization not supported`); the emulator takes any")
def test_any_bearer_token_is_taken(target):
    status, _, body = target.track([target.envelope()], {"Authorization": "Bearer garbage", "Content-Type": "application/json"})
    assert (status, body["itemsAccepted"]) == (200, 1)


@pytest.mark.azure_differs("Azure answers 404 or 401; the emulator names what is not built with 501")
def test_what_is_not_built(emu, target):
    # a host that names the service (the path alone would be "no service matched")
    status, _, raw = target.send("GET", emu.url("in", "eastus") + "/something/else")
    assert status == 501
    assert json.loads(raw)["error"]["code"] == "NotImplemented"


@pytest.mark.emulator_only
def test_what_was_accepted_can_be_read_back(emu, target):
    op = "readback" + uuid.uuid4().hex[:8]
    e = target.envelope()
    e["tags"]["ai.operation.id"] = op
    assert target.track([e, target.envelope(drop=("name",))])[0] == 206
    items = mu.telemetry(emu, operationId=op)
    assert [i["type"] for i in items] == ["EventData"]
    assert items[0]["envelope"]["tags"]["ai.cloud.role"] == "azemu-probe"
