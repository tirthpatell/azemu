"""The Kafka endpoint of Event Hubs with credentials of every kind (`docs/strict-auth.md`): SASL PLAIN with a connection string, SASL OAUTHBEARER with an Entra
token. Each test states one row of what Azure answered in November 2026. The emulator side goes over TLS (the host name carries the namespace); on Azure it is
port 9093. A produce leaves its record in the hub until the hub's retention ends, so the tests produce as few as they can."""

import time

import pytest
from confluent_kafka import Consumer, KafkaError, Producer

SCOPE_FMT = "https://{host}/.default"


@pytest.fixture(autouse=True)
def _namespace(world):
    if world.eh is None:
        pytest.skip("no Event Hubs namespace in this run")


def base(world):
    if world.azure:
        return {"bootstrap.servers": f"{world.eh.host}:9093", "security.protocol": "SASL_SSL", "socket.timeout.ms": 15000, "log.connection.close": False}
    return {
        "bootstrap.servers": f"{world.eh.host}",
        "security.protocol": "SASL_SSL",
        "ssl.ca.location": world.emu.ca_file,
        "socket.timeout.ms": 15000,
        "log.connection.close": False,
    }


def plain(world, password):
    return {**base(world), "sasl.mechanism": "PLAIN", "sasl.username": "$ConnectionString", "sasl.password": password}


def oauth(world, principal=None, token=None):
    host = f"{world.eh.namespace}.servicebus.windows.net"

    # asked for here, not in the callback: azure-identity's token cache and librdkafka's callback thread can deadlock
    t = token if token is not None else principal.token(SCOPE_FMT.format(host=host))

    def cb(_config):
        return t, time.time() + 3000

    return {**base(world), "sasl.mechanism": "OAUTHBEARER", "oauth_cb": cb}


def produce(world, conf):
    """(delivery error codes, the errors the client reported on the side)."""
    side, results = [], []
    p = Producer({**conf, "error_cb": lambda e: side.append(e), "message.timeout.ms": 15000})
    p.produce(world.eh.entity, b"auth", partition=0, on_delivery=lambda err, msg: results.append(err))
    p.flush(20)
    return [e.code() if e else None for e in results], side


def consume(world, conf, seconds=15):
    """(messages, errors) of a consumer of the group `$Default` that starts at the end."""
    msgs, errors = [], []
    c = Consumer({**conf, "group.id": "$Default", "auto.offset.reset": "latest", "enable.auto.commit": False, "error_cb": lambda e: errors.append(e)})
    c.subscribe([world.eh.entity])
    end = time.time() + seconds
    while time.time() < end:
        m = c.poll(1.0)
        if m is None:
            continue
        if m.error():
            errors.append(m.error())
        else:
            msgs.append(m)
        if errors and any(e.code() == KafkaError.TOPIC_AUTHORIZATION_FAILED for e in errors):
            break
    c.close()
    return msgs, errors


def codes(errors):
    return {e.code() for e in errors}


def texts(errors):
    return " | ".join(str(e) for e in errors)


# -- connection strings ----------------------------------------------------------------------------------------------------------------

def test_the_root_and_the_send_only_string_produce(world):
    for cred in (world.eh.root, world.eh.send):
        assert produce(world, plain(world, cred.conn))[0] == [None], cred.name


def test_a_listen_only_string_cannot_produce(world):
    delivered, _ = produce(world, plain(world, world.eh.listen.conn))
    assert delivered == [KafkaError.TOPIC_AUTHORIZATION_FAILED]
    assert KafkaError.TOPIC_AUTHORIZATION_FAILED == 29


def test_a_send_only_string_cannot_consume_the_offset_fetch_is_refused(world):
    msgs, errors = consume(world, plain(world, world.eh.send.conn))
    assert msgs == []
    assert KafkaError.TOPIC_AUTHORIZATION_FAILED in codes(errors), texts(errors)


def test_the_root_and_the_listen_only_string_consume(world):
    for cred in (world.eh.root, world.eh.listen):
        msgs, errors = consume(world, plain(world, cred.conn), seconds=8)
        assert KafkaError.TOPIC_AUTHORIZATION_FAILED not in codes(errors), (cred.name, texts(errors))


def test_a_wrong_key_an_unknown_policy_and_a_password_that_is_no_connection_string_are_refused(world):
    root = world.eh.root
    ep = f"Endpoint=sb://{world.eh.host}/"
    for password, text in (
        (f"{ep};SharedAccessKeyName={root.name};SharedAccessKey={root.key}x", "Invalid SASL PLAIN password."),
        (f"{ep};SharedAccessKeyName=nobody;SharedAccessKey={root.key}", "Invalid SASL PLAIN password."),
        ("this is not a connection string", "The Service Bus connection string is not of the expected format."),
    ):
        _, side = produce(world, plain(world, password))
        assert text in texts(side), (password[:12], texts(side))


def test_another_user_name_is_refused(world):
    conf = {**plain(world, world.eh.root.conn), "sasl.username": "someone"}
    _, side = produce(world, conf)
    assert "Invalid SASL PLAIN user name." in texts(side), texts(side)


# -- Entra tokens ----------------------------------------------------------------------------------------------------------------------------

def test_a_principal_with_a_data_role_produces_and_a_receiver_consumes_and_cannot_produce(world, main, reader):
    assert produce(world, oauth(world, main))[0] == [None]
    assert produce(world, oauth(world, reader))[0] == [KafkaError.TOPIC_AUTHORIZATION_FAILED]
    msgs, errors = consume(world, oauth(world, reader), seconds=8)
    assert KafkaError.TOPIC_AUTHORIZATION_FAILED not in codes(errors), texts(errors)


def test_a_principal_with_no_role_cannot_produce_and_its_consumer_gets_a_transport_failure(world, norole):
    delivered, _ = produce(world, oauth(world, norole))
    assert delivered == [KafkaError.TOPIC_AUTHORIZATION_FAILED]
    msgs, errors = consume(world, oauth(world, norole), seconds=12)
    assert msgs == []
    assert KafkaError._TRANSPORT in codes(errors) or KafkaError._ALL_BROKERS_DOWN in codes(errors), texts(errors)
    assert KafkaError.TOPIC_AUTHORIZATION_FAILED not in codes(errors), "no Kafka error code on the consumer's side: the connection is closed"


def test_a_token_for_another_audience_or_with_its_signature_changed_is_refused_at_the_login(world, main):
    host = f"{world.eh.namespace}.servicebus.windows.net"
    good = main.token(SCOPE_FMT.format(host=host))
    changed = good[:-4] + ("AAAA" if not good.endswith("AAAA") else "BBBB")
    _, side = produce(world, oauth(world, token=changed))
    assert "InvalidSignature: Invalid authorization token signature." in texts(side), texts(side)
    _, side = produce(world, oauth(world, token=main.token("https://vault.azure.net/.default")))
    assert "Invalid URI: The format of the URI could not be determined." in texts(side), texts(side)
