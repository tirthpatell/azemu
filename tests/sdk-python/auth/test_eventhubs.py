"""Event Hubs over AMQP with credentials of every kind (`docs/strict-auth.md`). Each test states one row of what Azure answered in November 2026; the same
tests run against the emulator (strict mode) and against Azure (`AZEMU_AZURE=1`: nothing is created on the management plane; a send leaves its event in the
hub until the hub's retention ends, so the tests send as few as they can: five in all).

Where the refusal comes: a bad credential at `put-token` (the SDK: `AuthenticationError`), a missing claim at the link attach (`ConnectionLostError`),
and a principal with no role at the `$management` read (`AuthenticationError`, status 401 inside the answer)."""

import threading
import time

import pytest
from azure.eventhub import EventData, EventHubConsumerClient, EventHubProducerClient
from azure.eventhub.exceptions import AuthenticationError, EventHubError

from authutil import FixedToken, SasCredential, sas_token

SCOPE = "https://eventhubs.azure.net/.default"
KW = dict(retry_total=0, auth_timeout=15)


@pytest.fixture(autouse=True)
def _namespace(world):
    if world.eh is None:
        pytest.skip("no Event Hubs namespace in this run")


# -- clients ----------------------------------------------------------------------------------------------------------------------------

def producer(world, cred):
    return EventHubProducerClient.from_connection_string(cred.conn.rstrip(";"), eventhub_name=world.eh.entity, **KW)


def consumer(world, cred):
    return EventHubConsumerClient.from_connection_string(cred.conn.rstrip(";"), consumer_group="$Default", eventhub_name=world.eh.entity, **KW)


def producer_with(world, credential):
    extra = {} if world.azure else {"use_tls": False}
    return EventHubProducerClient(world.eh.host, world.eh.entity, credential, **KW, **extra)


def consumer_with(world, credential):
    extra = {} if world.azure else {"use_tls": False}
    return EventHubConsumerClient(world.eh.host, world.eh.entity, "$Default", credential, **KW, **extra)


def send_one(p, text="auth"):
    batch = p.create_batch(partition_id="0")
    batch.add(EventData(text))
    p.send_batch(batch)


def receive_errors(c, seconds=4):
    """Open a receiver on partition 0 from the end, wait, and return what the SDK reported."""
    errors = []

    def on_error(partition_context, error):
        errors.append(error)

    def run():
        try:
            c.receive(on_event=lambda ctx, ev: None, partition_id="0", starting_position="@latest", on_error=on_error)
        except Exception as e:  # noqa: BLE001
            errors.append(e)

    t = threading.Thread(target=run, daemon=True)
    t.start()
    time.sleep(seconds)
    c.close()
    t.join(timeout=15)
    return errors


def needs(e, claim):
    t = str(e)
    assert "amqp:unauthorized-access" in t, t
    assert f"'{claim}' claim(s) are required to perform this operation." in t, t
    return t


# -- shared access keys -----------------------------------------------------------------------------------------------------------------

def test_the_root_key_reads_the_hub_and_sends(world):
    with producer(world, world.eh.root) as p:
        assert p.get_partition_ids() == ["0", "1"] or len(p.get_partition_ids()) >= 1
        send_one(p)


def test_a_send_only_rule_reads_the_hub_properties_and_sends_and_cannot_receive(world):
    with producer(world, world.eh.send) as p:
        assert p.get_partition_ids()
        assert p.get_eventhub_properties()["eventhub_name"] == world.eh.entity
        send_one(p)
    errors = receive_errors(consumer(world, world.eh.send))
    assert errors, "the receiver must have been refused"
    t = needs(errors[0], "Listen")
    # the resource is the partition of the consumer group, in lower case
    assert f"/consumergroups/$default/partitions/0" in t


def test_a_listen_only_rule_reads_receives_and_cannot_send(world):
    with producer(world, world.eh.listen) as p:
        assert p.get_partition_ids()
        with pytest.raises(EventHubError) as e:
            send_one(p)
        needs(e.value, "Send")
    assert receive_errors(consumer(world, world.eh.listen)) == []


# -- shared access signatures -------------------------------------------------------------------------------------------------------------

def resource(world, path=""):
    return f"sb://{world.eh.host}/{path}"


def sas_producer(world, token):
    return producer_with(world, SasCredential(token))


def test_a_sas_token_for_the_hub_or_the_namespace_is_accepted_and_one_late_by_290_seconds_too(world):
    r = world.eh.root
    for token in (
        sas_token(resource(world, world.eh.entity), r.name, r.key, int(time.time()) + 3600),
        sas_token(resource(world), r.name, r.key, int(time.time()) + 3600),
        sas_token(resource(world, world.eh.entity), r.name, r.key, int(time.time()) - 290),
    ):
        with sas_producer(world, token) as p:
            assert p.get_partition_ids()


def test_a_sas_token_that_is_expired_has_a_wrong_signature_a_wrong_name_or_another_hub_is_refused(world):
    r = world.eh.root
    hub = resource(world, world.eh.entity)
    soon = int(time.time()) + 3600
    bad = {
        "expired an hour ago": sas_token(hub, r.name, r.key, int(time.time()) - 3600),
        "expired 310 seconds ago": sas_token(hub, r.name, r.key, int(time.time()) - 310),
        "another key": sas_token(hub, r.name, r.key + "x", soon),
        "an unknown key name": sas_token(hub, "nobody", r.key, soon),
        "another hub": sas_token(resource(world, "somewhere-else"), r.name, r.key, soon),
    }
    for what, token in bad.items():
        with sas_producer(world, token) as p:
            with pytest.raises(AuthenticationError):
                p.get_partition_ids()


# -- Entra tokens ----------------------------------------------------------------------------------------------------------------------------

def test_a_principal_with_a_data_role_reads_and_sends(world, main):
    with producer_with(world, main) as p:
        assert p.get_partition_ids()
        send_one(p)


def test_a_receiver_reads_and_receives_and_cannot_send(world, reader):
    with producer_with(world, reader) as p:
        assert p.get_partition_ids()
        with pytest.raises(EventHubError) as e:
            send_one(p)
        needs(e.value, "Send")
    assert receive_errors(consumer_with(world, reader)) == []


def test_a_principal_with_no_role_passes_put_token_and_is_refused_at_the_attach_and_at_management(world, norole):
    with producer_with(world, norole) as p:
        with pytest.raises(EventHubError) as e:
            send_one(p)
        needs(e.value, "Send")
        # the `$management` link attaches, and the answer to the READ is a 401
        with pytest.raises(AuthenticationError) as e:
            p.get_partition_ids()
        assert "Management authentication failed. Status code: 401" in str(e.value)
    errors = receive_errors(consumer_with(world, norole))
    assert errors
    assert "Management authentication failed" in str(errors[0]) or "'Listen' claim(s)" in str(errors[0]), str(errors[0])


def test_a_token_for_another_audience_or_with_its_signature_changed_is_refused_at_put_token(world, main):
    good = main.token(SCOPE)
    changed = good[:-4] + ("AAAA" if not good.endswith("AAAA") else "BBBB")
    for token in (main.token("https://vault.azure.net/.default"), changed, "not-a-token"):
        with producer_with(world, FixedToken(token)) as p:
            with pytest.raises(AuthenticationError):
                p.get_partition_ids()
