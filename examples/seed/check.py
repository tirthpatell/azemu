"""A client for the seed example: it finds a blob container, a queue, a Cosmos container and a secret that it never creates.

    AZEMU_HOST   the name of the emulator as this program sees it (default: azemu, the Compose service name)

It prints one line per check and exits 1 when any failed.
"""

import os
import sys
import time
import urllib.request

HOST = os.environ.get("AZEMU_HOST", "azemu")
ACCOUNT = "devstoreaccount1"  # the account in seed.json under blob and queue
KEY = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="
COSMOS_KEY = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw=="
CA = "/tmp/azemu-ca.pem"


def wait_and_fetch_ca():
    """Wait until the emulator answers, then save the CA that signs its certificates (any port answers /_azemu/)."""
    deadline = time.time() + 60
    while True:
        try:
            with urllib.request.urlopen(f"http://{HOST}:10000/_azemu/ca.pem", timeout=3) as r:
                open(CA, "wb").write(r.read())
                return
        except Exception as e:  # noqa: BLE001
            if time.time() > deadline:
                raise SystemExit(f"the emulator did not answer on {HOST}:10000: {e}")
            time.sleep(0.5)


def blob():
    from azure.storage.blob import BlobServiceClient

    conn = f"DefaultEndpointsProtocol=http;AccountName={ACCOUNT};AccountKey={KEY};BlobEndpoint=http://{HOST}:10000/{ACCOUNT};"
    container = BlobServiceClient.from_connection_string(conn).get_container_client("uploads")
    container.upload_blob("hello.txt", b"hello")  # the container was not created here
    assert container.download_blob("hello.txt").readall() == b"hello"
    return "the container `uploads` is there; a blob written and read"


def queue():
    from azure.storage.queue import QueueServiceClient

    conn = f"DefaultEndpointsProtocol=http;AccountName={ACCOUNT};AccountKey={KEY};QueueEndpoint=http://{HOST}:10001/{ACCOUNT};"
    q = QueueServiceClient.from_connection_string(conn).get_queue_client("jobs")
    q.send_message("hello")
    assert [m.content for m in q.receive_messages()] == ["hello"]
    return "the queue `jobs` is there; a message sent and received"


def cosmos():
    from azure.cosmos import CosmosClient

    container = CosmosClient(f"http://{HOST}:8081", credential=COSMOS_KEY).get_database_client("app").get_container_client("items")
    container.create_item({"id": "1", "tenantId": "t1"})
    assert container.read_item("1", partition_key="t1")["tenantId"] == "t1"
    paths = container.read()["partitionKey"]["paths"]
    return f"the container `app/items` is there (partition key {paths[0]}); a document written and read"


def keyvault():
    from azure.core.credentials import AccessToken, AccessTokenInfo
    from azure.keyvault.secrets import SecretClient

    class AnyToken:  # the emulator accepts every token
        def get_token(self, *scopes, **kw):
            return AccessToken("token", int(time.time()) + 3600)

        def get_token_info(self, *scopes, options=None):
            return AccessTokenInfo("token", int(time.time()) + 3600)

    client = SecretClient(f"https://{HOST}:10010", AnyToken(), verify_challenge_resource=False, connection_verify=CA)
    assert client.get_secret("db-password").value == "local-only-password"
    return "the secret `db-password` is there with the value from the file"


if __name__ == "__main__":
    wait_and_fetch_ca()
    failed = 0
    for check in (blob, queue, cosmos, keyvault):
        try:
            print(f"ok   {check.__name__}: {check()}", flush=True)
        except BaseException as e:  # noqa: BLE001
            failed += 1
            print(f"FAIL {check.__name__}: {type(e).__name__}: {str(e)[:300]}", flush=True)
    sys.exit(1 if failed else 0)
