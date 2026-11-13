"""The admin API for event sources and the errors it gives."""

import seutil as su


def test_list_get_delete_subscription(emu, hook):
    view = su.subscribe(emu, "one", {"webhookUrl": hook.url, "filter": {"includedEventTypes": ["Microsoft.Storage.BlobCreated"]}})
    assert view["name"] == "one" and view["deliveryMode"] == "push"
    assert view["filter"]["includedEventTypes"] == ["Microsoft.Storage.BlobCreated"]
    status, listing = su.admin(emu, "GET", su.sub_path(), expect=200)
    assert [s["name"] for s in listing["value"]] == ["one"]
    _, sources = su.admin(emu, "GET", "/sources", expect=200)
    assert len(sources["value"]) == 1
    assert sources["value"][0]["resourceId"].endswith(f"/storageAccounts/{su.ACCOUNT}")
    su.admin(emu, "GET", su.sub_path("one"), expect=200)
    su.admin(emu, "DELETE", su.sub_path("one"), expect=204)
    su.admin(emu, "GET", su.sub_path("one"), expect=404)
    su.admin(emu, "DELETE", su.sub_path("one"), expect=404)


def test_replacing_a_subscription_gives_200(emu, hook):
    s1, _ = su.admin(emu, "PUT", su.sub_path("x"), {"webhookUrl": hook.url})
    s2, _ = su.admin(emu, "PUT", su.sub_path("x"), {"webhookUrl": hook.url})
    assert (s1, s2) == (201, 200)


def test_invalid_subscriptions_are_400(emu, hook):
    for body in (
        {},  # a source has nothing to pull
        {"destination": {"endpointType": "Nope"}},
        {"destination": {"endpointType": "StorageQueue", "storageAccount": "a"}},  # no queue name
        {"destination": {"endpointType": "StorageQueue", "storageAccount": "a", "queueName": "q", "queueMessageTimeToLiveInSeconds": 0}},
        {"webhookUrl": hook.url, "destination": {"endpointType": "EventHub", "namespace": "n", "eventHubName": "h"}},
        {"webhookUrl": hook.url, "filter": {"advancedFilters": [{"operatorType": "NoSuchOperator", "key": "a", "value": 1}]}},
    ):
        status, err = su.admin(emu, "PUT", su.sub_path("bad"), body)
        assert status == 400, (body, status, err)
        assert err["error"]["code"] == "BadRequest"
    assert su.admin(emu, "GET", su.sub_path(), expect=200)[1]["value"] == []


def test_unknown_source_kind_is_404(emu):
    status, err = su.admin(emu, "PUT", "/sources/nosuchkind/x/subscriptions/s", {"webhookUrl": "http://127.0.0.1:1/x"})
    assert status == 404 and "storage" in err["error"]["message"]


def test_webhook_destination_written_arm_style(emu, hook, container):
    view = su.subscribe(emu, "armhook", {"destination": {"endpointType": "WebHook", "properties": {"endpointUrl": hook.url}}})
    assert view["webhookUrl"] == hook.url and view["validation"]["status"] == "Succeeded"
    container.upload_blob("w.txt", b"1")
    assert len(hook.wait_events(1)) == 1


def test_failed_handshake_blocks_delivery(emu, container):
    # nothing listens on this port, so the handshake fails and the subscription is not used
    view = su.subscribe(emu, "nohook", {"webhookUrl": "http://127.0.0.1:1/none"})
    assert view["validation"]["status"] == "Failed"
    container.upload_blob("n.txt", b"1")
    import time

    time.sleep(0.4)
    _, v = su.admin(emu, "GET", su.sub_path("nohook"), expect=200)
    assert v["stats"]["delivered"] == 0 and v["stats"]["pendingPush"] == 0
