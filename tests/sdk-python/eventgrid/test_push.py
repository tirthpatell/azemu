"""Push delivery: the emulator POSTs events to a webhook that the test runs in-process.

Webhook subscriptions are created through the admin API (on Azure: ARM). What the webhook
sees follows Event Grid's delivery contract: the validation handshake, `aeg-*` headers, a
one-element array for the Event Grid schema, one event per request for CloudEvents, retries
on failure and dead-lettering.
"""

import time

import pytest
from azure.core.messaging import CloudEvent
from azure.eventgrid import EventGridEvent, EventGridPublisherClient

from egutil import Hook


@pytest.fixture
def topic(admin, name):
    admin.put_topic(name)
    return name


@pytest.fixture
def client(emu, topic, key):
    with EventGridPublisherClient(emu.url("eventgrid", topic) + "/api/events", key, retry_total=0) as c:
        yield c


def eg(subject="/subject", type_="Test.Event", data=None, version="2.0", **kw):
    return EventGridEvent(subject=subject, event_type=type_, data=data if data is not None else {"k": "v"}, data_version=version, **kw)


# -- handshake -------------------------------------------------------------------------

@pytest.mark.emulator_only
def test_subscription_validation_handshake_for_event_grid_schema(admin, topic, hook):
    sub = admin.put_sub(topic, "s", {"webhookUrl": hook.url}, expect=(201,))
    assert sub["validation"]["status"] == "Succeeded"
    (hs,) = hook.handshakes
    assert hs["method"] == "POST"
    assert hs["headers"]["aeg-event-type"] == "SubscriptionValidation"
    ev = hs["body"][0]
    assert ev["eventType"] == "Microsoft.EventGrid.SubscriptionValidationEvent"
    assert ev["data"]["validationCode"]
    assert ev["data"]["validationUrl"]
    assert ev["dataVersion"] == "2"
    assert ev["metadataVersion"] == "1"
    assert ev["topic"].endswith(f"/topics/{topic}")


@pytest.mark.emulator_only
def test_cloud_events_subscription_uses_the_options_handshake(admin, name, hook):
    admin.put_topic(name, {"inputSchema": "CloudEventSchemaV1_0"})
    sub = admin.put_sub(name, "s", {"webhookUrl": hook.url}, expect=(201,))
    assert sub["validation"]["status"] == "Succeeded"
    (hs,) = hook.handshakes
    assert hs["method"] == "OPTIONS"
    assert hs["headers"]["webhook-request-origin"] == "eventgrid.azure.net"
    assert "webhook-request-callback" in hs["headers"]


@pytest.mark.emulator_only
def test_failed_handshake_is_recorded_on_the_subscription_and_blocks_delivery(admin, topic, client):
    bad = Hook(statuses=[200], handshake=False)  # answers 200 but never echoes the validation code
    try:
        sub = admin.put_sub(topic, "s", {"webhookUrl": bad.url}, expect=(201,))
        assert sub["validation"]["status"] != "Succeeded"
        client.send(eg())
        time.sleep(0.4)
        assert bad.notifications == []
        assert admin.get_sub(topic, "s")["validation"]["status"] == sub["validation"]["status"]
    finally:
        bad.close()


@pytest.mark.emulator_only
def test_unreachable_webhook_fails_the_handshake(admin, topic):
    sub = admin.put_sub(topic, "s", {"webhookUrl": "http://127.0.0.1:1/nope"}, expect=(201,))
    assert sub["validation"]["status"] == "Failed"
    assert sub["validation"]["detail"]


@pytest.mark.emulator_only
def test_skip_validation_flag(admin, topic, client):
    hook = Hook(handshake=False)
    try:
        sub = admin.put_sub(topic, "s", {"webhookUrl": hook.url, "skipValidation": True}, expect=(201,))
        assert sub["validation"]["status"] == "Skipped"
        assert hook.handshakes == []
        client.send(eg())
        assert len(hook.wait_for(1)) == 1
    finally:
        hook.close()


# -- delivery ---------------------------------------------------------------------------

@pytest.mark.emulator_only
def test_event_grid_event_delivery(admin, topic, client, hook):
    admin.put_sub(topic, "mysub", {"webhookUrl": hook.url}, expect=(201,))
    e = eg(subject="/orders/42", type_="Order.Created", data={"id": 42})
    client.send(e)
    (n,) = hook.wait_for(1)
    h = n["headers"]
    assert n["method"] == "POST"
    assert h["aeg-event-type"] == "Notification"
    assert h["aeg-subscription-name"].upper() == "MYSUB"
    assert h["aeg-delivery-count"] == "0"
    assert h["aeg-data-version"] == "2.0"
    assert h["aeg-metadata-version"] == "1"
    assert h["content-type"].startswith("application/json")
    body = n["body"]
    assert isinstance(body, list) and len(body) == 1
    ev = body[0]
    assert ev["id"] == str(e.id)
    assert ev["subject"] == "/orders/42"
    assert ev["eventType"] == "Order.Created"
    assert ev["dataVersion"] == "2.0"
    assert ev["metadataVersion"] == "1"
    assert ev["data"] == {"id": 42}
    assert ev["topic"].endswith(f"/topics/{topic}")
    assert ev["eventTime"]


@pytest.mark.emulator_only
def test_every_event_of_a_batch_is_delivered(admin, topic, client, hook):
    admin.put_sub(topic, "s", {"webhookUrl": hook.url}, expect=(201,))
    client.send([eg(subject=f"/{i}", data={"i": i}) for i in range(6)])
    got = hook.wait_for(6)
    assert sorted(g["body"][0]["data"]["i"] for g in got) == list(range(6))


@pytest.mark.emulator_only
def test_cloud_event_delivery_in_structured_mode(admin, name, key, emu, hook):
    admin.put_topic(name, {"inputSchema": "CloudEventSchemaV1_0"})
    admin.put_sub(name, "s", {"webhookUrl": hook.url}, expect=(201,))
    with EventGridPublisherClient(emu.url("eventgrid", name) + "/api/events", key) as c:
        c.send(CloudEvent(source="/src", type="My.Type", data={"a": 1}, subject="sub"))
    (n,) = hook.wait_for(1)
    assert n["headers"]["content-type"].startswith("application/cloudevents+json")
    assert n["headers"]["aeg-event-type"] == "Notification"
    ev = n["body"]
    assert isinstance(ev, dict)
    assert ev["specversion"] == "1.0"
    assert (ev["source"], ev["type"], ev["subject"], ev["data"]) == ("/src", "My.Type", "sub", {"a": 1})


@pytest.mark.emulator_only
def test_delivery_schema_converts_event_grid_events_to_cloud_events(admin, topic, client, hook):
    admin.put_sub(topic, "s", {"webhookUrl": hook.url, "deliverySchema": "CloudEventSchemaV1_0"}, expect=(201,))
    client.send(eg(subject="/x", type_="T.One", data={"a": 1}))
    (n,) = hook.wait_for(1)
    ev = n["body"]
    assert ev["specversion"] == "1.0"
    assert ev["type"] == "T.One"
    assert ev["subject"] == "/x"
    assert ev["data"] == {"a": 1}
    assert n["headers"]["content-type"].startswith("application/cloudevents+json")


@pytest.mark.emulator_only
def test_custom_schema_events_are_delivered_as_published(admin, topic, client, hook):
    admin.put_sub(topic, "s", {"webhookUrl": hook.url}, expect=(201,))
    client.send({"customId": "1234", "customData": {"x": [1, 2]}})
    (n,) = hook.wait_for(1)
    assert n["body"] == [{"customId": "1234", "customData": {"x": [1, 2]}}]


@pytest.mark.emulator_only
def test_each_subscription_gets_every_event(admin, topic, client):
    hooks = [Hook(), Hook()]
    try:
        for i, h in enumerate(hooks):
            admin.put_sub(topic, f"s{i}", {"webhookUrl": h.url}, expect=(201,))
        client.send(eg())
        for h in hooks:
            assert len(h.wait_for(1)) == 1
    finally:
        for h in hooks:
            h.close()


@pytest.mark.emulator_only
def test_publish_returns_before_the_webhook_answers(admin, topic, client):
    """The publish call is not held up by a slow or failing webhook."""
    hook = Hook(statuses=[500])
    try:
        admin.put_sub(topic, "s", {"webhookUrl": hook.url, "retryPolicy": {"retryIntervalsMs": [2000]}}, expect=(201,))
        t0 = time.time()
        client.send(eg())
        assert time.time() - t0 < 1.0
    finally:
        hook.close()


# -- filters ----------------------------------------------------------------------------

@pytest.mark.emulator_only
def test_event_type_and_subject_filters(admin, topic, client, hook):
    admin.put_sub(
        topic,
        "s",
        {"webhookUrl": hook.url, "filter": {"includedEventTypes": ["Order.Created"], "subjectBeginsWith": "/orders/", "subjectEndsWith": ".json"}},
        expect=(201,),
    )
    client.send(
        [
            eg(subject="/orders/1.json", type_="Order.Created", data={"n": 1}),
            eg(subject="/orders/2.json", type_="Order.Deleted", data={"n": 2}),
            eg(subject="/other/3.json", type_="Order.Created", data={"n": 3}),
            eg(subject="/orders/4.txt", type_="Order.Created", data={"n": 4}),
        ]
    )
    got = hook.wait_for(1)
    time.sleep(0.4)
    got = hook.notifications
    assert [g["body"][0]["data"]["n"] for g in got] == [1]


@pytest.mark.emulator_only
def test_case_sensitive_subject_filter(admin, topic, client, hook):
    admin.put_sub(topic, "s", {"webhookUrl": hook.url, "filter": {"subjectBeginsWith": "/Orders", "isSubjectCaseSensitive": True}}, expect=(201,))
    client.send([eg(subject="/orders/a", data={"n": 1}), eg(subject="/Orders/b", data={"n": 2})])
    hook.wait_for(1)
    time.sleep(0.3)
    assert [g["body"][0]["data"]["n"] for g in hook.notifications] == [2]


@pytest.mark.emulator_only
@pytest.mark.parametrize(
    "flt,matching",
    [
        ({"operatorType": "StringIn", "key": "data.color", "values": ["red", "Green"]}, [1, 2]),
        ({"operatorType": "StringNotIn", "key": "data.color", "values": ["red"]}, [2, 3]),
        ({"operatorType": "StringContains", "key": "data.color", "values": ["ee"]}, [2]),
        ({"operatorType": "StringBeginsWith", "key": "data.color", "values": ["bl"]}, [3]),
        ({"operatorType": "StringEndsWith", "key": "data.color", "values": ["ed"]}, [1]),
        ({"operatorType": "NumberIn", "key": "data.size", "values": [1, 3]}, [1, 3]),
        ({"operatorType": "NumberGreaterThan", "key": "data.size", "value": 1}, [2, 3]),
        ({"operatorType": "NumberLessThanOrEquals", "key": "data.size", "value": 2}, [1, 2]),
        ({"operatorType": "NumberInRange", "key": "data.size", "values": [[2, 3]]}, [2, 3]),
        ({"operatorType": "BoolEquals", "key": "data.flag", "value": True}, [1, 3]),
        ({"operatorType": "IsNotNull", "key": "data.maybe"}, [2]),
        ({"operatorType": "IsNullOrUndefined", "key": "data.maybe"}, [1, 3]),
    ],
)
def test_advanced_filters(admin, topic, client, hook, flt, matching):
    admin.put_sub(topic, "s", {"webhookUrl": hook.url, "filter": {"advancedFilters": [flt]}}, expect=(201,))
    data = [
        {"n": 1, "color": "red", "size": 1, "flag": True},
        {"n": 2, "color": "green", "size": 2, "flag": False, "maybe": "x"},
        {"n": 3, "color": "blue", "size": 3, "flag": True},
    ]
    client.send([eg(subject=f"/{d['n']}", data=d) for d in data])
    hook.wait_for(len(matching))
    time.sleep(0.4)
    assert sorted(g["body"][0]["data"]["n"] for g in hook.notifications) == matching


@pytest.mark.emulator_only
def test_all_advanced_filters_must_match(admin, topic, client, hook):
    admin.put_sub(
        topic,
        "s",
        {
            "webhookUrl": hook.url,
            "filter": {
                "advancedFilters": [
                    {"operatorType": "StringIn", "key": "data.color", "values": ["red"]},
                    {"operatorType": "NumberGreaterThan", "key": "data.size", "value": 5},
                ]
            },
        },
        expect=(201,),
    )
    client.send([eg(data={"n": 1, "color": "red", "size": 1}), eg(data={"n": 2, "color": "red", "size": 9}), eg(data={"n": 3, "color": "blue", "size": 9})])
    hook.wait_for(1)
    time.sleep(0.4)
    assert [g["body"][0]["data"]["n"] for g in hook.notifications] == [2]


# -- retries and dead letters ------------------------------------------------------------

@pytest.mark.emulator_only
def test_failed_deliveries_are_retried_until_the_webhook_succeeds(admin, topic, client):
    hook = Hook(statuses=[500, 503, 200])
    try:
        admin.put_sub(topic, "s", {"webhookUrl": hook.url, "retryPolicy": {"retryIntervalsMs": [50]}}, expect=(201,))
        client.send(eg(data={"n": 1}))
        got = hook.wait_for(3)
        time.sleep(0.4)
        got = hook.notifications
        assert len(got) == 3  # no fourth attempt after the 200
        assert [g["headers"]["aeg-delivery-count"] for g in got] == ["0", "1", "2"]
        assert all(g["body"][0]["data"] == {"n": 1} for g in got)
        log = admin.inspect(topic, "s", "deliveries")
        assert [(a["attempt"], a["outcome"], a["statusCode"]) for a in log] == [(1, "Failure", 500), (2, "Failure", 503), (3, "Success", 200)]
    finally:
        hook.close()


@pytest.mark.emulator_only
def test_retry_intervals_are_honoured(admin, topic, client):
    hook = Hook(statuses=[500, 500, 200])
    try:
        admin.put_sub(topic, "s", {"webhookUrl": hook.url, "retryPolicy": {"retryIntervalsMs": [300, 600]}}, expect=(201,))
        client.send(eg())
        got = hook.wait_for(3)
        assert len(got) == 3
        gaps = [got[1]["time"] - got[0]["time"], got[2]["time"] - got[1]["time"]]
        assert 0.25 < gaps[0] < 0.55
        assert 0.55 < gaps[1] < 0.95
    finally:
        hook.close()


@pytest.mark.emulator_only
def test_event_is_dead_lettered_after_the_maximum_attempts(admin, topic, client):
    hook = Hook(statuses=[500])
    try:
        admin.put_sub(
            topic, "s", {"webhookUrl": hook.url, "retryPolicy": {"maxDeliveryAttempts": 3, "retryIntervalsMs": [30]}}, expect=(201,)
        )
        client.send(eg(data={"n": "doomed"}))
        hook.wait_for(3)
        time.sleep(0.5)
        assert len(hook.notifications) == 3
        dead = admin.inspect(topic, "s", "deadletters")
        assert len(dead) == 1
        assert dead[0]["reason"] == "MaxDeliveryAttemptsExceeded"
        assert dead[0]["deliveryCount"] == 3
        assert dead[0]["event"]["data"] == {"n": "doomed"}
        assert admin.inspect(topic, "s", "deliveries")[-1]["outcome"] == "DeadLettered"
        assert admin.get_sub(topic, "s")["stats"]["deadLettered"] == 1
    finally:
        hook.close()


@pytest.mark.emulator_only
def test_a_400_answer_is_dead_lettered_without_retry(admin, topic, client):
    hook = Hook(statuses=[400])
    try:
        admin.put_sub(topic, "s", {"webhookUrl": hook.url, "retryPolicy": {"retryIntervalsMs": [30]}}, expect=(201,))
        client.send(eg())
        hook.wait_for(1)
        time.sleep(0.5)
        assert len(hook.notifications) == 1
        assert len(admin.inspect(topic, "s", "deadletters")) == 1
    finally:
        hook.close()


@pytest.mark.emulator_only
@pytest.mark.parametrize("status", [200, 201, 202, 204])
def test_any_2xx_answer_counts_as_delivered(admin, topic, client, status):
    hook = Hook(statuses=[status])
    try:
        admin.put_sub(topic, "s", {"webhookUrl": hook.url}, expect=(201,))
        client.send(eg())
        hook.wait_for(1)
        time.sleep(0.3)
        assert len(hook.notifications) == 1
        assert admin.get_sub(topic, "s")["stats"]["delivered"] == 1
    finally:
        hook.close()


@pytest.mark.emulator_only
def test_event_time_to_live_of_the_retry_policy(admin, topic, client):
    hook = Hook(statuses=[500])
    try:
        admin.put_sub(
            topic, "s", {"webhookUrl": hook.url, "retryPolicy": {"eventTimeToLiveInMinutes": 0.01, "retryIntervalsMs": [100]}}, expect=(201,)
        )
        client.send(eg())
        deadline = time.time() + 5
        while time.time() < deadline and not admin.inspect(topic, "s", "deadletters"):
            time.sleep(0.1)
        dead = admin.inspect(topic, "s", "deadletters")
        assert dead and dead[0]["reason"] == "TimeToLiveExpired"
    finally:
        hook.close()


# -- delivery log -----------------------------------------------------------------------

@pytest.mark.emulator_only
def test_delivery_log_records_each_attempt(admin, topic, client, hook):
    admin.put_sub(topic, "s", {"webhookUrl": hook.url}, expect=(201,))
    e = eg()
    client.send(e)
    hook.wait_for(1)
    deadline = time.time() + 3
    while time.time() < deadline and not admin.inspect(topic, "s", "deliveries"):
        time.sleep(0.05)
    (a,) = admin.inspect(topic, "s", "deliveries")
    assert a["eventId"] == str(e.id)
    assert a["outcome"] == "Success"
    assert a["statusCode"] == 200
    assert a["attempt"] == 1
    assert a["time"] and "durationMs" in a


@pytest.mark.emulator_only
def test_delivery_log_explains_a_connection_failure(admin, topic, client):
    admin.put_sub(topic, "s", {"webhookUrl": "http://127.0.0.1:1/down", "skipValidation": True, "retryPolicy": {"maxDeliveryAttempts": 1}}, expect=(201,))
    client.send(eg())
    deadline = time.time() + 5
    while time.time() < deadline and not admin.inspect(topic, "s", "deliveries"):
        time.sleep(0.05)
    (a,) = admin.inspect(topic, "s", "deliveries")
    assert a["outcome"] == "DeadLettered"
    assert "statusCode" not in a
    assert "connect" in a["detail"].lower()


# -- lifecycle --------------------------------------------------------------------------

@pytest.mark.emulator_only
def test_deleting_the_subscription_stops_deliveries(admin, topic, client):
    hook = Hook(statuses=[500])
    try:
        admin.put_sub(topic, "s", {"webhookUrl": hook.url, "retryPolicy": {"retryIntervalsMs": [50]}}, expect=(201,))
        client.send(eg())
        hook.wait_for(2)
        assert admin.request("DELETE", f"/topics/{topic}/subscriptions/s").status_code == 204
        time.sleep(0.2)
        n = len(hook.notifications)
        time.sleep(0.5)
        assert len(hook.notifications) == n
    finally:
        hook.close()


@pytest.mark.emulator_only
def test_manual_validation_through_the_validation_url(admin, topic, client):
    import requests

    hook = Hook(statuses=[200], handshake=False)
    try:
        sub = admin.put_sub(topic, "s", {"webhookUrl": hook.url}, expect=(201,))
        assert sub["validation"]["status"] == "AwaitingManualAction"
        url = hook.handshakes[0]["body"][0]["data"]["validationUrl"]
        assert requests.get(url, timeout=10).status_code == 200
        assert admin.get_sub(topic, "s")["validation"]["status"] == "Succeeded"
        client.send(eg())
        assert len(hook.wait_for(1)) == 1
    finally:
        hook.close()
