"""File service properties: CORS rules and preflight, the shape of what a new account reports, and what the service refuses.

Only CORS is changed (and put back): metrics and the SMB settings are left as they are on a real account.
"""

import time

import pytest
from azure.core.exceptions import HttpResponseError
from azure.storage.fileshare import CorsRule

import fileutil
from fileutil import expect


@pytest.fixture
def clean_cors(service):
    yield
    service.set_service_properties(cors=[])


def test_a_new_account_has_no_cors_rules(service):
    props = service.get_service_properties()
    assert props["cors"] == []


def test_cors_rules_round_trip(service, clean_cors):
    rule = CorsRule(["http://a.example", "https://b.example"], ["GET", "PUT"], allowed_headers=["x-ms-meta-*", "Content-Type"],
                    exposed_headers=["x-ms-*"], max_age_in_seconds=60)
    service.set_service_properties(cors=[rule])
    got = service.get_service_properties()["cors"]
    assert len(got) == 1
    assert sorted(got[0].allowed_origins.split(",")) == ["http://a.example", "https://b.example"]
    assert sorted(got[0].allowed_methods.split(",")) == ["GET", "PUT"]
    assert got[0].max_age_in_seconds == 60 and got[0].exposed_headers == "x-ms-*"


def test_cors_preflight_and_response_headers(emu, service, share, clean_cors):
    service.set_service_properties(cors=[CorsRule(["http://a.example"], ["GET"], exposed_headers=["x-ms-*"], max_age_in_seconds=60)])
    raw = fileutil.Raw(emu)
    # Azure takes a few seconds to apply a new rule
    for _ in range(20):
        pre = raw.call("OPTIONS", f"/{share.share_name}", [("restype", "share")],
                       {"Origin": "http://a.example", "Access-Control-Request-Method": "GET"}, sign=False)
        if pre.status_code == 200 or not emu.azure:
            break
        time.sleep(3)
    assert pre.status_code == 200
    assert pre.headers["Access-Control-Allow-Origin"] == "http://a.example"
    assert pre.headers["Access-Control-Allow-Methods"] == "GET"
    assert pre.headers["Access-Control-Max-Age"] == "60"
    r = raw.call("GET", f"/{share.share_name}", [("restype", "share")], {"Origin": "http://a.example"})
    assert r.status_code == 200
    assert r.headers["Access-Control-Allow-Origin"] == "http://a.example"
    assert "x-ms-request-id" in r.headers["Access-Control-Expose-Headers"]
    other = raw.call("OPTIONS", f"/{share.share_name}", [("restype", "share")],
                     {"Origin": "http://nope.example", "Access-Control-Request-Method": "GET"}, sign=False)
    assert other.status_code == 403


def test_more_than_five_cors_rules(service, clean_cors):
    rules = [CorsRule([f"http://{i}.example"], ["GET"]) for i in range(6)]
    with pytest.raises(HttpResponseError) as e:
        service.set_service_properties(cors=rules)
    assert e.value.status_code == 400


def test_the_logging_section_is_not_for_files(emu):
    raw = fileutil.Raw(emu)
    body = "<StorageServiceProperties><Logging><Version>1.0</Version></Logging></StorageServiceProperties>"
    r = raw.call("PUT", "/", [("restype", "service"), ("comp", "properties")], {"Content-Type": "application/xml"}, body)
    # checked against Azure in November 2026
    assert r.status_code == 400 and r.headers["x-ms-error-code"] == "InvalidXmlDocument"
    assert "<Reason>Element Logging not supported for File service.</Reason>" in fileutil.text(r)
    assert "<LineNumber>1</LineNumber><LinePosition>28</LinePosition>" in fileutil.text(r)


def test_service_properties_shape(emu):
    raw = fileutil.Raw(emu)
    r = raw.call("GET", "/", [("restype", "service"), ("comp", "properties")])
    assert r.status_code == 200
    body = fileutil.text(r).lstrip("\ufeff")
    assert body.startswith('<?xml version="1.0" encoding="utf-8"?><StorageServiceProperties><HourMetrics>')
    assert "<ProtocolSettings><SMB><Multichannel><Enabled>" in body


def test_stats_are_not_for_the_file_service(emu):
    raw = fileutil.Raw(emu)
    r = raw.call("GET", "/", [("restype", "service"), ("comp", "stats")])
    assert r.status_code == 400 and r.headers["x-ms-error-code"] == "InvalidQueryParameterValue"
