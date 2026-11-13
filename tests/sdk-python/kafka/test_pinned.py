"""A pinned Kafka port (`--pin kafka=<port>`, the way to give Kafka a port of its own such as 9093) and the advertised-host
setting. These start a second emulator process each."""

import pytest

import kutil
from kutil import consume, consumer, produce_all, producer


@pytest.fixture
def pinned(emu):
    x = kutil.Extra(emu, ["--pin", "kafka={pinned}"])
    yield x
    x.stop()


def test_a_pinned_port_serves_plain_kafka_and_advertises_itself(pinned, track):
    hub = kutil.uniq("pin")
    pinned.create_hub(hub)
    conf = {"bootstrap.servers": f"127.0.0.1:{pinned.pinned}", "log.connection.close": False}
    a = kutil.admin(conf)
    md = a.list_topics(timeout=10)
    (b,) = md.brokers.values()
    assert (b.host, b.port) == ("127.0.0.1", pinned.pinned)  # the port the connection came in on, not the gateway's
    p = track(producer(conf))
    produce_all(p, hub, [b"on the pinned port"])
    c = track(consumer(conf, kutil.uniq("g")))
    c.subscribe([hub])
    assert consume(c, 1)[0].value() == b"on the pinned port"


def test_a_pinned_port_serves_tls_with_the_namespace_from_the_server_name(pinned, track):
    conf = {
        "bootstrap.servers": f"pinned.eventhub.localhost:{pinned.pinned}",
        "security.protocol": "SASL_SSL",
        "ssl.ca.location": pinned.ca_file,
        "sasl.mechanism": "PLAIN",
        "sasl.username": "$ConnectionString",
        "sasl.password": f"Endpoint=sb://pinned.servicebus.windows.net/;SharedAccessKeyName=k;SharedAccessKey=s",
        "log.connection.close": False,
    }
    hub = kutil.uniq("pintls")
    pinned.create_hub(hub, namespace="pinned")
    p = track(producer(conf))
    produce_all(p, hub, [b"tls on the pinned port"])
    md = kutil.admin(conf).list_topics(timeout=10)
    (b,) = md.brokers.values()
    # the TLS server name wins over the (unreachable) Azure name in the connection string
    assert (b.host, b.port) == ("pinned.eventhub.localhost", pinned.pinned)
    assert hub in md.topics


def test_the_gateway_port_still_speaks_http_and_kafka_with_a_pin(pinned, track):
    import urllib.request

    assert urllib.request.urlopen(f"http://127.0.0.1:{pinned.port}/_azemu/health", timeout=5).status == 200
    conf = {"bootstrap.servers": f"127.0.0.1:{pinned.port}", "log.connection.close": False}
    (b,) = kutil.admin(conf).list_topics(timeout=10).brokers.values()
    assert b.port == pinned.port


def test_the_advertised_host_setting_is_what_plain_clients_are_told(emu):
    x = kutil.Extra(emu, [], env={"AZEMU_KAFKA_ADVERTISED_HOST": "localhost"})
    try:
        conf = {"bootstrap.servers": f"127.0.0.1:{x.port}", "log.connection.close": False}
        (b,) = kutil.admin(conf).list_topics(timeout=10).brokers.values()
        assert (b.host, b.port) == ("localhost", x.port)
    finally:
        x.stop()


def test_the_advertised_port_setting(emu):
    x = kutil.Extra(emu, [], env={"AZEMU_KAFKA_ADVERTISED_HOST": "127.0.0.1", "AZEMU_KAFKA_ADVERTISED_PORT": "1"})
    try:
        conf = {"bootstrap.servers": f"127.0.0.1:{x.port}", "log.connection.close": False, "socket.timeout.ms": 3000}
        md = kutil.admin(conf).list_topics(timeout=10)  # the bootstrap works; the advertised broker is what it says
        (b,) = md.brokers.values()
        assert (b.host, b.port) == ("127.0.0.1", 1)
    finally:
        x.stop()
