from azure.storage.blob import BlobServiceClient

from conftest import BLOB

# The Azurite connection string, with the published port. Plain http needs no certificate.
ACCOUNT_KEY = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="


def test_blob_over_http(emulator):
    connection_string = (
        f"DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;AccountKey={ACCOUNT_KEY};"
        f"BlobEndpoint={emulator.url('http', BLOB, '/devstoreaccount1')};"
    )
    service = BlobServiceClient.from_connection_string(connection_string)

    # "uploads" comes from seed.json; nothing here creates it.
    blob = service.get_blob_client("uploads", "hello.txt")
    blob.upload_blob(b"hello")
    assert blob.download_blob().readall() == b"hello"
