"""Blob Storage events delivered to a webhook, in both schemas, with the filters."""

import json
import uuid

import pytest
from azure.storage.blob import BlobBlock, StandardBlobTier

import seutil as su


def sub_all(emu, hook, name="all", **extra):
    return su.subscribe(emu, name, {"webhookUrl": hook.url, **extra})


def test_put_blob_raises_blob_created(emu, hook, container, blob_service):
    view = sub_all(emu, hook)
    assert view["validation"]["status"] == "Succeeded"  # the handshake ran against the local webhook
    blob = container.get_blob_client("folder/new file.txt")
    resp = blob.upload_blob(b"hello world", content_settings=None, overwrite=True)

    (ev,) = hook.wait_events(1)
    su.check_eg_envelope(ev, "Microsoft.Storage.BlobCreated", su.subject_of(container.container_name, "folder/new file.txt"))
    d = ev["data"]
    assert set(d) == su.BLOB_CREATED_DATA, set(d) ^ su.BLOB_CREATED_DATA
    assert d["api"] == "PutBlob"
    assert d["eTag"] == resp["etag"].strip('"')  # the examples carry the ETag without quotes: "0x8D96060BDA19D9D"
    assert d["eTag"].startswith("0x")
    assert d["contentType"] == "application/octet-stream"
    assert d["contentLength"] == 11
    assert d["blobType"] == "BlockBlob"
    assert d["url"].startswith("http") and d["url"].endswith(f"/{container.container_name}/folder/new%20file.txt")
    assert f".blob." in d["url"]
    assert d["clientRequestId"]  # the SDK sends x-ms-client-request-id
    su.check_sequencer(d)


def test_delivery_headers_and_body_shape(emu, hook, container):
    sub_all(emu, hook, "Shape")
    container.upload_blob("a.txt", b"x")
    hook.wait_events(1)
    headers, raw = hook.requests[0]
    assert headers["aeg-event-type"] == "Notification"
    assert headers["aeg-subscription-name"] == "SHAPE"
    assert headers["aeg-metadata-version"] == "1"
    assert headers["aeg-delivery-count"] == "0"
    assert headers["content-type"].startswith("application/json")
    assert isinstance(json.loads(raw), list) and len(json.loads(raw)) == 1  # Event Grid schema: an array


def test_put_block_list_and_copy_blob(emu, hook, container):
    sub_all(emu, hook)
    blob = container.get_blob_client("blocks.bin")
    blob.stage_block("b1", b"aaaa")
    blob.stage_block("b2", b"bb")
    blob.commit_block_list([BlobBlock("b1"), BlobBlock("b2")])
    events = hook.wait_events(1)
    assert events[0]["data"]["api"] == "PutBlockList"
    assert events[0]["data"]["contentLength"] == 6
    # staging blocks does not raise anything (only the commit does)
    copy = container.get_blob_client("copy.bin")
    copy.start_copy_from_url(blob.url)
    events = hook.wait_events(2)
    by_api = {e["data"]["api"]: e for e in events}
    assert set(by_api) == {"PutBlockList", "CopyBlob"}
    assert by_api["CopyBlob"]["subject"].endswith("/blobs/copy.bin")
    assert by_api["CopyBlob"]["data"]["contentLength"] == 6


def test_append_page_and_metadata_calls_raise_nothing_else(emu, hook, container):
    """Learn lists PutBlob, PutBlockList and CopyBlob as the writes that raise BlobCreated; the rest is silent."""
    sub_all(emu, hook)
    ap = container.get_blob_client("log")
    ap.create_append_blob()  # PutBlob: one event
    ap.append_block(b"line\n")  # AppendBlock: none
    ap.set_blob_metadata({"a": "b"})  # none
    ap.create_snapshot()  # none
    events = hook.wait_events(1)
    import time

    time.sleep(0.4)
    assert len(hook.events) == 1
    assert events[0]["data"]["blobType"] == "AppendBlob"


def test_delete_blob_raises_blob_deleted(emu, hook, container):
    blob = container.get_blob_client("gone.txt")
    blob.upload_blob(b"data", overwrite=True)
    sub_all(emu, hook)  # subscribed after the upload: only the delete is seen
    blob.delete_blob()
    (ev,) = hook.wait_events(1)
    su.check_eg_envelope(ev, "Microsoft.Storage.BlobDeleted", su.subject_of(container.container_name, "gone.txt"))
    d = ev["data"]
    assert set(d) == su.BLOB_DELETED_DATA, set(d) ^ su.BLOB_DELETED_DATA
    assert d["api"] == "DeleteBlob"
    assert d["blobType"] == "BlockBlob"
    assert d["contentType"] == "application/octet-stream"


def test_failed_operations_raise_nothing(emu, hook, container):
    sub_all(emu, hook)
    blob = container.get_blob_client("exists.txt")
    blob.upload_blob(b"1")
    hook.wait_events(1)
    with pytest.raises(Exception):
        blob.upload_blob(b"2", overwrite=False)  # 409 BlobAlreadyExists
    with pytest.raises(Exception):
        container.get_blob_client("never-there").delete_blob()  # 404
    import time

    time.sleep(0.5)
    assert len(hook.events) == 1


def test_tier_change_events(emu, hook, container):
    blob = container.get_blob_client("tiered.bin")
    blob.upload_blob(b"0123456789")
    sub_all(emu, hook)
    blob.set_standard_blob_tier(StandardBlobTier.COOL)
    blob.set_standard_blob_tier(StandardBlobTier.HOT)
    events = hook.wait_events(2)
    events.sort(key=lambda e: e["data"]["sequencer"])
    first, second = events
    for ev in (first, second):
        su.check_eg_envelope(ev, "Microsoft.Storage.BlobTierChanged", su.subject_of(container.container_name, "tiered.bin"))
        assert ev["data"]["api"] == "SetBlobTier"
        assert ev["data"]["contentLength"] == 10
    assert first["data"]["accessTier"] == "Cool"
    assert "previousTier" not in first["data"]  # Learn: absent when the tier was inferred from the account
    assert second["data"]["accessTier"] == "Hot"
    assert second["data"]["previousTier"] == "Cool"
    assert set(second["data"]) == su.BLOB_TIER_CHANGED_DATA


def test_sequencer_grows_per_blob(emu, hook, container):
    sub_all(emu, hook)
    blob = container.get_blob_client("seq.txt")
    for i in range(3):
        blob.upload_blob(str(i).encode(), overwrite=True)
    events = hook.wait_events(3)
    seqs = sorted(e["data"]["sequencer"] for e in events)
    assert len(set(seqs)) == 3 and all(len(s) == len(seqs[0]) for s in seqs)


# -- filters ------------------------------------------------------------------------------------------------------------
def barrier(hook_all, n):
    """The catch-all subscription has seen n events: every delivery has been made."""
    import time

    hook_all.wait_events(n)
    time.sleep(0.4)


def test_filter_by_event_type(emu, container):
    all_hook, only_deleted = su.Webhook(), su.Webhook()
    try:
        sub_all(emu, all_hook, "all")
        su.subscribe(emu, "deleted", {"webhookUrl": only_deleted.url, "filter": {"includedEventTypes": ["Microsoft.Storage.BlobDeleted"]}})
        b = container.get_blob_client("f.txt")
        b.upload_blob(b"1")
        b.delete_blob()
        barrier(all_hook, 2)
        evs = only_deleted.events
        assert [e["eventType"] for e in evs] == ["Microsoft.Storage.BlobDeleted"]
    finally:
        all_hook.stop()
        only_deleted.stop()


def test_filter_by_subject_prefix_and_suffix(emu, blob_service):
    c1, c2 = blob_service.create_container(su.uniq("a")), blob_service.create_container(su.uniq("b"))
    all_hook, pre, suf, both = su.Webhook(), su.Webhook(), su.Webhook(), su.Webhook()
    try:
        sub_all(emu, all_hook, "all")
        prefix = f"/blobServices/default/containers/{c1.container_name}/"
        su.subscribe(emu, "pre", {"webhookUrl": pre.url, "filter": {"subjectBeginsWith": prefix}})
        su.subscribe(emu, "suf", {"webhookUrl": suf.url, "filter": {"subjectEndsWith": ".csv"}})
        su.subscribe(emu, "both", {"webhookUrl": both.url, "filter": {"subjectBeginsWith": prefix, "subjectEndsWith": ".csv"}})
        c1.upload_blob("x.csv", b"1")
        c1.upload_blob("x.txt", b"1")
        c2.upload_blob("y.csv", b"1")
        c2.upload_blob("y.txt", b"1")
        barrier(all_hook, 4)
        subjects = lambda h: sorted(e["subject"] for e in h.events)  # noqa: E731
        assert subjects(pre) == sorted([su.subject_of(c1.container_name, "x.csv"), su.subject_of(c1.container_name, "x.txt")])
        assert subjects(suf) == sorted([su.subject_of(c1.container_name, "x.csv"), su.subject_of(c2.container_name, "y.csv")])
        assert subjects(both) == [su.subject_of(c1.container_name, "x.csv")]
    finally:
        for h in (all_hook, pre, suf, both):
            h.stop()


def test_advanced_filter_on_data(emu, container):
    all_hook, big, by_api = su.Webhook(), su.Webhook(), su.Webhook()
    try:
        sub_all(emu, all_hook, "all")
        su.subscribe(emu, "big", {"webhookUrl": big.url, "filter": {"advancedFilters": [
            {"operatorType": "NumberGreaterThan", "key": "data.contentLength", "value": 5}]}})
        su.subscribe(emu, "api", {"webhookUrl": by_api.url, "filter": {"advancedFilters": [
            {"operatorType": "StringIn", "key": "data.api", "values": ["PutBlockList"]}]}})
        container.upload_blob("small", b"123")
        container.upload_blob("large", b"1234567890")
        blk = container.get_blob_client("blk")
        blk.stage_block("1", b"zz")
        blk.commit_block_list([BlobBlock("1")])
        barrier(all_hook, 3)
        assert [e["subject"].rsplit("/", 1)[1] for e in big.events] == ["large"]
        assert [e["subject"].rsplit("/", 1)[1] for e in by_api.events] == ["blk"]
    finally:
        for h in (all_hook, big, by_api):
            h.stop()


# -- CloudEvents ----------------------------------------------------------------------------------------------------
def test_cloudevents_schema(emu, hook, container):
    view = sub_all(emu, hook, "ce", deliverySchema="CloudEventSchemaV1_0")
    assert view["validation"]["status"] == "Succeeded"  # CloudEvents handshake: OPTIONS with WebHook-Request-Origin
    assert hook.handshakes and hook.handshakes[0][0] == "OPTIONS"
    assert hook.handshakes[0][1]["webhook-request-origin"] == "eventgrid.azure.net"
    container.upload_blob("ce.txt", b"cloud")
    (ev,) = hook.wait_events(1)
    # Learn: CloudEvents schema properties of a storage event
    assert set(ev) == {"source", "subject", "type", "time", "id", "data", "specversion"}, ev
    assert ev["specversion"] == "1.0"
    assert ev["type"] == "Microsoft.Storage.BlobCreated"
    assert ev["source"].endswith(f"/providers/Microsoft.Storage/storageAccounts/{su.ACCOUNT}")
    assert ev["subject"] == su.subject_of(container.container_name, "ce.txt")
    assert uuid.UUID(ev["id"])
    assert ev["data"]["api"] == "PutBlob" and ev["data"]["contentLength"] == 5
    headers, raw = hook.requests[0]
    assert headers["content-type"].startswith("application/cloudevents+json")
    assert isinstance(json.loads(raw), dict)  # one event, not an array


# -- no subscription, reset ------------------------------------------------------------------------------------------
def test_no_event_without_a_subscription(emu, hook, container):
    container.upload_blob("before.txt", b"1")  # nobody listens
    status, sources = su.admin(emu, "GET", "/sources", expect=200)
    assert sources["value"] == []  # not even a source was made
    sub_all(emu, hook)
    container.upload_blob("after.txt", b"1")
    (ev,) = hook.wait_events(1)
    assert ev["subject"].endswith("/after.txt")
    _, view = su.admin(emu, "GET", su.sub_path("all"), expect=200)
    assert view["stats"]["delivered"] == 1


def test_reset_clears_subscriptions(emu, hook, container):
    import time

    sub_all(emu, hook)
    container.upload_blob("one.txt", b"1")
    hook.wait_events(1)
    emu.reset()
    su.admin(emu, "GET", su.sub_path("all"), expect=404)
    assert su.admin(emu, "GET", "/sources", expect=200)[1]["value"] == []
    c2 = su.uniq("after-reset")
    from azure.storage.blob import BlobServiceClient

    BlobServiceClient.from_connection_string(emu.storage_conn_str(su.ACCOUNT, su.KEY)).create_container(c2).upload_blob("two.txt", b"1")
    time.sleep(0.5)
    assert len(hook.events) == 1


def test_delete_subscription_stops_events(emu, hook, container):
    import time

    sub_all(emu, hook)
    container.upload_blob("one.txt", b"1")
    hook.wait_events(1)
    su.admin(emu, "DELETE", su.sub_path("all"), expect=204)
    container.upload_blob("two.txt", b"1")
    time.sleep(0.5)
    assert len(hook.events) == 1


def test_other_accounts_are_separate(emu, hook, container):
    su.subscribe(emu, "other", {"webhookUrl": hook.url}, account="someoneelse")
    container.upload_blob("mine.txt", b"1")  # account evtest: not subscribed
    import time

    time.sleep(0.5)
    assert hook.events == []


def test_batch_delete_raises_one_event_per_blob(emu, hook, container):
    names = ["b1", "b2", "b3"]
    for n in names:
        container.upload_blob(n, b"1")
    sub_all(emu, hook)
    container.delete_blobs(*names)
    events = hook.wait_events(3)
    assert sorted(e["subject"].rsplit("/", 1)[1] for e in events) == names
    assert {e["eventType"] for e in events} == {"Microsoft.Storage.BlobDeleted"}
