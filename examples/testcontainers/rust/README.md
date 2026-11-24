# Testcontainers, Rust

Tests that start `ghcr.io/tirthpatell/azemu:0.3.0` with the `testcontainers` crate and reach it on the published ports: Blob over plain http (`tests/blob.rs`), Key Vault over https with a
token credential (`tests/key_vault.rs`), Service Bus send and receive (`tests/service_bus.rs`), and one container shared by two parallel tests (`tests/shared.rs`). The page that explains
them is [`docs/testcontainers.md`](../../../docs/testcontainers.md).

```bash
cargo test        # Docker must be running; the first run pulls the image and compiles the Azure crates
```

Run with: Rust 1.99, Docker 29.10, `testcontainers` 0.28.0, `azure_storage_blob` 1.1.0, `azure_security_keyvault_secrets` 1.0.1, `azure_identity` 1.0.0, `azservicebus` 0.25.1, azemu 0.3.0 on Linux x86-64.
`tests/service_bus.rs` publishes port 5672 on the same number on the host (the crate cannot dial any other), so no other program may hold that port while it runs.
