# Testcontainers, Go

Tests that start `ghcr.io/tirthpatell/azemu:0.3.0` with `testcontainers-go` and reach it on the published ports: Blob over plain http with the Azurite connection string (`blob_test.go`), Key
Vault over https with a token credential (`keyvault_test.go`), Service Bus send and receive (`servicebus_test.go`). `shared/` has two tests that share one container and empty it after each
test. `emulator/` is the helper that starts the container. The page that explains them is [`docs/testcontainers.md`](../../../docs/testcontainers.md).

```bash
go test ./...        # Docker must be running; the first run pulls the image
```

Run with: Go 1.27, Docker 29.10, `testcontainers-go` 0.45.0, `azblob` 1.8.3, `azsecrets` 1.5.1, `azidentity` 1.15.0, `azservicebus` 1.11.0, azemu 0.3.0 on Linux x86-64.
