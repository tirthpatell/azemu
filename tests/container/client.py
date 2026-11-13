"""The client side of the container test: a small SDK call against each service, by service name, on the standard ports.

It runs in a container of its own, on the same Docker network as the emulator (which has the network aliases `azemu`
and `eventhub`). It prints one line per check, `ok <name> ...` or `FAIL <name> ...`, and exits 1 when anything failed.
Nothing here needs a name under .localhost to resolve.
"""

import json
import os
import sys
import time
import urllib.request
import uuid

HOST = os.environ.get("AZEMU_HOST", "azemu")
EVENTHUB_HOST = os.environ.get("AZEMU_EVENTHUB_HOST", "eventhub")
GATEWAY = f"http://{HOST}:4577"
DEV_ACCOUNT = "devstoreaccount1"
DEV_KEY = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="
COSMOS_KEY = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw=="
SB_KEY = "c2VjcmV0c2VjcmV0c2VjcmV0"
CA = "/tmp/azemu-ca.pem"


def uid(prefix=""):
    return prefix + uuid.uuid4().hex[:10]


def wait_for_emulator():
    deadline = time.time() + 60
    while True:
        try:
            with urllib.request.urlopen(f"{GATEWAY}/_azemu/health", timeout=3) as r:
                return json.load(r)
        except Exception as e:  # noqa: BLE001
            if time.time() > deadline:
                raise SystemExit(f"FAIL start: the emulator did not answer at {GATEWAY}: {e}")
            time.sleep(0.5)


def fetch_ca():
    # the CA that signed the emulator's certificates; any port answers /_azemu/ paths
    with urllib.request.urlopen(f"http://{HOST}:10000/_azemu/ca.pem", timeout=5) as r:
        open(CA, "wb").write(r.read())
    os.environ["REQUESTS_CA_BUNDLE"] = CA  # requests (the Cosmos SDK, the Service Bus admin client)
    os.environ["SSL_CERT_FILE"] = CA


# -- the checks ----------------------------------------------------------------------------------
def check_blob():
    from azure.storage.blob import BlobServiceClient

    def conn(account, key):
        return f"DefaultEndpointsProtocol=http;AccountName={account};AccountKey={key};BlobEndpoint=http://{HOST}:10000/{account};"

    svc = BlobServiceClient.from_connection_string(conn(DEV_ACCOUNT, DEV_KEY))
    cont = svc.create_container(uid("c"))
    cont.upload_blob("hello.txt", b"hello from a container")
    assert cont.download_blob("hello.txt").readall() == b"hello from a container"
    assert "hello.txt" in [b.name for b in cont.list_blobs()]
    # another account on the same port: the account is the first path segment
    other = BlobServiceClient.from_connection_string(conn("otheraccount", "c2VjcmV0"))
    oc = other.create_container(uid("c"))
    oc.upload_blob("x", b"1")
    assert cont.container_name not in [c.name for c in other.list_containers()]
    return f"container and blob on {HOST}:10000, two accounts kept apart"


def check_queue():
    from azure.storage.queue import QueueServiceClient

    cs = f"DefaultEndpointsProtocol=http;AccountName={DEV_ACCOUNT};AccountKey={DEV_KEY};QueueEndpoint=http://{HOST}:10001/{DEV_ACCOUNT};"
    q = QueueServiceClient.from_connection_string(cs).create_queue(uid("q"))
    q.send_message("hello queue")
    msgs = list(q.receive_messages())
    assert [m.content for m in msgs] == ["hello queue"], msgs
    return f"send and receive on {HOST}:10001"


def check_table():
    from azure.data.tables import TableServiceClient

    cs = f"DefaultEndpointsProtocol=http;AccountName={DEV_ACCOUNT};AccountKey={DEV_KEY};TableEndpoint=http://{HOST}:10002/{DEV_ACCOUNT};"
    t = TableServiceClient.from_connection_string(cs).create_table(uid("t"))
    t.create_entity({"PartitionKey": "p", "RowKey": "r", "n": 7})
    rows = list(t.query_entities("PartitionKey eq 'p'"))
    assert len(rows) == 1 and rows[0]["n"] == 7, rows
    return f"entity on {HOST}:10002"


def check_keyvault():
    import time as _t

    from azure.core.credentials import AccessToken, AccessTokenInfo
    from azure.keyvault.secrets import SecretClient

    class Cred:
        def get_token(self, *scopes, **kw):
            return AccessToken("token", int(_t.time()) + 3600)

        def get_token_info(self, *scopes, options=None):
            return AccessTokenInfo("token", int(_t.time()) + 3600)

    # the vault is named by the host (`azemu`), the way `{vault}.vault.localhost` names it; the SDK's check of the
    # challenge's resource stays off until the challenge names the host
    sc = SecretClient(f"https://{HOST}:10010", Cred(), verify_challenge_resource=False, connection_verify=CA)
    name = uid("s")
    sc.set_secret(name, "s3cret")
    assert sc.get_secret(name).value == "s3cret"
    return f"secret in vault '{HOST}' on https://{HOST}:10010"


def check_cosmos():
    from azure.cosmos import CosmosClient, PartitionKey

    client = CosmosClient(f"https://{HOST}:8081/", credential=COSMOS_KEY)
    db = client.create_database(uid("db"))
    cont = db.create_container(uid("c"), PartitionKey(path="/pk"))
    cont.upsert_item({"id": "1", "pk": "a", "v": 5})
    rows = list(cont.query_items("SELECT * FROM c WHERE c.v = 5", enable_cross_partition_query=True))
    assert len(rows) == 1 and rows[0]["id"] == "1", rows
    return f"item and query on https://{HOST}:8081"


def check_servicebus():
    from azure.servicebus import ServiceBusClient, ServiceBusMessage
    from azure.servicebus.management import ServiceBusAdministrationClient

    cs = f"Endpoint=sb://{HOST}:5672;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey={SB_KEY};UseDevelopmentEmulator=true"
    queue = uid("q")
    # nothing is created on first use: the queue is made through the administration client (HTTPS on the same port)
    admin = ServiceBusAdministrationClient.from_connection_string(cs, connection_verify=CA)
    admin.create_queue(queue)
    with ServiceBusClient.from_connection_string(cs) as client:
        with client.get_queue_sender(queue) as sender:
            sender.send_messages(ServiceBusMessage("hello bus"))
        with client.get_queue_receiver(queue, max_wait_time=5) as receiver:
            got = receiver.receive_messages(max_message_count=1, max_wait_time=5)
            assert [str(m) for m in got] == ["hello bus"], got
            receiver.complete_message(got[0])
    return f"queue created, message sent and received on {HOST}:5672"


def check_eventhubs():
    from azure.eventhub import EventData, EventHubConsumerClient, EventHubProducerClient

    hub = uid("h")
    # the hub is created through the emulator's admin API; `eventhub` is the host label that makes the AMQP port Event Hubs
    req = urllib.request.Request(f"{GATEWAY}/_azemu/eventhubs/default/hubs/{hub}", data=b"{}", method="PUT", headers={"Content-Type": "application/json"})
    urllib.request.urlopen(req, timeout=10).read()
    cs = f"Endpoint=sb://{EVENTHUB_HOST}:5672;SharedAccessKeyName=k;SharedAccessKey={SB_KEY};UseDevelopmentEmulator=true;EntityPath={hub}"
    producer = EventHubProducerClient.from_connection_string(cs)
    with producer:
        batch = producer.create_batch(partition_id="0")
        batch.add(EventData("hello hub"))
        producer.send_batch(batch)
    consumer = EventHubConsumerClient.from_connection_string(cs, consumer_group="$Default")
    got = []

    def on_event(ctx, event):
        if event is not None:
            got.append(event.body_as_str())
            consumer.close()

    with consumer:
        consumer.receive(on_event=on_event, partition_id="0", starting_position="-1", max_wait_time=5)
    assert got == ["hello hub"], got
    return f"event sent and read on {EVENTHUB_HOST}:5672"


def check_datalake():
    from azure.storage.filedatalake import DataLakeServiceClient

    svc = DataLakeServiceClient(f"http://{HOST}:10003/{DEV_ACCOUNT}", credential={"account_name": DEV_ACCOUNT, "account_key": DEV_KEY})
    fs = svc.create_file_system(uid("fs"))
    f = fs.create_file("dir/hello.txt")
    f.upload_data(b"hello lake", overwrite=True)
    assert f.download_file().readall() == b"hello lake"
    assert "dir/hello.txt" in [p.name for p in fs.get_paths(recursive=True)]
    return f"file system and file on {HOST}:10003"


def check_appconfig():
    from azure.appconfiguration import AzureAppConfigurationClient, ConfigurationSetting

    client = AzureAppConfigurationClient.from_connection_string(f"Endpoint=https://{HOST}:10011;Id=azemu;Secret={SB_KEY}")
    key = uid("k")
    client.set_configuration_setting(ConfigurationSetting(key=key, value="v1"))
    assert client.get_configuration_setting(key).value == "v1"
    return f"setting in store '{HOST}' on https://{HOST}:10011"


def check_eventgrid():
    from azure.core.credentials import AzureKeyCredential
    from azure.core.messaging import CloudEvent
    from azure.eventgrid import EventGridPublisherClient

    # nothing is created on first use: the topic named like the host is made through the admin API
    req = urllib.request.Request(f"{GATEWAY}/_azemu/eventgrid/topics/{HOST}", data=b"{}", method="PUT", headers={"Content-Type": "application/json"})
    urllib.request.urlopen(req, timeout=10).read()
    EventGridPublisherClient(f"https://{HOST}:10012/api/events", AzureKeyCredential("c2VjcmV0")).send(
        [CloudEvent(source="/ct", type="ct.test", data={"n": 1})]
    )
    with urllib.request.urlopen(f"{GATEWAY}/_azemu/eventgrid/topics", timeout=10) as r:
        topics = {t["name"]: t for t in json.load(r)["value"]}
    assert topics[HOST].get("published") == 1, topics.get(HOST)
    return f"event published to topic '{HOST}' on https://{HOST}:10012"


def check_arm_and_identity():
    import ssl

    ctx = ssl.create_default_context(cafile=CA)
    arm = urllib.request.Request(f"https://{HOST}:10013/subscriptions?api-version=2022-12-01", headers={"Authorization": "Bearer anything"})
    with urllib.request.urlopen(arm, timeout=10, context=ctx) as r:
        subs = json.load(r)
    assert subs.get("value") is not None, subs
    data = b"grant_type=client_credentials&client_id=x&client_secret=y&scope=https%3A%2F%2Fvault.azure.net%2F.default"
    with urllib.request.urlopen(urllib.request.Request(f"http://{HOST}:10014/tenant1/oauth2/v2.0/token", data=data, method="POST"), timeout=10) as r:
        assert json.load(r).get("access_token")
    return f"subscriptions list on https://{HOST}:10013, token on http://{HOST}:10014"


CHECKS = [check_blob, check_datalake, check_queue, check_table, check_keyvault, check_appconfig, check_cosmos, check_eventgrid, check_servicebus, check_eventhubs, check_arm_and_identity]


def main():
    health = wait_for_emulator()
    print(f"ok emulator: version {health.get('version')} commit {health.get('commit')} services {','.join(health['services'])}", flush=True)
    fetch_ca()
    failed = 0
    for check in CHECKS:
        name = check.__name__.removeprefix("check_")
        try:
            print(f"ok {name}: {check()}", flush=True)
        except BaseException as e:  # noqa: BLE001
            failed += 1
            print(f"FAIL {name}: {type(e).__name__}: {str(e)[:300]}", flush=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
