//! Standalone binary: this service only. `azemu-file --port 4580`.

use std::sync::Arc;

use azemu_file::FileService;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    azemu_core::settings::apply_early_options();
    azemu_core::run_cli(vec![Arc::new(FileService::new())], vec![]).await
}
