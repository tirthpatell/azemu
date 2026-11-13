//! Standalone binary: this service only. `azemu-keyvault --port 4580`.

use std::sync::Arc;

use azemu_keyvault::KeyVaultService;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    azemu_core::settings::apply_early_options();
    azemu_core::run_cli(vec![Arc::new(KeyVaultService::new())], vec![]).await
}
