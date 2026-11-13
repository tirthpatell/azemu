"""Which tests run depends on whether the emulator creates things that are only named.

The emulator does not create queues, event hubs, consumer groups or Event Grid topics by being named, as Azure does not.
So the tests of a suite run against the default emulator and create what they use. A few tests are about auto-creation
itself: they carry the marker `autocreate` and run only when the emulator is started with the service's variable
(`AZEMU_SERVICEBUS_AUTOCREATE=1`, `AZEMU_EVENTHUBS_AUTOCREATE=1`, `AZEMU_EVENTGRID_AUTOCREATE=1`) or `AZEMU_LENIENT=1`;
the runner `tests/run_sdk_suites.py` does that as a second pass for them. Tests that carry `no_autocreate` check what Azure
answers for something missing and run in the default pass; they are skipped when auto-creation is on.

The test process and the emulator it starts see the same environment, so the variable that gates the tests is also the one
that configures the emulator.

Blob versioning works the same way with its own pair of markers: an account that appears on first use keeps
no versions, as on Azure. A test that needs versions in an account the test does not create through ARM carries
`versioning` and runs only when the emulator is started with `AZEMU_BLOB_VERSIONING=1` (or `AZEMU_LENIENT=1`); the runner
does that as a second pass. A test that checks the default (no `x-ms-version-id`) carries `no_versioning`. Where the suite
has ARM (`arm`, `persistence`, `storage_events`: the unified binary) a test turns versioning on for its own account through
`blobServices/default` instead and needs no marker.
"""

import os

import pytest

_ON = {"1", "true", "yes", "on"}
_OFF = {"0", "false", "no", "off"}


def _switch(name: str):
    value = os.environ.get(name, "").strip().lower()
    if value in _ON:
        return True
    if value in _OFF:
        return False
    return None


def autocreate_on(variable: str) -> bool:
    """Is auto-creation on for the emulator this test run starts? Its own variable wins, then `AZEMU_LENIENT`."""
    own = _switch(variable)
    return own if own is not None else bool(_switch("AZEMU_LENIENT"))


def add_markers(config) -> None:
    config.addinivalue_line(
        "markers",
        "autocreate: about auto-creation itself; runs only when the emulator is started with the service's AUTOCREATE variable "
        "(or AZEMU_LENIENT=1), skipped otherwise",
    )
    config.addinivalue_line(
        "markers",
        "no_autocreate: what Azure answers for something that was never created; runs by default, skipped when auto-creation is on",
    )


def gate(items, variable: str) -> None:
    on = autocreate_on(variable)
    skip_off = pytest.mark.skip(reason=f"about auto-creation: needs {variable}=1 (the runner's second pass sets it)")
    skip_on = pytest.mark.skip(reason=f"a missing entity is an error only while {variable} is off")
    for item in items:
        if not on and item.get_closest_marker("autocreate"):
            item.add_marker(skip_off)
        if on and item.get_closest_marker("no_autocreate"):
            item.add_marker(skip_on)


VERSIONING_VARIABLE = "AZEMU_BLOB_VERSIONING"


def versioning_on() -> bool:
    """Is blob versioning on for new accounts of the emulator this test run starts? Its own variable wins, then `AZEMU_LENIENT`."""
    return autocreate_on(VERSIONING_VARIABLE)


def add_versioning_markers(config) -> None:
    config.addinivalue_line(
        "markers",
        "versioning: needs blob versioning on for the account (the runner's second pass starts the emulator with AZEMU_BLOB_VERSIONING=1)",
    )
    config.addinivalue_line(
        "markers",
        "no_versioning: what Azure does on an account without versioning (the default); skipped when versioning is on",
    )


def gate_versioning(items) -> None:
    on = versioning_on()
    skip_off = pytest.mark.skip(reason=f"needs blob versioning: {VERSIONING_VARIABLE}=1 (the runner's second pass sets it)")
    skip_on = pytest.mark.skip(reason=f"checks an account without versioning: skipped while {VERSIONING_VARIABLE} is on")
    for item in items:
        if not on and item.get_closest_marker("versioning"):
            item.add_marker(skip_off)
        if on and item.get_closest_marker("no_versioning"):
            item.add_marker(skip_on)
