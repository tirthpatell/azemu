//! Runs only the script worker: `azemu-cosmos-script-worker __cosmos-script-worker <settings json>`.
//! Used by the tests of this crate (`CARGO_BIN_EXE_azemu-cosmos-script-worker`) and by embedders (`AZEMU_SCRIPT_WORKER`).

fn main() {
    azemu_cosmos::script_worker_main(std::env::args().skip(1).collect());
}
