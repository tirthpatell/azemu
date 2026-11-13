"""Key Vault and App Configuration through the management SDKs, then the data plane SDKs."""

import time

import pytest
from azure.appconfiguration import AzureAppConfigurationClient
from azure.core.exceptions import ResourceNotFoundError
from azure.keyvault.secrets import SecretClient
from azure.mgmt.appconfiguration import AppConfigurationManagementClient
from azure.mgmt.keyvault import KeyVaultManagementClient

from conftest import SUB, FakeCredential


@pytest.fixture
def kvm(cred, mgmt):
    with KeyVaultManagementClient(cred, SUB, **mgmt) as c:
        yield c


@pytest.fixture
def acm(cred, mgmt):
    with AppConfigurationManagementClient(cred, SUB, **mgmt) as c:
        yield c


def make_vault(kvm, rg, name):
    return kvm.vaults.begin_create_or_update(
        rg, name,
        {"location": "eastus", "properties": {"tenantId": "00000000-0000-0000-0000-000000000000", "sku": {"family": "A", "name": "standard"}, "accessPolicies": []}},
    ).result()


def test_vault_and_secret(kvm, rg, uniq, emu):
    name = uniq("vault-")
    v = make_vault(kvm, rg, name)
    assert v.name == name
    assert v.properties.vault_uri == f"https://{name}.vault.localhost:{emu.port}/"
    assert v.properties.sku.name == "standard"
    assert v.properties.enable_soft_delete is True
    assert name in [x.name for x in kvm.vaults.list_by_resource_group(rg)]
    client = SecretClient(v.properties.vault_uri, FakeCredential(), connection_verify=emu.ca_file)
    client.set_secret("db-password", "s3cret")
    assert client.get_secret("db-password").value == "s3cret"
    kvm.vaults.delete(rg, name)
    with pytest.raises(ResourceNotFoundError):
        kvm.vaults.get(rg, name)
    with pytest.raises(ResourceNotFoundError):
        client.get_secret("db-password")


def test_vault_name_check_and_access_policy(kvm, rg, uniq):
    name = uniq("vault-")
    assert kvm.vaults.check_name_availability({"name": name, "type": "Microsoft.KeyVault/vaults"}).name_available is True
    make_vault(kvm, rg, name)
    r = kvm.vaults.check_name_availability({"name": name, "type": "Microsoft.KeyVault/vaults"})
    assert r.name_available is False
    kvm.vaults.update_access_policy(
        rg, name, "add",
        {"properties": {"accessPolicies": [{"tenantId": "00000000-0000-0000-0000-000000000000", "objectId": "obj-1", "permissions": {"secrets": ["get", "list"]}}]}},
    )
    got = kvm.vaults.get(rg, name)
    assert [p.object_id for p in got.properties.access_policies] == ["obj-1"]


def test_app_configuration_store_and_data(acm, rg, uniq, emu):
    name = uniq("appcfg-")
    store = acm.configuration_stores.begin_create(rg, name, {"location": "eastus", "sku": {"name": "standard"}}).result()
    assert store.name == name
    assert store.endpoint == f"https://{name}.azconfig.localhost:{emu.port}"
    assert store.provisioning_state == "Succeeded"
    keys = list(acm.configuration_stores.list_keys(rg, name))
    assert len(keys) == 4
    primary = next(k for k in keys if k.name == "Primary" and not k.read_only)
    assert primary.connection_string.startswith(f"Endpoint=https://{name}.azconfig.localhost:{emu.port}")
    client = AzureAppConfigurationClient.from_connection_string(primary.connection_string, connection_verify=emu.ca_file)
    from azure.appconfiguration import ConfigurationSetting

    client.set_configuration_setting(ConfigurationSetting(key="color", value="red"))
    assert client.get_configuration_setting("color").value == "red"
    assert name in [s.name for s in acm.configuration_stores.list_by_resource_group(rg)]
    regen = acm.configuration_stores.regenerate_key(rg, name, {"id": primary.id})
    assert regen.value != primary.value
    acm.configuration_stores.begin_delete(rg, name).result()
    with pytest.raises(ResourceNotFoundError):
        acm.configuration_stores.get(rg, name)
    with pytest.raises(ResourceNotFoundError):
        client.get_configuration_setting("color")
