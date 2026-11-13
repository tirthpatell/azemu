//! Standalone binary: Blob and Data Lake Gen2 (they share their data). `azemu-blob --port 4580`.

use std::sync::Arc;

use azemu_blob::BlobService;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    azemu_core::settings::apply_early_options();
    let blob = BlobService::new();
    let dfs = blob.datalake();
    azemu_core::run_cli(vec![Arc::new(blob), Arc::new(dfs)], vec![]).await
}
