# Tests

How the emulator is checked. Every suite below starts its own copy of the emulator on a free port, drives it with the official SDK or tool of one kind, and stops it.
The suites are also working examples of pointing each SDK at the emulator; [`docs/sdk-notes.md`](../docs/sdk-notes.md) collects what each one needs.

| Directory | What it is | Needs |
|---|---|---|
| [`sdk-python/`](sdk-python/README.md) | the Python SDKs, every service | Python 3.13 and `requirements.txt` |
| [`sdk-js/`](sdk-js/README.md) | the JavaScript SDKs on Node | Node 22+ |
| [`sdk-go/`](sdk-go/README.md) | the Go SDK modules | Go |
| [`sdk-java/`](sdk-java/README.md) | the Java SDKs | JDK 17+, Maven |
| [`sdk-dotnet/`](sdk-dotnet/README.md) | the .NET SDKs | .NET 10 SDK |
| [`sdk-rust/`](sdk-rust/README.md) | the Rust SDK crates | Rust, OpenSSL headers |
| [`functions/`](functions/README.md) | the Azure Functions host (Core Tools) against the emulator | .NET 10 SDK, Core Tools 4, Node |
| [`terraform/`](terraform/README.md) | the `azurerm` Terraform provider through Resource Manager | Terraform |
| [`container/`](container/README.md) | the container image, reached by name from a second container | Docker |

`run_all.sh [binary]` runs the Python, replay, Rust, JavaScript, Go, Java and .NET suites one after the other against one binary and prints a summary (about an hour; run it
alone; `SKIP="java rust"` leaves suites out). `run_sdk_suites.py` runs the Python suites in parallel. The replay of recorded Azure traffic is in
[`tools/replay`](../tools/replay/README.md). The emulator's own unit and integration tests are `cargo test -p azemu-<crate>` (or `cargo test --workspace`).

A suite needs a built emulator: `cargo build -p azemu` gives `target/debug/azemu`, or use a release binary.
