"""Resource groups, resources, tags, providers, subscriptions with azure-mgmt-resource."""

import pytest
from azure.core.exceptions import ResourceNotFoundError
from azure.mgmt.resource.subscriptions import SubscriptionClient

from conftest import SUB


def test_resource_group_lifecycle(resource_client, uniq):
    name = uniq("rg-")
    assert resource_client.resource_groups.check_existence(name) is False
    created = resource_client.resource_groups.create_or_update(name, {"location": "East US", "tags": {"a": "1"}})
    assert created.name == name
    assert created.location == "eastus"
    assert created.properties.provisioning_state == "Succeeded"
    assert created.id == f"/subscriptions/{SUB}/resourceGroups/{name}"
    assert created.type == "Microsoft.Resources/resourceGroups"
    assert resource_client.resource_groups.check_existence(name) is True
    got = resource_client.resource_groups.get(name)
    assert got.tags == {"a": "1"}
    updated = resource_client.resource_groups.update(name, {"tags": {"b": "2"}})
    assert updated.tags == {"b": "2"}
    assert name in [g.name for g in resource_client.resource_groups.list()]
    poller = resource_client.resource_groups.begin_delete(name)
    poller.result()
    assert poller.done()
    assert resource_client.resource_groups.check_existence(name) is False
    with pytest.raises(ResourceNotFoundError):
        resource_client.resource_groups.get(name)


def test_generic_resource_through_resources_client(resource_client, rg):
    poller = resource_client.resources.begin_create_or_update(
        rg, "Microsoft.Network", "", "virtualNetworks", "vnet1",
        {"location": "eastus", "tags": {"k": "v"}, "properties": {"addressSpace": {"addressPrefixes": ["10.0.0.0/16"]}}},
        api_version="2024-05-01",
    )
    res = poller.result()
    assert res.name == "vnet1"
    assert res.type == "Microsoft.Network/virtualNetworks"
    assert res.properties["provisioningState"] == "Succeeded"
    got = resource_client.resources.get(rg, "Microsoft.Network", "", "virtualNetworks", "vnet1", api_version="2024-05-01")
    assert got.tags == {"k": "v"}
    in_group = list(resource_client.resources.list_by_resource_group(rg))
    assert [r.name for r in in_group] == ["vnet1"]
    everywhere = list(resource_client.resources.list(filter="resourceType eq 'Microsoft.Network/virtualNetworks'"))
    assert "vnet1" in [r.name for r in everywhere]
    assert resource_client.resources.check_existence(rg, "Microsoft.Network", "", "virtualNetworks", "vnet1", api_version="2024-05-01")
    resource_client.resources.begin_delete(rg, "Microsoft.Network", "", "virtualNetworks", "vnet1", api_version="2024-05-01").result()
    assert not resource_client.resources.check_existence(rg, "Microsoft.Network", "", "virtualNetworks", "vnet1", api_version="2024-05-01")


def test_tags_at_scope(resource_client, rg):
    scope = f"/subscriptions/{SUB}/resourceGroups/{rg}"
    resource_client.tags.begin_create_or_update_at_scope(scope, {"properties": {"tags": {"x": "1"}}}).result()
    got = resource_client.tags.get_at_scope(scope)
    assert got.properties.tags == {"x": "1"}
    resource_client.tags.begin_update_at_scope(scope, {"operation": "Merge", "properties": {"tags": {"y": "2"}}}).result()
    assert resource_client.resource_groups.get(rg).tags == {"x": "1", "y": "2"}
    resource_client.tags.begin_delete_at_scope(scope).result()
    assert not resource_client.resource_groups.get(rg).tags


def test_tag_names(resource_client):
    resource_client.tags.create_or_update("env")
    resource_client.tags.create_or_update_value("env", "prod")
    names = {t.tag_name: t for t in resource_client.tags.list()}
    assert "env" in names
    resource_client.tags.delete_value("env", "prod")
    resource_client.tags.delete("env")
    assert "env" not in [t.tag_name for t in resource_client.tags.list()]


def test_providers(resource_client):
    names = [p.namespace for p in resource_client.providers.list()]
    for ns in ("Microsoft.Storage", "Microsoft.ServiceBus", "Microsoft.EventHub", "Microsoft.KeyVault", "Microsoft.DocumentDB"):
        assert ns in names
    p = resource_client.providers.get("Microsoft.Storage")
    assert p.registration_state == "Registered"
    assert "storageAccounts" in [t.resource_type for t in p.resource_types]
    assert resource_client.providers.register("Microsoft.Storage").registration_state == "Registered"
    assert resource_client.providers.unregister("Microsoft.Storage").namespace == "Microsoft.Storage"


def test_subscriptions_tenants_locations(cred, mgmt):
    with SubscriptionClient(cred, **mgmt) as c:
        subs = list(c.subscriptions.list())
        assert subs
        sub = c.subscriptions.get(SUB)
        assert sub.subscription_id == SUB
        assert sub.state == "Enabled"
        locs = list(c.subscriptions.list_locations(SUB))
        assert "eastus" in [l.name for l in locs]
        assert list(c.tenants.list())


def test_resource_not_found_error_shape(resource_client, rg):
    with pytest.raises(ResourceNotFoundError) as e:
        resource_client.resources.get(rg, "Microsoft.Network", "", "virtualNetworks", "nope", api_version="2024-05-01")
    assert e.value.error.code == "ResourceNotFound"
    with pytest.raises(ResourceNotFoundError) as e:
        resource_client.resource_groups.get("no-such-group")
    assert e.value.error.code == "ResourceGroupNotFound"
