from azure.servicebus import ServiceBusClient, ServiceBusMessage
from azure.servicebus.management import ServiceBusAdministrationClient

from conftest import SERVICE_BUS


# AMQP on the published port, in plain text (UseDevelopmentEmulator=true). The administration client always speaks
# https to the same host and port, so it gets the CA.
def test_service_bus_send_and_receive(emulator, tmp_path):
    connection_string = (
        f"Endpoint=sb://{emulator.host}:{emulator.port(SERVICE_BUS)};"
        "SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any;UseDevelopmentEmulator=true"
    )

    # Nothing exists until it is created, as on Azure.
    admin = ServiceBusAdministrationClient.from_connection_string(connection_string, connection_verify=emulator.ca_file(tmp_path))
    admin.create_queue("commands")

    with ServiceBusClient.from_connection_string(connection_string) as client:
        with client.get_queue_sender("commands") as sender:
            sender.send_messages(ServiceBusMessage("ship order 1"))
        with client.get_queue_receiver("commands", max_wait_time=10) as receiver:
            (message,) = receiver.receive_messages(max_message_count=1)
            assert str(message) == "ship order 1"
            receiver.complete_message(message)
