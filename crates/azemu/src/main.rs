//! The `azemu` binary: every service mounted on one port. How the services are wired together is in `lib.rs`.

fn main() -> anyhow::Result<()> {
    // `azemu __healthcheck [port]`: what the container's HEALTHCHECK runs; answers and exits before anything is built
    azemu_core::health_check_if_requested();
    azemu_core::settings::apply_early_options();
    // Cosmos scripts run in a worker process that is this same program started with a hidden first argument: before anything else
    azemu_cosmos::script_worker_main_if_requested();
    serve()
}

#[tokio::main(flavor = "multi_thread")]
async fn serve() -> anyhow::Result<()> {
    let stack = azemu::full_stack();
    azemu_core::run_cli(stack.services, stack.raw).await
}
