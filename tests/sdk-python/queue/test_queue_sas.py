"""Shared access signatures: queue SAS and account SAS, permissions and expiry."""

from datetime import timedelta

import pytest
from azure.core.exceptions import ClientAuthenticationError, HttpResponseError
from azure.storage.queue import (
    AccountSasPermissions,
    QueueClient,
    QueueServiceClient,
    QueueSasPermissions,
    ResourceTypes,
    generate_account_sas,
    generate_queue_sas,
)

from queue_support import assert_storage_error, parse_conn_str, utc_now


def _account(emu):
    parts = parse_conn_str(emu.storage_conn_str())
    return parts["AccountName"], parts["AccountKey"]


def _queue_client_with_sas(emu, service, queue_name, permission, expiry=None):
    account, key = _account(emu)
    sas = generate_queue_sas(
        account,
        queue_name,
        key,
        permission=permission,
        expiry=expiry or utc_now() + timedelta(hours=1),
    )
    return QueueClient(account_url=service.url, queue_name=queue_name, credential=sas)


def test_queue_sas_with_add_read_and_process_can_send_and_receive(emu, service, queue, queue_name):
    client = _queue_client_with_sas(
        emu, service, queue_name, QueueSasPermissions(add=True, read=True, process=True)
    )

    client.send_message("via-sas")
    got = client.receive_message()

    assert got.content == "via-sas"


def test_queue_sas_without_add_cannot_send_403(emu, service, queue, queue_name):
    client = _queue_client_with_sas(emu, service, queue_name, QueueSasPermissions(read=True, process=True))

    with pytest.raises(HttpResponseError) as exc:
        client.send_message("nope")

    assert_storage_error(exc.value, 403, "AuthorizationPermissionMismatch")


def test_queue_sas_without_process_cannot_receive_403(emu, service, queue, queue_name):
    client = _queue_client_with_sas(emu, service, queue_name, QueueSasPermissions(add=True, read=True))

    with pytest.raises(HttpResponseError) as exc:
        client.receive_message()

    assert_storage_error(exc.value, 403, "AuthorizationPermissionMismatch")


def test_queue_sas_without_read_cannot_peek_403(emu, service, queue, queue_name):
    client = _queue_client_with_sas(emu, service, queue_name, QueueSasPermissions(add=True, process=True))

    with pytest.raises(HttpResponseError) as exc:
        client.peek_messages()

    assert_storage_error(exc.value, 403, "AuthorizationPermissionMismatch")


def test_expired_queue_sas_is_rejected_403_authentication_failed(emu, service, queue, queue_name):
    client = _queue_client_with_sas(
        emu,
        service,
        queue_name,
        QueueSasPermissions(add=True, read=True, process=True),
        expiry=utc_now() - timedelta(minutes=10),
    )

    with pytest.raises(ClientAuthenticationError) as exc:
        client.send_message("too-late")

    assert_storage_error(exc.value, 403, "AuthenticationFailed")


def test_account_sas_with_add_read_and_process_can_send_and_receive(emu, service, queue, queue_name):
    account, key = _account(emu)
    sas = generate_account_sas(
        account,
        key,
        resource_types=ResourceTypes(object=True),
        permission=AccountSasPermissions(add=True, read=True, process=True),
        expiry=utc_now() + timedelta(hours=1),
    )
    client = QueueServiceClient(account_url=service.url, credential=sas).get_queue_client(queue_name)

    client.send_message("account-sas")

    assert client.receive_message().content == "account-sas"

