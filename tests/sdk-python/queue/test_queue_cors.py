"""CORS: rules set through the SDK, then browser-style requests (OPTIONS preflight and actual requests with
Origin) sent as raw HTTP, which is what a web page does.

Expectations from Microsoft Learn, "Cross-Origin Resource Sharing (CORS) support for Azure Storage":
a matching rule answers the preflight 200 with the Access-Control-* headers; no rule (or no CORS at all) is
403 CorsPreflightFailure; a preflight without Origin or Access-Control-Request-Method is 400; an actual request
with Origin gets Access-Control-Allow-Origin (and the exposed headers) when a rule matches. The rules are
evaluated in order and the first one that matches origin and method is the only one that counts.
"""

import pytest
import requests
from azure.storage.queue import CorsRule

from queue_support import DEV_ACCOUNT


@pytest.fixture
def base(emu):
    return f"http://127.0.0.1:{emu.port}", {"Host": f"{DEV_ACCOUNT}.queue.core.localhost:{emu.port}"}


def preflight(base, path, origin, method, headers=None, *, with_origin=True, with_method=True):
    url, host = base
    h = dict(host)
    if with_origin:
        h["Origin"] = origin
    if with_method:
        h["Access-Control-Request-Method"] = method
    if headers:
        h["Access-Control-Request-Headers"] = headers
    return requests.options(url + path, headers=h, timeout=10)


def rule(origins, methods, allowed_headers, exposed_headers, max_age):
    return CorsRule(origins, methods, max_age_in_seconds=max_age, allowed_headers=allowed_headers, exposed_headers=exposed_headers)


def test_no_rules_preflight_is_403_cors_preflight_failure(base):
    r = preflight(base, "/myqueue/messages", "http://contoso.example", "GET")
    assert r.status_code == 403
    assert r.headers["x-ms-error-code"] == "CorsPreflightFailure"
    assert "Access-Control-Allow-Origin" not in r.headers


def test_preflight_matches_rules_set_through_the_sdk(base, service):
    service.set_service_properties(
        cors=[
            rule(["http://www.contoso.com"], ["PUT", "HEAD"], ["x-ms-blob-content-type"], ["x-ms-*"], 5),
            rule(["*"], ["PUT", "GET"], ["x-ms-blob-content-type"], ["x-ms-*"], 7),
            rule(["http://www.contoso.com"], ["GET"], ["x-ms-client-request-id"], ["x-ms-*"], 9),
        ]
    )
    # the three requests of the documentation's table; the queue need not exist (the preflight is about the service)
    r = preflight(base, "/anyqueue/messages", "http://www.contoso.com", "PUT", "x-ms-blob-content-type")
    assert r.status_code == 200
    assert r.headers["Access-Control-Allow-Origin"] == "http://www.contoso.com"
    assert r.headers["Access-Control-Allow-Methods"] == "PUT"
    assert r.headers["Access-Control-Allow-Headers"] == "x-ms-blob-content-type"
    assert r.headers["Access-Control-Max-Age"] == "5"
    r = preflight(base, "/anyqueue/messages", "http://www.contoso.com", "GET", "x-ms-blob-content-type")
    assert r.status_code == 200
    assert r.headers["Access-Control-Allow-Origin"] == "*"
    assert r.headers["Access-Control-Max-Age"] == "7"
    r = preflight(base, "/anyqueue/messages", "http://www.contoso.com", "GET", "x-ms-client-request-id")
    assert r.status_code == 403
    assert r.headers["x-ms-error-code"] == "CorsPreflightFailure"


def test_preflight_without_required_headers_is_400(base, service):
    service.set_service_properties(cors=[rule(["*"], ["GET"], ["*"], [], 0)])
    r = preflight(base, "/q", "", "GET", with_origin=False)
    assert r.status_code == 400
    assert r.headers["x-ms-error-code"] == "MissingRequiredHeader"
    r = preflight(base, "/q", "http://a.example", "", with_method=False)
    assert r.status_code == 400
    assert r.headers["x-ms-error-code"] == "MissingRequiredHeader"


def test_removing_the_rules_ends_cors(base, service):
    service.set_service_properties(cors=[rule(["*"], ["GET"], ["*"], [], 0)])
    assert preflight(base, "/q", "http://a.example", "GET").status_code == 200
    service.set_service_properties(cors=[])
    assert preflight(base, "/q", "http://a.example", "GET").status_code == 403


def test_actual_request_with_origin_gets_cors_headers(base, service, queue, queue_name):
    service.set_service_properties(
        cors=[rule(["http://app.contoso.com"], ["GET", "POST"], ["*"], ["x-ms-request-id", "x-ms-meta-*"], 60)]
    )
    url, host = base
    auth = {"Authorization": "SharedKey devstoreaccount1:Sanitized", "x-ms-date": "Fri, 27 Mar 2026 01:00:51 GMT"}
    r = requests.get(f"{url}/{queue_name}?comp=metadata", headers={**host, **auth, "Origin": "http://app.contoso.com"}, timeout=10)
    assert r.status_code == 200
    assert r.headers["Access-Control-Allow-Origin"] == "http://app.contoso.com"
    assert r.headers["Access-Control-Expose-Headers"].replace(" ", "") == "x-ms-request-id,x-ms-meta-*"
    assert r.headers["Vary"] == "Origin"
    # an origin that no rule allows: the request works, the CORS headers are absent
    r = requests.get(f"{url}/{queue_name}?comp=metadata", headers={**host, **auth, "Origin": "http://evil.example"}, timeout=10)
    assert r.status_code == 200
    assert "Access-Control-Allow-Origin" not in r.headers
    # no Origin: not a CORS request
    r = requests.get(f"{url}/{queue_name}?comp=metadata", headers={**host, **auth}, timeout=10)
    assert "Access-Control-Allow-Origin" not in r.headers
