"""AMQP inside TLS on the same port as plain AMQP (certificate from the emulator's CA, `emu.ca_file`).

The SDK dials `default.servicebus.localhost:<port>` (from `custom_endpoint_address`), checks the server certificate
against `connection_verify` for that name, then runs the same SASL ANONYMOUS / AMQP open / $cbs dance as without TLS.
`transport_type=TransportType.Amqp` is needed because a custom endpoint address otherwise selects AMQP over WebSocket.
"""

import pytest
from azure.core.credentials import AzureNamedKeyCredential
from azure.servicebus import ServiceBusClient, ServiceBusMessage, TransportType
from azure.servicebus.exceptions import ServiceBusError

import sbutil
from sbutil import body_of, drain_bodies, receive_n, send_n


def tls_client(emu, namespace="default", **kw):
    host = sbutil.host(emu, namespace)
    kwargs = {
        **sbutil.CLIENT_KW,
        "custom_endpoint_address": f"sb://{host}",
        "transport_type": TransportType.Amqp,
        "connection_verify": emu.ca_file,
        **kw,
    }
    return ServiceBusClient(host.split(":")[0], AzureNamedKeyCredential(sbutil.KEY_NAME, sbutil.KEY), **kwargs)


def test_send_and_receive_over_tls(emu, queue):
    with tls_client(emu) as client:
        with client.get_queue_sender(queue) as s:
            s.send_messages(ServiceBusMessage("secure"))
        with client.get_queue_receiver(queue, max_wait_time=3) as r:
            (m,) = receive_n(r, 1, max_wait=3)
            assert body_of(m) == "secure"


def test_management_operations_work_over_tls(emu, queue):
    # peek / schedule / cancel go through the $management link
    from datetime import timedelta

    with tls_client(emu) as client:
        send_n(client, queue, 2)
        with client.get_queue_receiver(queue, max_wait_time=2) as r:
            assert len(r.peek_messages(max_message_count=5)) == 2
        with client.get_queue_sender(queue) as s:
            seqs = s.schedule_messages(ServiceBusMessage("x"), sbutil.utcnow() + timedelta(seconds=120))
            s.cancel_scheduled_messages(seqs)


def test_sessions_work_over_tls(emu, session_queue):
    with tls_client(emu) as client:
        with client.get_queue_sender(session_queue) as s:
            s.send_messages(ServiceBusMessage("in-session", session_id="tls"))
        with client.get_queue_receiver(session_queue, session_id="tls", max_wait_time=3) as r:
            (m,) = receive_n(r, 1, max_wait=3)
            assert m.session_id == "tls"
            r.session.set_state("tls-state")
            assert r.session.get_state() == b"tls-state"


def test_tls_and_plain_clients_see_the_same_namespace(emu, queue):
    with tls_client(emu) as secure, sbutil.make_client(emu) as plain:
        sent = send_n(secure, queue, 2)
        assert drain_bodies(plain, queue) == sent


def test_tls_client_for_another_namespace(emu, admin, queue):
    other_admin = sbutil.make_admin(emu, "other")
    other_admin.create_queue(queue)
    try:
        with tls_client(emu, "other") as client:
            send_n(client, queue, 1, prefix="o")
            assert drain_bodies(client, queue) == ["o-0"]
    finally:
        other_admin.close()


def test_tls_with_untrusted_ca_is_refused(emu, queue):
    host = sbutil.host(emu)
    client = ServiceBusClient(
        host.split(":")[0],
        AzureNamedKeyCredential(sbutil.KEY_NAME, sbutil.KEY),
        custom_endpoint_address=f"sb://{host}",
        transport_type=TransportType.Amqp,
        retry_total=0,
        # no connection_verify: the system CA bundle does not know the emulator CA
    )
    with client:
        with pytest.raises(ServiceBusError):
            with client.get_queue_sender(queue) as s:
                s.send_messages(ServiceBusMessage("x"))
