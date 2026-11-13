//! Standalone binary: Event Hubs only (every AMQP connection is an Event Hubs connection), with the Kafka endpoint.
//! `azemu-eventhubs --port 4580`; `--pin kafka=9093` for a Kafka port of its own.

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    azemu_core::settings::apply_early_options();
    let (service, amqp) = azemu_eventhubs::pair();
    let kafka = amqp.kafka();
    azemu_core::run_cli(vec![service, kafka.clone()], vec![amqp, kafka]).await
}
