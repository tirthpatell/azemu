# Contributing

Thanks for helping. azemu is a local emulator of Azure services: the aim is that a program written for Azure runs against it unchanged and that a test passing here means
something. This page is short on purpose.

## Build and test

```bash
cargo build -p azemu                       # debug build: target/debug/azemu (cargo build -p <crate> for one crate)
cargo test -p azemu-<crate>                # the unit and integration tests of one crate
cargo fmt --all                            # the style is in rustfmt.toml; CI checks it
cargo clippy --workspace --all-targets -- -D warnings
.venv/bin/python tests/run_sdk_suites.py --bin target/debug/azemu --only blob     # one SDK suite (see tests/README.md)
```

The workspace needs Rust 1.91 or newer. Build the crate you work on (`-p <crate>`) rather than the whole workspace when you can; a full build takes minutes and a few GB.
`tests/README.md` lists the SDK suites and what each needs; `tools/replay` replays recorded Azure traffic against a build.

## What a change must come with

- **A test.** A Rust test in the crate for the logic, and an SDK test (in `tests/sdk-*`) when the behaviour is something a client sees. Tests for a service come from the SDK and
  Microsoft's documentation, not from reading the service's code.
- **A source for what Azure does.** For behaviour, say where it comes from in the test or in a comment: the REST API specification, Microsoft's documentation, an SDK's source, or a
  recorded real answer. "It seemed right" is not a source.
- **Honest limits.** An operation that is not built answers `501` and names itself (`GET /_azemu/unsupported` lists them), so a test never gets a made-up success.
- **A written guess.** When you could not confirm what Azure does and chose a behaviour, write it in the service's page under "Behaviour that could not be confirmed against Azure"
  (`docs/services/<name>.md`), with the choice and why.
- **Current docs.** Keep the service's page current: what is built, what is not, known differences from Azure.

## Code rules

- No `unwrap()` or `expect()` on anything that comes from a request; no `unsafe`; no new background threads (use tokio tasks).
- Code that runs what a client wrote (Cosmos stored procedures, triggers, user-defined functions) runs in the separate worker process that the emulator can kill, never in the emulator's own process.
- Dependencies: use the shared version from the root `Cargo.toml` (`foo.workspace = true`) when it is listed there. Check a new one's licence against `deny.toml`.
- Keep files under about 800 lines and one file per group of related operations.
- Settings are `AZEMU_*` variables declared with `settings::describe`; `docs/SETTINGS.md` is generated from those calls
  (`AZEMU_UPDATE_SETTINGS_DOC=1 cargo test -p azemu --test settings_doc`).
- Comments say why the code is as it is; they do not narrate the history of the change.

## Pull requests

Describe what changed and why, and say which tests you ran. Small, focused changes are easier to review. By contributing you agree that your work is licensed under the
terms of the project (Apache 2.0 or MIT, at the user's option).
