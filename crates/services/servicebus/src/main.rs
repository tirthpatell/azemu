//! Standalone binary: Service Bus only. `azemu-servicebus --port 4580`.

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    azemu_core::settings::apply_early_options();
    let (service, amqp) = azemu_servicebus::pair();
    azemu_core::run_cli(vec![service], vec![amqp]).await
}
