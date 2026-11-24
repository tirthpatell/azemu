"""The Azure Monitor OpenTelemetry exporters (`azure-monitor-opentelemetry-exporter`) sending traces, metrics and logs to the
emulator, and the test reading them back through the admin API.

The exporters post to `{IngestionEndpoint}/v2.1/track`. The connection string names the emulator with plain http (the exporter
takes `http` as it is), and the emulator keeps what arrives, so no test here waits for Azure or needs a key.
"""

import logging

import pytest
from azure.monitor.opentelemetry.exporter import AzureMonitorLogExporter, AzureMonitorMetricExporter, AzureMonitorTraceExporter
from opentelemetry import trace
from opentelemetry.sdk._logs import LoggerProvider, LoggingHandler
from opentelemetry.sdk._logs.export import SimpleLogRecordProcessor
from opentelemetry.sdk.metrics import MeterProvider
from opentelemetry.sdk.metrics.export import PeriodicExportingMetricReader
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor

import monitorutil as mu

pytestmark = pytest.mark.emulator_only

ROLE = "azemu-test-service"


@pytest.fixture
def tracer(conn_str, monkeypatch):
    provider = TracerProvider(resource=Resource.create({"service.name": ROLE}))
    provider.add_span_processor(SimpleSpanProcessor(AzureMonitorTraceExporter(connection_string=conn_str, disable_offline_storage=True)))
    # the exporter asks the global provider for the resource; give it this test's provider
    monkeypatch.setattr(trace, "get_tracer_provider", lambda: provider)
    yield provider.get_tracer("azemu-test")
    provider.shutdown()


def test_server_and_client_spans_share_one_operation(emu, tracer):
    with tracer.start_as_current_span("GET /orders", kind=trace.SpanKind.SERVER) as server:
        server.set_attribute("http.request.method", "GET")
        server.set_attribute("url.full", "http://shop.example/orders")
        server.set_attribute("http.response.status_code", 200)
        with tracer.start_as_current_span("SELECT orders", kind=trace.SpanKind.CLIENT) as client:
            client.set_attribute("db.system", "postgresql")
            client.set_attribute("db.statement", "select 1")
        trace_id = format(server.get_span_context().trace_id, "032x")

    items = mu.wait_for(lambda: len(mu.telemetry(emu, operationId=trace_id)) >= 2 and mu.telemetry(emu, operationId=trace_id), "two items")
    by_type = {i["type"]: i for i in items}
    assert set(by_type) == {"RequestData", "RemoteDependencyData"}
    request, dependency = by_type["RequestData"], by_type["RemoteDependencyData"]
    assert request["name"] == "GET /orders"
    assert request["envelope"]["data"]["baseData"]["responseCode"] == "200"
    assert request["envelope"]["data"]["baseData"]["success"] is True
    assert dependency["name"] == "SELECT orders"
    assert dependency["envelope"]["data"]["baseData"]["type"] == "postgresql"
    # the dependency belongs to the request
    assert dependency["envelope"]["tags"]["ai.operation.parentId"].endswith(request["envelope"]["data"]["baseData"]["id"])
    for i in items:
        assert i["iKey"] == mu.IKEY
        assert i["envelope"]["tags"]["ai.operation.id"] == trace_id
        assert i["envelope"]["tags"]["ai.cloud.role"] == ROLE
        assert i["operationId"] == trace_id


def test_a_recorded_exception_is_an_exception_item(emu, tracer):
    with tracer.start_as_current_span("work", kind=trace.SpanKind.SERVER) as span:
        trace_id = format(span.get_span_context().trace_id, "032x")
        try:
            raise ValueError("it broke")
        except ValueError as e:
            span.record_exception(e)
    items = mu.wait_for(lambda: mu.telemetry(emu, operationId=trace_id, type="ExceptionData"), "the exception")
    exc = items[0]["envelope"]["data"]["baseData"]["exceptions"][0]
    assert (exc["typeName"], exc["message"]) == ("ValueError", "it broke")


def test_a_failed_request_is_not_a_success(emu, tracer):
    with tracer.start_as_current_span("POST /pay", kind=trace.SpanKind.SERVER) as span:
        span.set_attribute("http.request.method", "POST")
        span.set_attribute("url.full", "http://shop.example/pay")
        span.set_attribute("http.response.status_code", 500)
        span.set_status(trace.Status(trace.StatusCode.ERROR))
        trace_id = format(span.get_span_context().trace_id, "032x")
    items = mu.wait_for(lambda: mu.telemetry(emu, operationId=trace_id, type="RequestData"), "the request")
    base = items[0]["envelope"]["data"]["baseData"]
    assert (base["responseCode"], base["success"]) == ("500", False)


def test_metrics(emu, conn_str):
    reader = PeriodicExportingMetricReader(
        AzureMonitorMetricExporter(connection_string=conn_str, disable_offline_storage=True), export_interval_millis=3_600_000
    )
    provider = MeterProvider(metric_readers=[reader])
    meter = provider.get_meter("azemu-test")
    meter.create_counter("orders.placed").add(3)
    hist = meter.create_histogram("order.size")
    hist.record(5)
    hist.record(7)
    provider.force_flush()
    provider.shutdown()

    counter = mu.wait_for(lambda: mu.telemetry(emu, type="MetricData", name="orders.placed"), "the counter")
    assert counter[0]["envelope"]["data"]["baseData"]["metrics"][0]["value"] == 3
    size = mu.telemetry(emu, type="MetricData", name="order.size")[0]["envelope"]["data"]["baseData"]["metrics"][0]
    assert (size["value"], size["count"], size["min"], size["max"]) == (12, 2, 5, 7)


def test_logs_carry_the_operation_of_the_span_they_were_written_in(emu, conn_str, tracer):
    provider = LoggerProvider()
    provider.add_log_record_processor(SimpleLogRecordProcessor(AzureMonitorLogExporter(connection_string=conn_str, disable_offline_storage=True)))
    logger = logging.getLogger("azemu.monitor.test")
    handler = LoggingHandler(logger_provider=provider)
    logger.addHandler(handler)
    logger.setLevel(logging.INFO)
    try:
        with tracer.start_as_current_span("job") as span:
            trace_id = format(span.get_span_context().trace_id, "032x")
            logger.warning("disk is %s percent full", 91)
            try:
                raise KeyError("missing")
            except KeyError:
                logger.exception("lookup failed")
    finally:
        logger.removeHandler(handler)
        provider.shutdown()

    messages = mu.wait_for(lambda: mu.telemetry(emu, operationId=trace_id, type="MessageData"), "the log message")
    base = messages[0]["envelope"]["data"]["baseData"]
    assert base["message"] == "disk is 91 percent full"
    assert base["severityLevel"] == 2  # warning
    errors = mu.wait_for(lambda: mu.telemetry(emu, operationId=trace_id, type="ExceptionData"), "the logged exception")
    assert errors[0]["envelope"]["data"]["baseData"]["exceptions"][0]["typeName"] == "KeyError"


def test_since_reads_what_came_after_a_point(emu, tracer):
    with tracer.start_as_current_span("first"):
        pass
    mark = mu.wait_for(lambda: mu.admin(emu, "/telemetry")["lastSequence"], "the first item")
    with tracer.start_as_current_span("second"):
        pass
    after = mu.wait_for(lambda: mu.telemetry(emu, since=mark), "the second item")
    assert [i["name"] for i in after] == ["second"]
