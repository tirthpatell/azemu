//! Standalone binary: ARM only, no data plane behind it. `azemu-arm --port 4580`.
//! Use `azemu-arm-full` to have ARM create resources in the other services.

use std::sync::Arc;

use azemu_arm::ArmService;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    azemu_core::settings::apply_early_options();
    azemu_core::run_cli(vec![Arc::new(ArmService::standalone())], vec![]).await
}
