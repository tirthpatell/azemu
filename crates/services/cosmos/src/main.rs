//! Standalone binary: this service only. `azemu-cosmos --port 4580`.

use std::sync::Arc;

use azemu_cosmos::CosmosService;

fn main() -> anyhow::Result<()> {
    azemu_core::settings::apply_early_options();
    // scripts run in a worker process that is this same program started with a hidden first argument: before anything else
    azemu_cosmos::script_worker_main_if_requested();
    serve()
}

#[tokio::main(flavor = "multi_thread")]
async fn serve() -> anyhow::Result<()> {
    azemu_core::run_cli(vec![Arc::new(CosmosService::new())], vec![]).await
}
