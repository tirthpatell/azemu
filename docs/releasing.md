# Releasing azemu

For maintainers and contributors: what the release build is and why, and the steps to cut a release. How users install it is in [`install.md`](install.md).

## The release build

The profile is in the root `Cargo.toml`; the reasons are in its comments and here.

| Setting | Value | Why |
|---|---|---|
| `lto` | `"thin"` | the build that was tested. Full link-time optimisation with one code unit (`lto = "fat"`, `codegen-units = 1`) links the whole program in one piece, which needs much more memory, and the binary already starts in about 7 ms |
| `codegen-units` | `4` | same |
| `strip` | `true` | no symbols in the file; it is about 36 MB stripped |
| `panic` | `"unwind"` (the default, written down) | the emulator turns a panic in one request handler into a 500 for that request and goes on; `"abort"` would end the whole emulator, and with it every other test that is using it |
| `overflow-checks` | `true` | an integer that overflows then panics (one request gets a 500) instead of wrapping to a wrong number that is stored. Measured cost: about 1 to 2% of run time on a mixed HTTP load, inside the spread between runs, and 2% of binary size |
| `debug-assertions` | off (default) | they are for development builds |

**Features** (`crates/azemu/Cargo.toml`), both on by default:

| Feature | What it holds | With it off |
|---|---|---|
| `cosmos-scripts` | the JavaScript engine (`boa_engine`) for stored procedures, triggers and user-defined functions; the script worker process | saves about 27% of the binary. Running a script answers 501 `NotImplemented` and says the build has no script engine; storing the scripts and everything else works |
| `kafka` | the Kafka endpoint of Event Hubs | saves about 1.4%. A stand-in answers 501 on `/_azemu/kafka/...` and on the Kafka port; a Kafka client that connects is closed with one log line saying the build has no Kafka |

Neither (`--no-default-features`) gives a binary about 28% smaller. The crates' own tests need the default features (the Cosmos script tests start the engine); build the smaller
binary in a separate target directory (`CARGO_TARGET_DIR=...`) so it does not replace the `target/debug/azemu` that the test suites use. The per-service binaries (`azemu-blob`, ...)
are development tools and are not released.

## The image

The `Dockerfile` has two stages: `rust` (a pinned version) builds `cargo build --release --locked -p azemu`; the runtime stage is a distroless base with the binary, the licence
files in `/licenses`, an empty `/data` and certificate directory, user 10001, a `HEALTHCHECK` (`azemu __healthcheck`, so no curl), `STOPSIGNAL SIGTERM`, the standard ports in
`EXPOSE`, OCI labels, and the default command `--host 0.0.0.0 --standard-ports`. `.dockerignore` lets only `Cargo.toml`, `Cargo.lock`, `crates`, `.git` and the licence
files into the build context, so a new top-level directory is never sent by accident.

## Cutting a release

1. **Prepare.** Update the version in the root `Cargo.toml` (`[workspace.package]`), add the entry to `CHANGELOG.md`, run `cargo update --workspace` only if `Cargo.lock` has to
   change (the build uses `--locked`). Regenerate the dependency list: `python3 tools/release/third_party_licenses.py > THIRD-PARTY-LICENSES.md`, and run
   `cargo deny check licenses sources` (install once with `cargo install --locked cargo-deny`).
2. **Commit and tag.** Commit, then `git tag -a vX.Y.Z -m "azemu X.Y.Z"`. A binary built outside a git checkout says `commit unknown`; build the release from the tagged commit so that
   `azemu --version` names it.
3. **Build the binary.** `cargo build --release --locked -p azemu`.
4. **Run every suite against that binary.** `tests/run_all.sh <binary>` runs the Python, replay, Rust, JavaScript, Go, Java and .NET suites (about an hour; run it alone). Any suite that
   fails in release and passes in debug is a finding, not a flake. `SCORES_DIR=/some/dir` keeps the replay from rewriting `fidelity/SCORES.md`;
   `python3 tools/release/check_scores.py /some/dir/SCORES.md` says whether a score fell.
5. **Build the image from a clean checkout** and test that image: `docker build -t azemu:X.Y.Z -t azemu:latest .`, then `AZEMU_IMAGE=azemu:X.Y.Z tests/container/run.sh`. Check
   `docker image inspect azemu:X.Y.Z` (size, labels).
6. **Publish.** Push the tag. `.github/workflows/release.yml` runs on a `v*` tag (run by hand on a branch it is a trial that publishes nothing). It builds the binary for Linux
   x86-64 and arm64 (glibc 2.35 or newer), Linux x86-64 static (musl) and macOS arm64 (a failure of the macOS job does not stop the release), smoke-tests each one
   (`tools/release/smoke_test.py`), packs them with the licence files (`tools/release/package.sh`, `checksums.sh`), builds the image for `linux/amd64` and `linux/arm64`, pushes
   it to the GitHub Container Registry, and creates the GitHub release with the changelog section as its text. The first time, set the image's package visibility to public
   in the repository's package settings.
7. **Afterwards.** Bump to the next development version if you want one.

## The workflows

- `ci.yml`: on pull requests and pushes to `main`: formatting, clippy, the crates' tests, a release build with a smoke test, the container test, the licence
  checks, the fast Python suites, and the Go and Rust SDK suites. Started by hand (Run workflow) it also runs the Service Bus and Event Hubs Python suites and the
  JavaScript, Java and .NET suites: run it that way before a release. Nothing runs on a schedule.
- `replay.yml`: by hand, and on pushes to `main` that touch the emulator: fetches Microsoft's recordings (`tools/fetch-refs.py`, pinned tags), replays them, and fails
  when a score falls below `fidelity/SCORES.md`.
- `release.yml`: above.

## Other targets

`tools/release/Dockerfile.musl` builds a static binary (`rust:<version>-alpine`); `tools/release/Dockerfile.cross-arm64` cross-compiles for arm64 with `gcc-aarch64-linux-gnu`. See
[`install.md`](install.md) for what has been run on each.

## Publishing to crates.io

Not planned: the project is released through GitHub only. What would stop it anyway: the workspace crates depend on each other by `path` with no version (`cargo publish` needs a
published version of every one, in order), and `crates/core/build.rs` reads `../../.git`, which a published crate does not have (the commit would say `unknown`).
