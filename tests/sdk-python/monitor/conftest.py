"""Fixtures for the monitor (Application Insights and Azure Monitor) SDK tests.

The `emu` fixture (../conftest.py) starts the emulator. Each test points the OpenTelemetry exporters or the
`azure-monitor-ingestion` client at it and reads what arrived through the admin API (`/_azemu/monitor/...`).

Every test is also checked against the emulator's request log: a request the emulator answered with a 4xx or 5xx status
is a request an SDK made that nothing handles, so the test fails, unless it is marked `expects_refusals` (the tests that
send bad input on purpose).
"""

import os
import pathlib

import pytest

import monitorutil


def pytest_configure(config):
    config.addinivalue_line("markers", "expects_refusals: the test sends bad input on purpose; its refused requests are not an error")


@pytest.fixture(scope="session")
def azemu_service():
    return "monitor"


@pytest.fixture(autouse=True)
def no_refused_requests(request, emu):
    """Fail a test when the emulator refused a request of the monitor service while it ran."""
    log = monitorutil.log_path(emu)
    if log is None or request.node.get_closest_marker("expects_refusals"):
        yield
        return
    start = log.stat().st_size if log.exists() else 0
    yield
    refused = monitorutil.refused_since(log, start)
    assert not refused, "the emulator refused requests of the monitor service:\n" + "\n".join(refused)


@pytest.fixture(autouse=True)
def sdk_environment(monkeypatch):
    """What an SDK would otherwise send to Microsoft's own hosts (statistics, remote configuration) stays off, and
    nothing from the developer's shell chooses a connection string."""
    for name in ("APPLICATIONINSIGHTS_CONNECTION_STRING", "APPINSIGHTS_INSTRUMENTATIONKEY"):
        monkeypatch.delenv(name, raising=False)
    for name in (
        "APPLICATIONINSIGHTS_STATSBEAT_DISABLED_ALL",
        "APPLICATIONINSIGHTS_CONTROLPLANE_DISABLED",
        "APPLICATIONINSIGHTS_SDKSTATS_DISABLED",
    ):
        monkeypatch.setenv(name, "true")


@pytest.fixture
def conn_str(emu):
    """The connection string of the emulator, with the endpoints written the way the docs show: plain http."""
    return monitorutil.conn_string(emu)
