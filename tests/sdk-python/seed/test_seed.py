"""The seed file (`--seed`): what an application finds at start without creating it, through the official SDKs, what a reset
puts back, and what the program says about a file that is wrong."""

import pathlib
import sys

import pytest

for sibling in ("keyvault", "cosmos", "appconfig"):
    sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / sibling))
import kvutil  # noqa: E402
from appconfigutil import conn_string  # noqa: E402
from cosmosutil import new_client as new_cosmos_client  # noqa: E402

from azure.appconfiguration import AzureAppConfigurationClient  # noqa: E402
from azure.core.exceptions import ResourceNotFoundError  # noqa: E402
from azure.cosmos import PartitionKey  # noqa: E402
from azure.data.tables import TableServiceClient  # noqa: E402
from azure.storage.blob import BlobServiceClient  # noqa: E402
from azure.storage.queue import QueueServiceClient  # noqa: E402

ACCOUNT = "seedacct"
VAULT = "seedvault"
STORE = "seedstore"

SEED = {
    "blob": {ACCOUNT: {"containers": ["uploads", {"name": "public-files", "publicAccess": "blob", "metadata": {"team": "red"}}]}},
    "queue": {ACCOUNT: {"queues": ["jobs"]}},
    "table": {ACCOUNT: {"tables": ["Orders"]}},
    "cosmos": {
        ACCOUNT: {
            "databases": [
                {
                    "id": "app",
                    "containers": [
                        {"id": "items", "partitionKey": "/tenantId", "defaultTtl": -1, "uniqueKeyPolicy": {"uniqueKeys": [{"paths": ["/email"]}]}},
                        {"id": "events", "partitionKey": ["/tenantId", "/userId"]},
                    ],
                }
            ]
        }
    },
    "keyvault": {VAULT: {"secrets": {"db-password": "from-the-seed"}}},
    "appconfig": {
        STORE: {
            "keyValues": [
                {"key": "App:Title", "value": "Hello"},
                {"key": "App:Title", "label": "dev", "value": "Hello dev", "contentType": "text/plain"},
            ]
        }
    },
}


def blobs(emu):
    return BlobServiceClient.from_connection_string(emu.storage_conn_str(ACCOUNT))


def queues(emu):
    return QueueServiceClient.from_connection_string(emu.storage_conn_str(ACCOUNT))


def tables(emu):
    return TableServiceClient.from_connection_string(emu.storage_conn_str(ACCOUNT))


def cosmos(emu):
    return new_cosmos_client(emu.url("documents", ACCOUNT), emu.ca_file)


def appconfig(emu):
    return AzureAppConfigurationClient.from_connection_string(conn_string(emu.url("azconfig", STORE)))


def check_everything_is_there(emu):
    uploads = blobs(emu).get_container_client("uploads").get_container_properties()
    assert uploads.public_access is None
    public = blobs(emu).get_container_client("public-files").get_container_properties()
    assert public.public_access == "blob" and public.metadata == {"team": "red"}
    assert queues(emu).get_queue_client("jobs").get_queue_properties().name == "jobs"
    assert [t.name for t in tables(emu).list_tables()] == ["Orders"]
    tables(emu).get_table_client("Orders").create_entity({"PartitionKey": "p", "RowKey": "r", "v": 1})
    db = cosmos(emu).get_database_client("app")
    items = db.get_container_client("items").read()
    assert items["partitionKey"]["paths"] == ["/tenantId"] and items["defaultTtl"] == -1
    assert items["uniqueKeyPolicy"] == {"uniqueKeys": [{"paths": ["/email"]}]}
    events = db.get_container_client("events").read()
    assert events["partitionKey"]["paths"] == ["/tenantId", "/userId"] and events["partitionKey"]["kind"] == "MultiHash"
    assert kvutil.make_secret_client(emu, VAULT).get_secret("db-password").value == "from-the-seed"
    assert appconfig(emu).get_configuration_setting("App:Title").value == "Hello"
    dev = appconfig(emu).get_configuration_setting("App:Title", label="dev")
    assert (dev.value, dev.content_type) == ("Hello dev", "text/plain")


def test_the_seeded_things_exist_and_work_without_the_application_creating_them(seeded):
    s = seeded(SEED)
    check_everything_is_there(s.emu)
    # a seeded container is a real one: the application can write to it and read back
    c = blobs(s.emu).get_container_client("uploads")
    c.upload_blob("a.txt", b"hello")
    assert c.download_blob("a.txt").readall() == b"hello"
    queue = queues(s.emu).get_queue_client("jobs")
    queue.send_message("one")
    assert [m.content for m in queue.receive_messages()] == ["one"]
    items = cosmos(s.emu).get_database_client("app").get_container_client("items")
    items.create_item({"id": "1", "tenantId": "t1", "email": "a@x"})
    assert items.read_item("1", partition_key="t1")["email"] == "a@x"
    # `GET /_azemu/config` names the file in force, as an absolute path (it was given as a relative one)
    shown = s.config()["seedFile"]
    assert shown.startswith("/") and shown.endswith("seed.json")


def test_a_reset_puts_the_seed_back_and_removes_everything_else(seeded):
    s = seeded(SEED)
    emu = s.emu
    blobs(emu).create_container("extra")
    blobs(emu).delete_container("uploads")
    queues(emu).delete_queue("jobs")
    cosmos(emu).get_database_client("app").create_container("more", partition_key=PartitionKey(path="/x"))
    kvutil.make_secret_client(emu, VAULT).set_secret("db-password", "changed")
    s.reset()
    check_everything_is_there(emu)
    assert [c["name"] for c in blobs(emu).list_containers()] == ["public-files", "uploads"]
    with pytest.raises(Exception):
        cosmos(emu).get_database_client("app").get_container_client("more").read()
    assert len(list(kvutil.make_secret_client(emu, VAULT).list_properties_of_secret_versions("db-password"))) == 1


def test_a_partial_reset_puts_back_only_what_it_removed(seeded):
    s = seeded(SEED)
    emu = s.emu
    blobs(emu).delete_container("uploads")
    queues(emu).delete_queue("jobs")
    assert s.reset("?service=queue")["reset"] == ["queue"]
    assert queues(emu).get_queue_client("jobs").get_queue_properties().name == "jobs"
    with pytest.raises(ResourceNotFoundError):
        blobs(emu).get_container_client("uploads").get_container_properties()
    s.reset(f"?account={ACCOUNT}&service=blob")
    blobs(emu).get_container_client("uploads").get_container_properties()


def test_what_already_exists_is_left_as_it_is(seeded):
    # a name twice in the file is not an error and the first one stays
    two = dict(SEED, blob={ACCOUNT: {"containers": ["uploads", {"name": "uploads", "publicAccess": "container"}]}})
    s = seeded(two)
    assert blobs(s.emu).get_container_client("uploads").get_container_properties().public_access is None
    assert "ERROR" not in s.log_text()


@pytest.mark.parametrize(
    "content, wants",
    [
        ({"blob": {ACCOUNT: {"containers": ["Not_Valid"]}}}, ["blob.seedacct.containers[0]", "InvalidResourceName"]),
        ({"queue": {ACCOUNT: {"queues": ["jobs"], "tables": []}}}, ["queue.seedacct.tables", "unknown property"]),
        ({"blobs": {}}, ["blobs", "unknown top-level property"]),
        ('{"blob": ', ["not valid JSON"]),
        ({"servicebus": {}}, ["AZEMU_SERVICEBUS_CONFIG"]),
    ],
)
def test_a_file_that_is_wrong_stops_the_start_and_the_message_names_the_place(seeded, content, wants):
    code, log = seeded(content, expect_failure=True)
    assert code != 0
    assert "seed file " in log and "seed.json" in log, log
    for w in wants:
        assert w in log, log
