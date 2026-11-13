"""Where the emulator binary is: one answer for every test that starts an emulator.

Order, for the unified binary (`azemu`, every service in one program):

    AZEMU_UNIFIED_BIN or AZEMU_FULL_BIN   an explicit path (older names, still honoured)
    AZEMU_BIN                             what `tests/run_sdk_suites.py --bin` and `tests/run_all.sh` set,
                                          unless it names one of the per-service development binaries
    target/release/azemu, target/debug/azemu   whichever exists, the newer one first

For a single service the per-service development binary (`target/debug/azemu-<service>`) is used when
it exists and nothing says otherwise; the unified binary serves every service, so it is the fallback.
"""

import os
import pathlib

ROOT = pathlib.Path(__file__).resolve().parents[2]

_SERVICES = (
    "blob", "queue", "table", "keyvault", "appconfig", "identity", "cosmos", "eventgrid",
    "servicebus", "eventhubs", "arm", "cosmos-script-worker",
)


def _is_service_binary(path: str) -> bool:
    name = pathlib.Path(path).name
    return any(name == f"azemu-{s}" for s in _SERVICES)


def _built(name: str):
    """`target/release/<name>` or `target/debug/<name>`, the newer of the two; None if neither exists."""
    found = [p for p in (ROOT / "target" / "release" / name, ROOT / "target" / "debug" / name) if p.exists()]
    if not found:
        return None
    return str(max(found, key=lambda p: p.stat().st_mtime))


def unified():
    """Path of the unified binary, or None when there is none."""
    for var in ("AZEMU_UNIFIED_BIN", "AZEMU_FULL_BIN"):
        if os.environ.get(var):
            return os.environ[var]
    explicit = os.environ.get("AZEMU_BIN")
    if explicit and not _is_service_binary(explicit):
        return explicit
    return _built("azemu")


def for_service(service: str):
    """Path of a binary that serves `service`, or None: AZEMU_BIN, the service's own binary, the unified one."""
    explicit = os.environ.get("AZEMU_BIN")
    if explicit:
        return explicit
    return _built(f"azemu-{service}") or unified()


def missing_message(what: str = "azemu") -> str:
    return (f"no emulator binary found: build one with `cargo build -p {what}` (or `cargo build --release -p azemu`), "
            "or set AZEMU_BIN to the path of an azemu binary")
