"""Checkpointing and load balancing with a Blob-backed checkpoint store (needs Event Hubs AND Blob Storage in one emulator).

The store is `blobcheckpoint.BlobCheckpointStore` (azure-eventhub-checkpointstoreblob is not installed): blobs named
`<fqn>/<hub>/<group>/ownership/<pid>` (metadata `ownerid`, etag = ownership etag) and `.../checkpoint/<pid>` (metadata `offset`,
`sequencenumber`), claims made with `If-None-Match: *` / `If-Match: <etag>`.

This module starts the FULL emulator itself (`target/debug/azemu --port <free> --cert-dir <tmp>`), waits for `/_azemu/health` and
needs both `eventhubs` and `blob` in its `services` list; otherwise the whole module is skipped. Event Hubs and Blob share the one
port: `Endpoint=sb://myns.eventhub.localhost:{port}` and `BlobEndpoint=http://devstoreaccount1.blob.core.localhost:{port}`.

For checking the tests without the full emulator, `AZEMU_EH_STACK=<eventhubs port>,<blob port>` points the module at two
separately running processes instead (e.g. the double in fake_eventhubs.py and target/debug/azemu-blob).
"""

import json
import os
import pathlib
import socket
import subprocess
import tempfile
import time
import urllib.request
import uuid

import pytest

import emubin
from azure.core.exceptions import ResourceExistsError
from azure.storage.blob import ContainerClient

import ehutil
from blobcheckpoint import BlobCheckpointStore
from ehutil import Collector, bodies, host, make_consumer, make_producer, send_n, texts, wait_until

ROOT = pathlib.Path(__file__).resolve().parents[3]
DEV_ACCOUNT = "devstoreaccount1"
DEV_KEY = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="  # top-level conftest

pytestmark = pytest.mark.needs_blob


class Stack:
    """The few things ehutil needs from the `emu` fixture (`port`, `gateway`, `url`) plus the storage connection string."""

    def __init__(self, eh_port: int, blob_port: int):
        self.port = eh_port
        self.blob_port = blob_port
        self.ca_file = None

    @property
    def gateway(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def url(self, label: str, account: str, scheme: str = "http") -> str:
        suffix = "core.localhost" if label in ("blob", "queue", "table", "dfs") else "localhost"
        return f"{scheme}://{account}.{label}.{suffix}:{self.port}"

    @property
    def storage_conn_str(self) -> str:
        return (
            f"DefaultEndpointsProtocol=http;AccountName={DEV_ACCOUNT};AccountKey={DEV_KEY};"
            f"BlobEndpoint=http://{DEV_ACCOUNT}.blob.core.localhost:{self.blob_port};"
        )


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _health(port: int, timeout: float = 20.0, proc=None):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        if proc is not None and proc.poll() is not None:
            return None
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/_azemu/health", timeout=1) as r:
                return json.loads(r.read())
        except Exception as e:  # noqa: BLE001 - not up yet
            last = e
            time.sleep(0.1)
    return None


def _services(health) -> set:
    names = set()
    for s in (health or {}).get("services", []):
        names.add(s if isinstance(s, str) else str(s.get("name", "")))
    return names


@pytest.fixture(scope="module")
def stack():
    override = os.environ.get("AZEMU_EH_STACK")
    if override:
        eh, blob = (int(x) for x in override.split(","))
        yield Stack(eh, blob)
        return
    binary = emubin.unified()
    if not binary or not pathlib.Path(binary).exists():
        pytest.fail(emubin.missing_message())
    port = _free_port()
    with tempfile.TemporaryDirectory(prefix="azemu-eh-blob-") as cert_dir:
        log = open(os.path.join(cert_dir, "azemu.log"), "wb")
        proc = subprocess.Popen([binary, "--port", str(port), "--cert-dir", cert_dir], stdout=log, stderr=subprocess.STDOUT)
        try:
            health = _health(port, proc=proc)
            names = _services(health)
            if not {"eventhubs", "blob"} <= names:
                pytest.skip(f"the full emulator does not serve both eventhubs and blob (services: {sorted(names)})")
            yield Stack(port, port)
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
            log.close()


@pytest.fixture
def store(stack):
    container = ContainerClient.from_connection_string(stack.storage_conn_str, f"cp-{uuid.uuid4().hex[:10]}")
    container.create_container()
    yield BlobCheckpointStore(container)
    try:
        container.delete_container()
    except Exception:  # noqa: BLE001
        pass


@pytest.fixture
def eh_hub(stack):
    def make(partitions=4, groups=()):
        name = ehutil.uniq()
        ehutil.admin_create_hub(stack, name, partitions, groups)
        return name

    return make


FAST = dict(load_balancing_interval=0.5, partition_ownership_expiration_interval=20)


def consumer(stack, hub, store, group="$Default", **kw):
    return make_consumer(stack, hub, group, checkpoint_store=store, **{**FAST, **kw})


def blobs(store):
    return {b.name: b.metadata or {} for b in store._container.list_blobs(include=["metadata"])}


# -- the store on its own (Blob only) ----------------------------------------------------------------------------------------------
def test_claim_list_and_conflict(store):
    base = dict(fully_qualified_namespace="ns:1", eventhub_name="hub", consumer_group="$Default")
    first = list(store.claim_ownership([dict(base, partition_id="0", owner_id="A"), dict(base, partition_id="1", owner_id="A")]))
    assert len(first) == 2 and all(o["etag"] for o in first)
    assert list(store.claim_ownership([dict(base, partition_id="0", owner_id="B")])) == []  # no etag: only creation is allowed
    listed = {o["partition_id"]: o for o in store.list_ownership("ns:1", "hub", "$Default")}
    assert {p: o["owner_id"] for p, o in listed.items()} == {"0": "A", "1": "A"}
    stolen = list(store.claim_ownership([dict(listed["0"], owner_id="B")]))
    assert len(stolen) == 1 and stolen[0]["owner_id"] == "B"
    assert list(store.claim_ownership([dict(listed["0"], owner_id="C")])) == []  # stale etag loses


def test_checkpoint_is_stored_as_blob_metadata(store):
    store.update_checkpoint(dict(fully_qualified_namespace="ns:1", eventhub_name="hub", consumer_group="$Default",
                                 partition_id="3", offset="123", sequence_number=7))
    assert blobs(store)["ns:1/hub/$default/checkpoint/3"] == {"offset": "123", "sequencenumber": "7"}
    (cp,) = store.list_checkpoints("ns:1", "hub", "$Default")
    assert (cp["partition_id"], cp["offset"], cp["sequence_number"]) == ("3", "123", 7)


# -- with Event Hubs ---------------------------------------------------------------------------------------------------------------
def test_resume_after_a_checkpoint_stored_in_blobs(stack, eh_hub, store, track):
    hub = eh_hub(1)
    p = track(make_producer(stack, hub))
    send_n(p, 6, partition_id="0")

    def checkpoint_third(ctx, event):
        if event.body_as_str() == "m-2":
            ctx.update_checkpoint(event)

    with Collector(consumer(stack, hub, store), starting_position="-1", max_wait_time=1, on_event_hook=checkpoint_third) as col:
        col.wait_events(6)
    third = next(e for e in col.events if e.body_as_str() == "m-2")
    meta = blobs(store)[f"{host(stack)}/{hub}/$default/checkpoint/0".lower()]
    assert meta == {"offset": third.offset, "sequencenumber": str(third.sequence_number)}

    with Collector(consumer(stack, hub, store), starting_position="-1", max_wait_time=1) as col2:
        assert bodies(col2.wait_events(3)) == ["m-3", "m-4", "m-5"]
        col2.settle()
        assert col2.count() == 3


def test_ownership_blob_names_the_owner_and_is_released_on_close(stack, eh_hub, store):
    hub = eh_hub(2)
    col = Collector(consumer(stack, hub, store), starting_position="-1", max_wait_time=1).start()
    col.wait_initialized(2)
    name0 = f"{host(stack)}/{hub}/$default/ownership/0".lower()
    meta = wait_until(lambda: blobs(store).get(name0), 8, what="the ownership blob of partition 0")
    assert meta["ownerid"]
    col.stop()
    after = blobs(store)
    assert all(not m.get("ownerid") for n, m in after.items() if "/ownership/" in n)


@pytest.mark.slow
def test_two_consumers_split_the_partitions_through_blob_ownership(stack, eh_hub, store, track):
    hub = eh_hub(2)
    p = track(make_producer(stack, hub))
    a = Collector(consumer(stack, hub, store), starting_position="-1", max_wait_time=1).start()
    b = None
    try:
        a.wait_initialized(2)
        b = Collector(consumer(stack, hub, store), starting_position="-1", max_wait_time=1).start()

        def split():
            owners = [m.get("ownerid") for n, m in blobs(store).items() if "/ownership/" in n]
            return len(owners) == 2 and len(set(owners)) == 2 and all(owners)

        wait_until(split, 14, 0.3, "two ownership blobs with two different owners")
        time.sleep(1.5)
        sent = {pid: send_n(p, 2, prefix=f"e{pid}", partition_id=pid) for pid in ("0", "1")}
        wait_until(lambda: a.count() + b.count() >= 4, 8, 0.1, "4 events")
        time.sleep(0.5)
        assert sorted(bodies(a.events) + bodies(b.events)) == sorted(sent["0"] + sent["1"])
        assert not set(a.by_partition()) & set(b.by_partition())
    finally:
        a.stop()
        if b:
            b.stop()


def test_a_blob_store_shared_by_two_groups_keeps_their_checkpoints_apart(stack, eh_hub, store, track):
    hub = eh_hub(1, groups=["g2"])
    p = track(make_producer(stack, hub))
    send_n(p, 3, partition_id="0")
    with Collector(consumer(stack, hub, store), starting_position="-1", max_wait_time=1,
                   on_event_hook=lambda ctx, e: ctx.update_checkpoint(e)) as col:
        col.wait_events(3)
    with Collector(consumer(stack, hub, store, "g2"), starting_position="-1", max_wait_time=1) as col2:
        assert bodies(col2.wait_events(3)) == texts(3)
    names = [n for n in blobs(store) if "/checkpoint/" in n]
    assert names == [f"{host(stack)}/{hub}/$default/checkpoint/0".lower()]
