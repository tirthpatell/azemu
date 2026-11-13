"""Client construction and lifecycle: connection strings, credentials, context managers, errors that never reach the broker.

Sources: SDK `_client_base._parse_conn_str` / `ClientBase.__init__` (every error below is raised by the SDK itself; the messages
and types were checked by running the SDK locally), `_connection_string_parser.parse_connection_string`, Microsoft's
`unittest/test_connection_string_parser.py`, `test_client_creation.py` and `livetest/synctests/test_negative.py` (connection
refused/unknown host -> ConnectError). Credentials: the emulator accepts every token, so these only prove that the SAS / JWT
`$cbs` exchange of each credential type works against it.

Note: the SDK does NOT reject a connection string whose EntityPath differs from the `eventhub_name` keyword; the keyword wins
(`entity = eventhub_name or entity_path`). That is asserted below as it is.
"""

import socket
import time

import pytest
from azure.core.credentials import AccessToken, AzureNamedKeyCredential, AzureSasCredential
from azure.eventhub import (
    EventData, EventHubConsumerClient, EventHubProducerClient, EventHubSharedKeyCredential, TransportType, parse_connection_string,
)
from azure.eventhub._pyamqp.utils import generate_sas_token
from azure.eventhub.exceptions import ConnectError, EventHubError

import ehutil
from ehutil import Collector, KEY, KEY_NAME, bodies, conn_str, host, make_consumer, make_producer, read_from_start, send_n

BASE = "Endpoint=sb://h.example/;SharedAccessKeyName=k;SharedAccessKey=x"


# -- malformed connection strings (client side only) -------------------------------------------------------------------------------------
@pytest.mark.parametrize("bad", [
    "", "garbage", "SharedAccessKeyName=k;SharedAccessKey=x", "Endpoint=;SharedAccessKeyName=k;SharedAccessKey=x",
    "Endpoint=sb://h.example/;SharedAccessKeyName=k", "Endpoint=sb://h.example/;SharedAccessKey=x", "Endpoint=sb://h.example/",
    "Endpoint=notaurl;SharedAccessKeyName=k;SharedAccessKey=x",
], ids=["empty", "garbage", "no-endpoint", "blank-endpoint", "no-key", "no-key-name", "endpoint-only", "bad-endpoint"])
def test_malformed_connection_string_is_a_value_error(bad):
    with pytest.raises(ValueError):
        EventHubProducerClient.from_connection_string(bad, eventhub_name="hub")
    with pytest.raises(ValueError):
        EventHubConsumerClient.from_connection_string(bad, consumer_group="$Default", eventhub_name="hub")


def test_missing_eventhub_name_is_a_value_error():
    with pytest.raises(ValueError):
        EventHubProducerClient.from_connection_string(BASE)
    with pytest.raises(ValueError):
        EventHubConsumerClient.from_connection_string(BASE, consumer_group="$Default")


def test_parse_connection_string_properties():
    props = parse_connection_string(BASE + ";EntityPath=hub")
    assert props.fully_qualified_namespace == "h.example"
    assert props.endpoint == "sb://h.example/"
    assert props.eventhub_name == "hub"
    assert props.shared_access_key_name == "k" and props.shared_access_key == "x"
    assert props.shared_access_signature is None


def test_parse_connection_string_without_entity_path_has_no_hub_name():
    assert parse_connection_string(BASE).eventhub_name is None


def test_parse_connection_string_keeps_the_port_of_the_endpoint():
    props = parse_connection_string("Endpoint=sb://myns.eventhub.localhost:5555;SharedAccessKeyName=k;SharedAccessKey=x")
    assert props.fully_qualified_namespace == "myns.eventhub.localhost:5555"


def test_parse_connection_string_rejects_key_and_signature_together():
    with pytest.raises(ValueError):
        parse_connection_string(BASE + ";SharedAccessSignature=SharedAccessSignature sr=a&sig=b&se=99")


# -- hub name from the string or the keyword ---------------------------------------------------------------------------------------------------
def test_entity_path_in_the_connection_string_names_the_hub(emu, hub, track):
    p = track(EventHubProducerClient.from_connection_string(conn_str(emu, hub), **ehutil.CLIENT_KW))
    assert p.eventhub_name == hub
    assert p.get_eventhub_properties()["eventhub_name"] == hub


def test_eventhub_name_keyword_names_the_hub(emu, hub, track):
    p = track(EventHubProducerClient.from_connection_string(conn_str(emu), eventhub_name=hub, **ehutil.CLIENT_KW))
    assert p.get_eventhub_properties()["eventhub_name"] == hub


def test_the_keyword_wins_over_a_conflicting_entity_path(emu, new_hub, track):
    a, b = new_hub(2), new_hub(3)
    p = track(EventHubProducerClient.from_connection_string(conn_str(emu, a), eventhub_name=b, **ehutil.CLIENT_KW))
    assert p.eventhub_name == b
    assert p.get_partition_ids() == ["0", "1", "2"]


def test_consumer_with_entity_path_in_the_connection_string(emu, hub, producer, track):
    send_n(producer, 2, partition_id="0")
    c = EventHubConsumerClient.from_connection_string(conn_str(emu, hub), consumer_group="$Default", **ehutil.CLIENT_KW)
    with Collector(c, partition_id="0", starting_position="-1", max_wait_time=1) as col:
        assert bodies(col.wait_events(2)) == ["m-0", "m-1"]


def test_hub_names_may_contain_dots_hyphens_and_underscores(emu, track):
    name = f"my.hub_name-{ehutil.uniq('x')}"
    ehutil.admin_create_hub(emu, name, partitions=1)
    p = track(make_producer(emu, name))
    send_n(p, 1, partition_id="0")
    assert bodies(read_from_start(emu, name, 1, partition_id="0")) == ["m-0"]


# -- credentials -------------------------------------------------------------------------------------------------------------------------------
class FakeTokenCredential:
    """A TokenCredential like azure-identity's: no `token_type`, so the SDK sends the token as a `jwt` claim."""

    def __init__(self):
        self.calls = []

    def get_token(self, *scopes, **kwargs):
        self.calls.append(scopes)
        return AccessToken("not.a.real.jwt", int(time.time()) + 3600)


def sas_token(emu, hub):
    return generate_sas_token(f"sb://{host(emu)}/{hub}", KEY_NAME, KEY, int(time.time()) + 3600)


def credential_variants(emu, hub):
    return {
        "named-key": AzureNamedKeyCredential(KEY_NAME, KEY),
        "sas": AzureSasCredential(sas_token(emu, hub)),
        "shared-key": EventHubSharedKeyCredential(KEY_NAME, KEY),
        "token": FakeTokenCredential(),
    }


@pytest.mark.parametrize("kind", ["named-key", "sas", "shared-key", "token"])
def test_producer_with_a_credential_object(emu, hub, track, kind):
    cred = credential_variants(emu, hub)[kind]
    p = track(EventHubProducerClient(host(emu), hub, cred, use_tls=False, **ehutil.CLIENT_KW))
    assert p.get_partition_ids() == ["0", "1", "2", "3"]
    send_n(p, 2, partition_id="0")
    assert bodies(read_from_start(emu, hub, 2, partition_id="0")) == ["m-0", "m-1"]


@pytest.mark.parametrize("kind", ["named-key", "sas", "token"])
def test_consumer_with_a_credential_object(emu, hub, producer, track, kind):
    send_n(producer, 2, partition_id="1")
    cred = credential_variants(emu, hub)[kind]
    c = EventHubConsumerClient(host(emu), hub, "$Default", cred, use_tls=False, **ehutil.CLIENT_KW)
    with Collector(c, partition_id="1", starting_position="-1", max_wait_time=1) as col:
        assert bodies(col.wait_events(2)) == ["m-0", "m-1"]


def test_token_credential_is_asked_for_the_eventhubs_scope(emu, hub, track):
    cred = FakeTokenCredential()
    p = track(EventHubProducerClient(host(emu), hub, cred, use_tls=False, **ehutil.CLIENT_KW))
    p.get_partition_ids()
    assert cred.calls and all(scopes == ("https://eventhubs.azure.net//.default",) for scopes in cred.calls)


def test_connection_string_with_a_shared_access_signature(emu, hub, track):
    cs = f"Endpoint=sb://{host(emu)};SharedAccessSignature={sas_token(emu, hub)};UseDevelopmentEmulator=true;EntityPath={hub}"
    p = track(EventHubProducerClient.from_connection_string(cs, **ehutil.CLIENT_KW))
    assert p.get_eventhub_properties()["eventhub_name"] == hub


def test_connection_string_key_names_other_than_root_manage_are_accepted(emu, hub, track):
    cs = f"Endpoint=sb://{host(emu)};SharedAccessKeyName=send-only;SharedAccessKey={KEY};UseDevelopmentEmulator=true;EntityPath={hub}"
    p = track(EventHubProducerClient.from_connection_string(cs, **ehutil.CLIENT_KW))
    assert p.get_partition_ids() == ["0", "1", "2", "3"]


def test_endpoint_with_a_trailing_slash(emu, hub, track):
    cs = f"Endpoint=sb://{host(emu)}/;SharedAccessKeyName={KEY_NAME};SharedAccessKey={KEY};UseDevelopmentEmulator=true;EntityPath={hub}"
    p = track(EventHubProducerClient.from_connection_string(cs, **ehutil.CLIENT_KW))
    assert p.get_partition_ids() == ["0", "1", "2", "3"]


def test_client_options_the_sdk_documents_are_accepted(emu, hub, track):
    p = track(make_producer(emu, hub, user_agent="azemu-test", logging_enable=False, retry_mode="fixed", idle_timeout=120, socket_timeout=1))
    assert p.get_partition_ids() == ["0", "1", "2", "3"]
    send_n(p, 1, partition_id="0")  # a link opens on a connection that advertises an idle timeout


# -- lifecycle -----------------------------------------------------------------------------------------------------------------------------------
def test_producer_context_manager(emu, hub):
    with make_producer(emu, hub) as p:
        send_n(p, 2, partition_id="0")
    assert bodies(read_from_start(emu, hub, 2, partition_id="0")) == ["m-0", "m-1"]


def test_producer_close_twice_is_harmless(emu, hub):
    p = make_producer(emu, hub)
    send_n(p, 1, partition_id="0")
    p.close()
    p.close()


def test_consumer_close_twice_is_harmless(emu, hub):
    c = make_consumer(emu, hub)
    c.get_partition_ids()
    c.close()
    c.close()


def test_close_ends_the_receive_loop(emu, hub):
    col = Collector(make_consumer(emu, hub), starting_position="-1", max_wait_time=1).start()
    col.wait_initialized(4)
    col.stop(timeout=8)
    assert not col._thread.is_alive()


def test_consumer_context_manager_closes_the_receive_loop(emu, hub):
    with make_consumer(emu, hub) as c:
        col = Collector(c, partition_id="0", starting_position="-1", max_wait_time=1).start()
        col.wait_initialized(1)
    col._thread.join(8)
    assert not col._thread.is_alive()


def test_several_producers_and_consumers_in_one_process(emu, hub, track):
    producers = [track(make_producer(emu, hub)) for _ in range(3)]
    for i, p in enumerate(producers):
        p.send_event(EventData(f"from-{i}"), partition_id="0")
    assert bodies(read_from_start(emu, hub, 3, partition_id="0")) == ["from-0", "from-1", "from-2"]


# -- connection failures ------------------------------------------------------------------------------------------------------------------------
def dead_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class _Dead:
    port = None

    def __init__(self):
        self.port = dead_port()

    def url(self, label, account, scheme="http"):
        return f"{scheme}://{account}.{label}.localhost:{self.port}"


KW0 = dict(retry_total=0, retry_backoff_factor=0.01, auth_timeout=3)


def test_connection_refused_is_a_connect_error_for_properties():
    dead = _Dead()
    p = EventHubProducerClient.from_connection_string(conn_str(dead), eventhub_name="h", **KW0)
    with pytest.raises(ConnectError):
        p.get_eventhub_properties()
    c = EventHubConsumerClient.from_connection_string(conn_str(dead), consumer_group="$Default", eventhub_name="h", **KW0)
    with pytest.raises(ConnectError):
        c.get_partition_ids()


def test_connection_refused_is_a_connect_error_for_sending():
    # Microsoft's test_send_batch_with_invalid_hostname expects ConnectError for send_batch / create_batch
    dead = _Dead()
    p = EventHubProducerClient.from_connection_string(conn_str(dead), eventhub_name="h", **KW0)
    with pytest.raises(ConnectError):
        p.create_batch()
    with pytest.raises(ConnectError):
        p.send_batch([EventData("x")])
    with pytest.raises(ConnectError):
        p.send_event(EventData("x"))


def test_connection_refused_goes_to_on_error_when_a_callback_is_set():
    dead = _Dead()
    errs = []
    p = EventHubProducerClient.from_connection_string(conn_str(dead), eventhub_name="h", on_error=lambda ev, pid, e: errs.append((ev, pid, e)), **KW0)
    p.send_event(EventData("x"))
    assert len(errs) == 1 and isinstance(errs[0][2], ConnectError) and not errs[0][1]


def test_receive_with_connection_refused_reports_connect_error_to_on_error():
    # Microsoft's test_receive_with_invalid_hostname_sync / test_client_invalid_credential (ConnectError through on_error)
    dead = _Dead()
    c = EventHubConsumerClient.from_connection_string(conn_str(dead), consumer_group="$Default", eventhub_name="h", **KW0)
    with Collector(c, starting_position="-1") as col:
        col.wait_until(lambda x: x.errors, 8, "an on_error call")
        assert isinstance(col.errors[0][1], ConnectError)


# -- AMQP over WebSocket ------------------------------------------------------------------------------------------------------------------------------
def test_amqp_over_websocket(emu, hub, track):
    # Over WebSocket the SDK ignores the port of `Endpoint=` and dials 443 (80 with UseDevelopmentEmulator), so any other port has to
    # be given as `custom_endpoint_address`. More WebSocket cases are in test_websocket.py.
    p = track(EventHubProducerClient.from_connection_string(
        conn_str(emu, hub), transport_type=TransportType.AmqpOverWebsocket,
        custom_endpoint_address=f"http://{ehutil.host(emu, ehutil.NAMESPACE)}/",
        retry_total=0, retry_backoff_factor=0.01, auth_timeout=5))
    assert p.get_partition_ids() == ["0", "1", "2", "3"]
