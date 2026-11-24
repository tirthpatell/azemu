import pytest
from azure.core.exceptions import ResourceNotFoundError
from azure.identity import ManagedIdentityCredential
from azure.keyvault.secrets import SecretClient

from conftest import GATEWAY, KEY_VAULT


# Key Vault needs https and a token. The credential is the one a service uses on Azure; two variables send it to the
# emulator's token endpoint. The client trusts this container's CA and no other.
def test_key_vault_over_https(emulator, tmp_path, monkeypatch):
    monkeypatch.setenv("IDENTITY_ENDPOINT", emulator.url("http", GATEWAY, "/_azemu/identity/msi/token"))
    monkeypatch.setenv("IDENTITY_HEADER", "azemu")
    secrets = SecretClient(
        emulator.url("https", KEY_VAULT),
        ManagedIdentityCredential(),
        connection_verify=emulator.ca_file(tmp_path),
        # A host with one label ("localhost") fails the client's check of the Key Vault challenge: switch it off.
        verify_challenge_resource=False,
    )

    # "db-password" comes from seed.json.
    assert secrets.get_secret("db-password").value == "local-only-password"
    secrets.set_secret("api-key", "hunter2")
    assert secrets.get_secret("api-key").value == "hunter2"

    # After a reset the secret that a test made is gone and the seeded one is back.
    emulator.reset()
    with pytest.raises(ResourceNotFoundError):
        secrets.get_secret("api-key")
    assert secrets.get_secret("db-password").value == "local-only-password"
