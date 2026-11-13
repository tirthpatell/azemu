"""TLS on the emulator's own port, SASL PLAIN with a connection string, SASL OAUTHBEARER, and how a connection finds its
namespace and the broker address it is told to use."""

import time

import pytest
from confluent_kafka import KafkaException, TopicPartition

import ehutil
import kutil
from kutil import consume, consumer, produce_all, producer, sasl_plain, sasl_ssl, tls

pytestmark = pytest.mark.tls


def test_produce_and_consume_over_tls(emu, track):
    hub = kutil.uniq("tls")
    kutil.admin_create(emu, hub, 2, namespace=kutil.NS)
    p = track(producer(tls(emu)))
    produce_all(p, hub, [b"over tls"], partition=1)
    c = track(consumer(tls(emu), kutil.uniq("g")))
    c.subscribe([hub])
    (m,) = consume(c, 1)
    assert (m.value(), m.partition()) == (b"over tls", 1)


def test_the_tls_server_name_is_the_namespace_and_the_broker_address(emu, track):
    hub = kutil.uniq("ns")
    kutil.admin_create(emu, hub, 1, namespace="alpha")
    a = kutil.admin(tls(emu, "alpha"))
    md = a.list_topics(timeout=10)
    assert hub in md.topics
    (b,) = md.brokers.values()
    assert (b.host, b.port) == ("alpha.eventhub.localhost", emu.port)  # the name the client used, the port it came in on
    other = kutil.admin(tls(emu, "beta")).list_topics(timeout=10)
    assert hub not in other.topics  # another namespace does not have it


def test_sasl_plain_over_tls_with_a_connection_string(emu, track):
    hub = kutil.uniq("saslssl")
    kutil.admin_create(emu, hub, 1, namespace=kutil.NS)
    p = track(producer(sasl_ssl(emu)))
    produce_all(p, hub, [b"hello sasl"])
    c = track(consumer(sasl_ssl(emu), kutil.uniq("g")))
    c.subscribe([hub])
    assert consume(c, 1)[0].value() == b"hello sasl"


def test_sasl_plain_without_tls_takes_the_namespace_from_the_connection_string(emu, track):
    hub = kutil.uniq("saslplain")
    kutil.admin_create(emu, hub, 1, namespace=kutil.NS)
    conf = sasl_plain(emu)  # bootstrap 127.0.0.1: no host name anywhere but the Endpoint in the password
    a = kutil.admin(conf)
    md = a.list_topics(timeout=10)
    assert hub in md.topics
    (b,) = md.brokers.values()
    assert (b.host, b.port) == (f"{kutil.NS}.eventhub.localhost", emu.port)  # from the Endpoint (it names a local host)
    p = track(producer(conf))
    produce_all(p, hub, [b"x"])
    # a connection string for another namespace sees another set of topics
    assert hub not in kutil.admin(sasl_plain(emu, "gamma")).list_topics(timeout=10).topics


def test_sasl_plain_accepts_any_credentials(emu, track):
    conf = {**sasl_plain(emu), "sasl.username": "whoever", "sasl.password": "Endpoint=sb://myns.eventhub.localhost/;SharedAccessKeyName=x;SharedAccessKey=anything"}
    assert kutil.admin(conf).list_topics(timeout=10) is not None


def test_sasl_plain_with_a_password_that_is_not_a_connection_string_still_works(emu, track):
    conf = {**sasl_plain(emu), "sasl.password": "not a connection string"}
    md = kutil.admin(conf).list_topics(timeout=10)
    assert len(md.brokers) == 1


def test_a_client_that_skips_sasl_is_accepted(emu, track):
    hub = kutil.uniq("nosasl")
    kutil.admin_create(emu, hub, 1)  # namespace default
    p = track(producer(kutil.plain(emu)))
    produce_all(p, hub, [b"x"])


def test_oauthbearer_is_accepted_with_any_token(emu, track):
    hub = kutil.uniq("oauth")
    kutil.admin_create(emu, hub, 1, namespace=kutil.NS)
    calls = []

    def token(_cfg):
        calls.append(1)
        return "not-a-real-jwt", time.time() + 3600

    conf = {**tls(emu), "security.protocol": "SASL_SSL", "sasl.mechanism": "OAUTHBEARER", "oauth_cb": token}
    p = track(producer(conf))
    produce_all(p, hub, [b"with a bearer token"])
    assert calls


def test_a_client_that_does_not_trust_the_ca_cannot_connect(emu):
    conf = {**tls(emu), "ssl.ca.location": None}
    conf.pop("ssl.ca.location")
    conf["ssl.ca.location"] = "/nonexistent/ca.pem"
    with pytest.raises(Exception):
        producer(conf)  # librdkafka refuses a CA file it cannot read, at creation
    # with the system trust store only, the emulator's certificate is not trusted
    conf = {k: v for k, v in tls(emu).items() if k != "ssl.ca.location"}
    with pytest.raises(KafkaException):
        kutil.admin(conf).list_topics(timeout=6)


def test_plain_and_tls_work_on_the_same_port_at_the_same_time(emu, track):
    hub = kutil.uniq("both")
    kutil.admin_create(emu, hub, 1, namespace=kutil.NS)
    kutil.admin_create(emu, hub, 1)
    p1 = track(producer(tls(emu)))
    p2 = track(producer(kutil.plain(emu)))
    produce_all(p1, hub, [b"tls"])
    produce_all(p2, hub, [b"plain"])


def test_amqp_and_kafka_share_the_port(emu, track):
    # AMQP over plain TCP and TLS keep working next to Kafka (the first bytes tell the protocols apart)
    hub = kutil.uniq("amqp")
    ehutil.admin_create_hub(emu, hub, 1)
    prod = track(ehutil.make_producer(emu, hub))
    ehutil.send_n(prod, 2, partition_id="0")
    p = track(producer(tls(emu)))
    produce_all(p, hub, [b"k"], partition=0)
    assert len(ehutil.read_from_start(emu, hub, 3, partition_id="0")) == 3
