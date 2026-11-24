"""Live Metrics (QuickPulse). With `enable_live_metrics` the Python exporter pings `{LiveEndpoint}/QuickPulseService.svc/ping`
(the REST form, `api-version=2024-04-01-preview`). The emulator answers "nobody is watching" and the exporter goes on pinging
without an error."""

import pytest
from azure.monitor.opentelemetry.exporter._quickpulse import enable_live_metrics
from azure.monitor.opentelemetry.exporter._quickpulse._state import get_quickpulse_manager

import monitorutil as mu

pytestmark = pytest.mark.emulator_only


def test_the_exporter_pings_and_is_answered(emu, conn_str):
    log = mu.log_path(emu)
    if log is None:
        pytest.skip("needs the emulator's own log")
    start = log.stat().st_size
    enable_live_metrics(connection_string=conn_str)
    try:
        pings = mu.wait_for(
            lambda: [r for r in mu.requests_since(log, start) if "QuickPulseService.svc/ping" in r[1]], "a Live Metrics ping", timeout=30
        )
    finally:
        get_quickpulse_manager().shutdown()
    assert pings[0][0] == "POST"
    assert all(status == 200 for _, _, status in pings)
