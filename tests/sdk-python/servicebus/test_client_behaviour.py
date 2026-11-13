"""ServiceBusClient behaviour: context managers, closing, connection strings, namespaces, factory options."""

import pytest
from azure.core.credentials import AzureNamedKeyCredential
from azure.servicebus import ServiceBusClient, ServiceBusMessage, parse_connection_string
from azure.servicebus.exceptions import ServiceBusError

import sbutil
from sbutil import DLQ, body_of, drain_bodies, receive_n, send_n


# -- context managers and closing -----------------------------------------------------------------
def test_client_context_manager_sender_and_receiver(emu, queue):
    with sbutil.make_client(emu) as client:
        with client.get_queue_sender(queue) as s:
            s.send_messages(ServiceBusMessage("cm"))
        with client.get_queue_receiver(queue, max_wait_time=3) as r:
            (m,) = receive_n(r, 1, max_wait=3)
            assert body_of(m) == "cm"


def test_closing_the_client_closes_its_senders_and_receivers(emu, queue):
    client = sbutil.make_client(emu)
    sender = client.get_queue_sender(queue)
    receiver = client.get_queue_receiver(queue, max_wait_time=1)
    sender.send_messages(ServiceBusMessage("x"))
    client.close()
    with pytest.raises(ValueError):
        sender.send_messages(ServiceBusMessage("y"))
    with pytest.raises(ValueError):
        receiver.receive_messages(max_wait_time=1)


def test_sender_and_receiver_can_be_closed_twice(sb, queue):
    s = sb.get_queue_sender(queue)
    r = sb.get_queue_receiver(queue, max_wait_time=1)
    s.close()
    s.close()
    r.close()
    r.close()


def test_closed_sender_cannot_be_reused_as_context_manager(sb, queue):
    s = sb.get_queue_sender(queue)
    s.close()
    with pytest.raises(ValueError):
        with s:
            pass


def test_new_sender_can_be_made_after_closing_the_old_one(sb, queue):
    for i in range(3):
        with sb.get_queue_sender(queue) as s:
            s.send_messages(ServiceBusMessage(f"gen-{i}"))
    assert drain_bodies(sb, queue) == ["gen-0", "gen-1", "gen-2"]


def test_receiver_reopened_many_times_keeps_getting_messages(sb, queue):
    send_n(sb, queue, 5)
    seen = []
    for _ in range(5):
        with sb.get_queue_receiver(queue, max_wait_time=2) as r:
            seen.extend(body_of(m) for m in receive_n(r, 1))
    assert seen == [f"m-{i}" for i in range(5)]


def test_two_clients_on_one_queue_share_messages(emu, queue):
    with sbutil.make_client(emu) as a, sbutil.make_client(emu) as b:
        sent = send_n(a, queue, 4)
        with b.get_queue_receiver(queue, max_wait_time=2) as r:
            assert [body_of(m) for m in receive_n(r, 4)] == sent


def test_one_client_sends_while_other_receives(emu, queue):
    with sbutil.make_client(emu) as a, sbutil.make_client(emu) as b:
        with b.get_queue_receiver(queue, max_wait_time=3) as r:
            assert r.receive_messages(max_message_count=1, max_wait_time=1) == []
            send_n(a, queue, 1)
            assert len(receive_n(r, 1, max_wait=3)) == 1


def test_several_senders_from_one_client_to_one_queue(sb, queue):
    with sb.get_queue_sender(queue) as s1, sb.get_queue_sender(queue) as s2:
        s1.send_messages(ServiceBusMessage("from-1"))
        s2.send_messages(ServiceBusMessage("from-2"))
    assert sorted(drain_bodies(sb, queue)) == ["from-1", "from-2"]


# -- factory options -------------------------------------------------------------------------------
def test_sender_and_receiver_accept_client_identifier_and_socket_timeout(sb, queue):
    with sb.get_queue_sender(queue, client_identifier="my-sender", socket_timeout=0.5) as s:
        s.send_messages(ServiceBusMessage("id"))
    with sb.get_queue_receiver(queue, client_identifier="my-receiver", max_wait_time=3) as r:
        assert len(receive_n(r, 1, max_wait=3)) == 1


def test_send_with_explicit_timeout(sb, queue):
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("timed"), timeout=10)
    assert drain_bodies(sb, queue) == ["timed"]


def test_receive_mode_enum_and_string_both_work(sb, queue):
    send_n(sb, queue, 2)
    with sb.get_queue_receiver(queue, receive_mode=sbutil.PEEK_LOCK, max_wait_time=2) as r:
        assert len(receive_n(r, 1)) == 1
    with sb.get_queue_receiver(queue, receive_mode="peeklock", max_wait_time=2) as r:
        assert len(receive_n(r, 1)) == 1


def test_receive_mode_and_sub_queue_are_case_sensitive(sb, queue):
    # Microsoft's test_queue_by_servicebus_client_enum_case_sensitivity
    with pytest.raises(ValueError):
        sb.get_queue_receiver(queue, receive_mode="RECEIVEANDDELETE")
    with pytest.raises(ValueError):
        sb.get_queue_receiver(queue, sub_queue="DEADLETTER")


def test_receiver_max_wait_time_applies_to_receive_calls(sb, queue):
    import time

    with sb.get_queue_receiver(queue, max_wait_time=1) as r:
        start = time.monotonic()
        assert r.receive_messages(max_message_count=1) == []
        assert time.monotonic() - start < 8


def test_receive_messages_rejects_non_positive_max_wait_time(sb, queue):
    with sb.get_queue_receiver(queue, max_wait_time=1) as r:
        with pytest.raises(ValueError):
            r.receive_messages(max_wait_time=0)


def test_subscription_receiver_options(sb, topic_sub):
    topic, sub = topic_sub
    send_n(sb, topic, 2, topic=True)
    with sb.get_subscription_receiver(topic, sub, receive_mode="receiveanddelete", prefetch_count=2, max_wait_time=2) as r:
        assert len(receive_n(r, 2, settle=None)) == 2


def test_dead_letter_sub_queue_option_on_queue_receiver(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        r.dead_letter_message(m)
    assert len(drain_bodies(sb, queue, sub_queue=DLQ, max_wait=2)) == 1


# -- connection strings ----------------------------------------------------------------------------
def test_connection_string_parse_keeps_host_and_port(emu):
    props = parse_connection_string(sbutil.conn_str(emu))
    assert props.fully_qualified_namespace == sbutil.host(emu)
    assert props.endpoint == f"sb://{sbutil.host(emu)}/"
    assert props.shared_access_key_name == sbutil.KEY_NAME
    assert props.shared_access_key == sbutil.KEY
    assert props.entity_path is None


def test_connection_string_parse_entity_path(emu):
    assert parse_connection_string(sbutil.conn_str(emu, entity="myq")).entity_path == "myq"


@pytest.mark.parametrize(
    "bad",
    ["", "Endpoint=sb://x.servicebus.localhost", "SharedAccessKeyName=a;SharedAccessKey=b", "garbage",
     "Endpoint=sb://h;SharedAccessKeyName=a"],
    ids=["blank", "no-key", "no-endpoint", "no-equals", "key-name-without-key"],
)
def test_malformed_connection_strings_raise_value_error(bad):
    with pytest.raises(ValueError):
        ServiceBusClient.from_connection_string(bad)


def test_connection_string_with_entity_path_for_the_queue(emu, queue):
    cs = sbutil.conn_str(emu, entity=queue)
    with ServiceBusClient.from_connection_string(cs, **sbutil.CLIENT_KW) as client:
        with client.get_queue_sender(queue) as s:
            s.send_messages(ServiceBusMessage("scoped"))
        assert drain_bodies(client, queue) == ["scoped"]


def test_connection_string_entity_path_must_match_queue_name(emu, queue):
    cs = sbutil.conn_str(emu, entity=queue)
    with ServiceBusClient.from_connection_string(cs, **sbutil.CLIENT_KW) as client:
        with pytest.raises(ValueError):
            client.get_queue_sender("some-other-queue")


def test_named_key_credential_with_use_tls_false(emu, queue):
    cred = AzureNamedKeyCredential(sbutil.KEY_NAME, sbutil.KEY)
    with ServiceBusClient(sbutil.host(emu), cred, use_tls=False, **sbutil.CLIENT_KW) as client:
        send_n(client, queue, 1)
        assert drain_bodies(client, queue) == ["m-0"]


def test_ip_address_endpoint_lands_in_namespace_default(emu, queue):
    # open.hostname is "127.0.0.1": an IP means namespace `default`, where the admin client created the queue
    cs = f"Endpoint=sb://127.0.0.1:{emu.port};SharedAccessKeyName=k;SharedAccessKey={sbutil.KEY};UseDevelopmentEmulator=true"
    with ServiceBusClient.from_connection_string(cs, **sbutil.CLIENT_KW) as client:
        sent = send_n(client, queue, 2)
        assert drain_bodies(client, queue) == sent


def test_any_shared_access_key_is_accepted(emu, queue):
    cs = f"Endpoint=sb://{sbutil.host(emu)};SharedAccessKeyName=whatever;SharedAccessKey=bm90YXJlYWxrZXk=;UseDevelopmentEmulator=true"
    with ServiceBusClient.from_connection_string(cs, **sbutil.CLIENT_KW) as client:
        send_n(client, queue, 1)
        assert len(drain_bodies(client, queue)) == 1


# -- namespaces ---------------------------------------------------------------------------------------
def test_namespace_comes_from_the_host_name_in_the_open_frame(emu, admin, queue):
    other_admin = sbutil.make_admin(emu, "other")
    other_admin.create_queue(queue)  # same queue name in namespace `other`
    try:
        with sbutil.make_client(emu) as default, sbutil.make_client(emu, "other") as other:
            send_n(other, queue, 2, prefix="other")
            assert drain_bodies(default, queue) == []
            assert drain_bodies(other, queue) == ["other-0", "other-1"]
    finally:
        other_admin.close()


def test_messages_in_different_namespaces_do_not_mix(emu, queue):
    sbutil.make_admin(emu, "ns2").create_queue(queue)
    with sbutil.make_client(emu) as a, sbutil.make_client(emu, "ns2") as b:
        send_n(a, queue, 1, prefix="in-default")
        send_n(b, queue, 1, prefix="in-ns2")
        assert drain_bodies(a, queue) == ["in-default-0"]
        assert drain_bodies(b, queue) == ["in-ns2-0"]


def test_service_bus_error_is_the_base_class_of_messaging_errors():
    from azure.servicebus import exceptions as ex

    for cls in (ex.MessageLockLostError, ex.SessionLockLostError, ex.MessagingEntityNotFoundError,
                ex.OperationTimeoutError, ex.SessionCannotBeLockedError, ex.MessageSizeExceededError):
        assert issubclass(cls, ServiceBusError)
