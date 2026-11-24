//! Service Bus over AMQP. The unofficial `azservicebus` crate, with its `unsecured` feature, speaks plain AMQP. It builds
//! the address from the host name alone and dials port 5672 whatever the connection string says, so this is the one test
//! that publishes a container port on the same number on the host.
mod common;

use azservicebus::{ServiceBusClient, ServiceBusClientOptions, ServiceBusMessage, ServiceBusReceiverOptions, ServiceBusSenderOptions};
use common::{Emulator, SERVICE_BUS};

#[tokio::test]
async fn service_bus_send_and_receive() {
    let emulator = Emulator::start_on_standard_service_bus_port().await;
    let port = emulator.port(SERVICE_BUS).await; // 5672

    // Nothing exists until it is created, as on Azure. The Rust SDK has no administration client: the same call
    // the others make is one HTTP request (plain http works on this port).
    let entry = r#"<entry xmlns="http://www.w3.org/2005/Atom"><content type="application/xml"><QueueDescription xmlns="http://schemas.microsoft.com/netservices/2010/10/servicebus/connect"/></content></entry>"#;
    let response = reqwest::Client::new()
        .put(format!("http://{}:{port}/commands?api-version=2021-05", emulator.host))
        .header("Authorization", "Bearer any")
        .header("Content-Type", "application/atom+xml")
        .body(entry)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);

    // The host "localhost" is the namespace "default".
    let connection = format!("Endpoint=sb://{}:{port};SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any", emulator.host);
    let mut client =
        ServiceBusClient::unsecured().new_from_connection_string(connection, ServiceBusClientOptions::default()).await.unwrap();
    let mut sender = client.create_sender("commands", ServiceBusSenderOptions::default()).await.unwrap();
    sender.send_message(ServiceBusMessage::new("ship order 1")).await.unwrap();
    let mut receiver = client.create_receiver_for_queue("commands", ServiceBusReceiverOptions::default()).await.unwrap();
    let message = receiver.receive_message().await.unwrap();
    assert_eq!(message.body().unwrap(), b"ship order 1");
    receiver.complete_message(&message).await.unwrap();

    sender.dispose().await.unwrap();
    receiver.dispose().await.unwrap();
    client.dispose().await.unwrap();
}
