"""A small client for the examples: Blob, Queue and Key Vault, each called once through its Python SDK.

    AZEMU_HOST   the name of the emulator as this program sees it (default: azemu, the Compose service name)

It fetches the emulator's CA from the emulator itself, so nothing has to be copied between containers. It prints one
line per service and exits 1 when any call failed.
"""

import os
import sys
import time
import urllib.request
import uuid

HOST = os.environ.get("AZEMU_HOST", "azemu")
ACCOUNT = "devstoreaccount1"  # the well known development account every Azurite user has
KEY = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="
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

    # Azurite style: the port is the Blob service, the account is the first part of the path.
    conn = f"DefaultEndpointsProtocol=http;AccountName={ACCOUNT};AccountKey={KEY};BlobEndpoint=http://{HOST}:10000/{ACCOUNT};"
    container = BlobServiceClient.from_connection_string(conn).create_container("c" + uuid.uuid4().hex[:8])
    container.upload_blob("hello.txt", b"hello")
    assert container.download_blob("hello.txt").readall() == b"hello"
    return f"a blob written and read on {HOST}:10000"


def queue():
    from azure.storage.queue import QueueServiceClient

    conn = f"DefaultEndpointsProtocol=http;AccountName={ACCOUNT};AccountKey={KEY};QueueEndpoint=http://{HOST}:10001/{ACCOUNT};"
    q = QueueServiceClient.from_connection_string(conn).create_queue("q" + uuid.uuid4().hex[:8])
    q.send_message("hello")
    assert [m.content for m in q.receive_messages()] == ["hello"]
    return f"a message sent and received on {HOST}:10001"


def keyvault():
    from azure.core.credentials import AccessToken, AccessTokenInfo
    from azure.keyvault.secrets import SecretClient

    class AnyToken:  # the emulator accepts every token
        def get_token(self, *scopes, **kw):
            return AccessToken("token", int(time.time()) + 3600)

        def get_token_info(self, *scopes, options=None):
            return AccessTokenInfo("token", int(time.time()) + 3600)

    # https needs a certificate for the name the client uses: the emulator is started with AZEMU_TLS_NAME=<that name>
    # (not needed for localhost). Which vault this is comes from that name: https://azemu:10010 is the vault `azemu`.
    client = SecretClient(f"https://{HOST}:10010", AnyToken(), verify_challenge_resource=False, connection_verify=CA)
    client.set_secret("greeting", "hello")
    assert client.get_secret("greeting").value == "hello"
    return f"a secret written and read on https://{HOST}:10010"


if __name__ == "__main__":
    wait_and_fetch_ca()
    failed = 0
    for check in (blob, queue, keyvault):
        try:
            print(f"ok   {check.__name__}: {check()}", flush=True)
        except BaseException as e:  # noqa: BLE001
            failed += 1
            print(f"FAIL {check.__name__}: {type(e).__name__}: {str(e)[:300]}", flush=True)
    sys.exit(1 if failed else 0)
