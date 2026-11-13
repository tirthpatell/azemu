"""Blob service properties through ARM (`blobServices/default`): versioning, CORS, delete retention.

Versioning is checked with the Blob SDK (`x-ms-version-id` on a write). On Azure it is off until it is turned on, so an
account made through ARM starts with it off; an account ARM never made follows the emulator's default, which is off as
well (on with AZEMU_BLOB_VERSIONING=1 or AZEMU_LENIENT=1: the tests marked `versioning` run in the runner's second pass).
"""

import pytest
from azure.mgmt.storage import StorageManagementClient
from azure.storage.blob import BlobServiceClient

from conftest import SUB


@pytest.fixture
def storage(cred, mgmt):
    with StorageManagementClient(cred, SUB, **mgmt) as c:
        yield c


def make_account(storage, rg, name):
    acct = storage.storage_accounts.begin_create(rg, name, {"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}).result()
    key = storage.storage_accounts.list_keys(rg, name)["keys"][0].value
    return acct, BlobServiceClient(acct.primary_endpoints.blob, credential=key)


def write_headers(service, container, name, data=b"x"):
    seen = {}
    service.get_container_client(container).upload_blob(name, data, overwrite=True, raw_response_hook=lambda r: seen.update({k.lower(): v for k, v in r.http_response.headers.items()}))
    return seen


def props_path(rg, acct):
    return f"/subscriptions/{SUB}/resourceGroups/{rg}/providers/Microsoft.Storage/storageAccounts/{acct}/blobServices/default"


def test_versioning_is_off_for_a_new_account_and_follows_arm(storage, rg, uniq):
    name = uniq("veracct")
    acct, blobs = make_account(storage, rg, name)
    assert storage.blob_services.get_service_properties(rg, name).is_versioning_enabled is False
    blobs.create_container("cont")
    assert "x-ms-version-id" not in write_headers(blobs, "cont", "a.txt")

    on = storage.blob_services.set_service_properties(rg, name, {"properties": {"isVersioningEnabled": True}})
    assert on.is_versioning_enabled is True
    assert storage.blob_services.get_service_properties(rg, name).is_versioning_enabled is True
    h1 = write_headers(blobs, "cont", "a.txt", b"one")
    h2 = write_headers(blobs, "cont", "a.txt", b"two")
    assert h1["x-ms-version-id"] and h2["x-ms-version-id"] and h1["x-ms-version-id"] != h2["x-ms-version-id"]

    storage.blob_services.set_service_properties(rg, name, {"properties": {"isVersioningEnabled": False}})
    assert storage.blob_services.get_service_properties(rg, name).is_versioning_enabled is False
    assert "x-ms-version-id" not in write_headers(blobs, "cont", "a.txt", b"three")
    # the versions that exist stay
    versions = [b for b in blobs.get_container_client("cont").list_blobs(include=["versions"]) if b.name == "a.txt"]
    assert len(versions) >= 2


@pytest.mark.no_versioning
def test_account_not_made_through_arm_has_no_versioning_by_default(storage, rg, emu, uniq):
    name = uniq("neverarm")
    blobs = BlobServiceClient(emu.url("blob", name), credential="c2VjcmV0c2VjcmV0c2VjcmV0")
    blobs.create_container("cont")
    assert "x-ms-version-id" not in write_headers(blobs, "cont", "a.txt")


@pytest.mark.versioning
def test_account_not_made_through_arm_follows_the_emulator_setting_when_it_is_on(storage, rg, emu, uniq):
    """With AZEMU_BLOB_VERSIONING=1 (or AZEMU_LENIENT=1) an account ARM never made keeps versions; one ARM made does not."""
    name = uniq("neverarm")
    blobs = BlobServiceClient(emu.url("blob", name), credential="c2VjcmV0c2VjcmV0c2VjcmV0")
    blobs.create_container("cont")
    assert "x-ms-version-id" in write_headers(blobs, "cont", "a.txt")
    made = uniq("madearm")
    _, via_arm = make_account(storage, rg, made)
    via_arm.create_container("cont")
    assert "x-ms-version-id" not in write_headers(via_arm, "cont", "a.txt")


def test_other_properties_are_stored_and_returned(storage, rg, uniq, raw):
    name = uniq("propacct")
    make_account(storage, rg, name)
    body = {
        "properties": {
            "containerDeleteRetentionPolicy": {"enabled": True, "days": 9},
            "changeFeed": {"enabled": True, "retentionInDays": 30},
            "lastAccessTimeTrackingPolicy": {"enable": True, "name": "AccessTimeTracking", "trackingGranularityInDays": 1, "blobType": ["blockBlob"]},
            "restorePolicy": {"enabled": False},
            "automaticSnapshotPolicyEnabled": False,
        }
    }
    status, got = raw("PUT", props_path(rg, name), body)
    assert status == 200, got
    p = got["properties"]
    assert p["containerDeleteRetentionPolicy"] == {"enabled": True, "days": 9}
    assert p["changeFeed"] == {"enabled": True, "retentionInDays": 30}
    assert p["lastAccessTimeTrackingPolicy"]["trackingGranularityInDays"] == 1
    assert got["name"] == "default"
    assert got["type"] == "Microsoft.Storage/storageAccounts/blobServices"
    # a property the request leaves out keeps its value; the one it names is replaced
    status, got = raw("PUT", props_path(rg, name), {"properties": {"isVersioningEnabled": True}})
    assert status == 200
    p = got["properties"]
    assert p["isVersioningEnabled"] is True
    assert p["changeFeed"] == {"enabled": True, "retentionInDays": 30}
    assert storage.blob_services.get_service_properties(rg, name).change_feed.retention_in_days == 30


def test_cors_and_delete_retention_reach_the_blob_service(storage, rg, uniq):
    name = uniq("corsacct")
    acct, blobs = make_account(storage, rg, name)
    out = storage.blob_services.set_service_properties(
        rg, name,
        {
            "properties": {
                "cors": {"corsRules": [{"allowedOrigins": ["https://example.com", "https://b.example.com"], "allowedMethods": ["GET", "PUT"], "maxAgeInSeconds": 120, "exposedHeaders": ["x-ms-request-id"], "allowedHeaders": ["x-ms-meta-*"]}]},
                "deleteRetentionPolicy": {"enabled": True, "days": 5},
            }
        },
    )
    assert out.cors.cors_rules[0].allowed_methods == ["GET", "PUT"]
    assert out.delete_retention_policy.enabled is True
    assert out.delete_retention_policy.days == 5
    # the Blob SDK sees the same settings on the data plane
    live = blobs.get_service_properties()
    rule = live["cors"][0]
    # the Blob SDK shows each list as the comma separated text the service holds
    assert rule.allowed_origins == "https://example.com,https://b.example.com"
    assert rule.allowed_methods == "GET,PUT"
    assert rule.max_age_in_seconds == 120
    assert rule.exposed_headers == "x-ms-request-id"
    assert rule.allowed_headers == "x-ms-meta-*"
    assert live["delete_retention_policy"].enabled is True
    assert live["delete_retention_policy"].days == 5
    # and removing them through ARM removes them from the service
    storage.blob_services.set_service_properties(rg, name, {"properties": {"cors": {"corsRules": []}, "deleteRetentionPolicy": {"enabled": False}}})
    live = blobs.get_service_properties()
    assert live["cors"] == []
    assert live["delete_retention_policy"].enabled is False


def test_bad_cors_is_refused(storage, rg, uniq, raw):
    name = uniq("badcors")
    make_account(storage, rg, name)
    rules = [{"allowedOrigins": ["*"], "allowedMethods": ["GET"], "maxAgeInSeconds": 1, "exposedHeaders": [], "allowedHeaders": []}] * 6
    status, got = raw("PUT", props_path(rg, name), {"properties": {"cors": {"corsRules": rules}}})
    assert status == 400, got
    status, got = raw("PUT", props_path(rg, name), {"properties": {"isVersioningEnabled": "yes"}})
    assert status == 400, got
    status, got = raw("PUT", props_path(rg, name).replace("/default", "/other"), {"properties": {}})
    assert status == 400, got
