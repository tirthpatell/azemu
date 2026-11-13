//! Standalone binary: this service only. `azemu-queue --port 4580`.

use std::sync::Arc;

use azemu_queue::QueueService;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    azemu_core::settings::apply_early_options();
    azemu_core::run_cli(vec![Arc::new(QueueService::new())], vec![]).await
}
