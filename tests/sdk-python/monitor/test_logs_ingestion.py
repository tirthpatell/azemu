"""The Logs Ingestion client (`azure-monitor-ingestion`) uploading to the emulator.

The client sends a bearer token and so insists on https: the emulator's certificate authority is given to it with
`connection_verify`. The credential is a fake one: the emulator takes any token. Rules and streams are not declared anywhere;
the names are kept as they came.
"""

import gzip
import io
import json
import time

import pytest
from azure.core.credentials import AccessToken
from azure.monitor.ingestion import LogsIngestionClient
from azure.monitor.ingestion.aio import LogsIngestionClient as AsyncLogsIngestionClient

import monitorutil as mu

pytestmark = pytest.mark.emulator_only

DCR = "dcr-0123456789abcdef0123456789abcdef"
STREAM = "Custom-AppLogs_CL"


class FakeCredential:
    def get_token(self, *scopes, **kwargs):
        FakeCredential.scopes = scopes
        return AccessToken("azemu-test-token", int(time.time()) + 3600)


class FakeAsyncCredential:
    async def get_token(self, *scopes, **kwargs):
        return AccessToken("azemu-test-token", int(time.time()) + 3600)

    async def close(self):
        pass


@pytest.fixture
def endpoint(emu):
    """The gateway host form: `https://{dce}.ingest.localhost:{port}`."""
    return emu.https_url("ingest", "dce1")


@pytest.fixture
def client(endpoint, emu):
    with LogsIngestionClient(endpoint, FakeCredential(), connection_verify=emu.ca_file) as c:
        yield c


def rows(n, prefix="r"):
    return [{"TimeGenerated": "2026-11-20T10:00:00Z", "Computer": f"{prefix}{i}", "Level": i} for i in range(n)]


def test_upload_is_kept_with_its_rule_and_stream(emu, client):
    client.upload(rule_id=DCR, stream_name=STREAM, logs=rows(3))
    got = mu.logs(emu)
    assert [g["row"]["Computer"] for g in got] == ["r0", "r1", "r2"]
    assert {(g["dcr"], g["stream"]) for g in got} == {(DCR, STREAM)}
    assert FakeCredential.scopes == ("https://monitor.azure.com/.default",)


def test_filters_by_rule_and_stream(emu, client):
    client.upload(rule_id=DCR, stream_name=STREAM, logs=rows(2))
    client.upload(rule_id="dcr-other", stream_name="Custom-Other_CL", logs=rows(1, "o"))
    assert len(mu.logs(emu, dcr="dcr-other")) == 1
    assert len(mu.logs(emu, stream=STREAM)) == 2
    assert len(mu.logs(emu, dcr=DCR, stream="Custom-Other_CL")) == 0


def test_a_big_upload_is_split_into_chunks_and_all_of_it_arrives(emu, client):
    # about 3 MB of JSON: the client cuts it into calls of at most 1 MB
    big = [{"TimeGenerated": "2026-11-20T10:00:00Z", "Message": "x" * 1000, "Index": i} for i in range(3000)]
    log = mu.log_path(emu)
    start = log.stat().st_size if log else 0
    client.upload(rule_id=DCR, stream_name=STREAM, logs=big)
    got = mu.logs(emu)
    assert [g["row"]["Index"] for g in got] == list(range(3000))
    if log:
        calls = [r for r in mu.requests_since(log, start) if "/dataCollectionRules/" in r[1]]
        assert len(calls) >= 2 and all(s == 204 for _, _, s in calls), calls


def test_a_body_that_is_already_gzip_is_taken_as_it_is(emu, client):
    payload = gzip.compress(json.dumps(rows(2, "z")).encode())
    client.upload(rule_id=DCR, stream_name=STREAM, logs=io.BytesIO(payload))
    assert [g["row"]["Computer"] for g in mu.logs(emu)] == ["z0", "z1"]


def test_the_endpoint_without_a_host_that_names_the_service(emu):
    # `https://127.0.0.1:{port}`: the path alone tells the emulator what the call is
    with LogsIngestionClient(f"https://127.0.0.1:{emu.port}", FakeCredential(), connection_verify=emu.ca_file) as c:
        c.upload(rule_id=DCR, stream_name=STREAM, logs=rows(1))
    assert len(mu.logs(emu)) == 1


@pytest.mark.asyncio
async def test_async_client(emu, endpoint):
    async with AsyncLogsIngestionClient(endpoint, FakeAsyncCredential(), connection_verify=emu.ca_file) as c:
        await c.upload(rule_id=DCR, stream_name=STREAM, logs=rows(2, "a"))
    assert [g["row"]["Computer"] for g in mu.logs(emu)] == ["a0", "a1"]


def test_on_error_is_not_called_for_a_good_upload(emu, client):
    errors = []
    client.upload(rule_id=DCR, stream_name=STREAM, logs=rows(1), on_error=errors.append)
    assert errors == []
