"""ARM template deployments with azure-mgmt-resource-deployments.

Bicep compiles to the same JSON; the template below is what `bicep build` produces for a
storage account with a container and a Service Bus queue (written by hand: no Bicep CLI here).
"""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.mgmt.resource.deployments import DeploymentsMgmtClient
from azure.mgmt.servicebus import ServiceBusManagementClient
from azure.mgmt.storage import StorageManagementClient
from azure.storage.blob import BlobServiceClient

from conftest import SUB

TEMPLATE = {
    "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
    "contentVersion": "1.0.0.0",
    "parameters": {
        "storageName": {"type": "string", "minLength": 3, "maxLength": 24},
        "location": {"type": "string", "defaultValue": "[resourceGroup().location]"},
        "queueNames": {"type": "array", "defaultValue": ["alpha", "beta"]},
    },
    "variables": {"nsName": "[concat('bus', uniqueString(resourceGroup().id))]"},
    "resources": [
        {
            "type": "Microsoft.Storage/storageAccounts",
            "apiVersion": "2023-05-01",
            "name": "[parameters('storageName')]",
            "location": "[parameters('location')]",
            "sku": {"name": "Standard_LRS"},
            "kind": "StorageV2",
            "properties": {"minimumTlsVersion": "TLS1_2"},
        },
        {
            "type": "Microsoft.Storage/storageAccounts/blobServices/containers",
            "apiVersion": "2023-05-01",
            "name": "[format('{0}/default/{1}', parameters('storageName'), 'reports')]",
            "dependsOn": ["[resourceId('Microsoft.Storage/storageAccounts', parameters('storageName'))]"],
            "properties": {"publicAccess": "None"},
        },
        {
            "type": "Microsoft.ServiceBus/namespaces",
            "apiVersion": "2022-10-01-preview",
            "name": "[variables('nsName')]",
            "location": "[parameters('location')]",
            "sku": {"name": "Standard", "tier": "Standard"},
        },
        {
            "type": "Microsoft.ServiceBus/namespaces/queues",
            "apiVersion": "2022-10-01-preview",
            "name": "[format('{0}/{1}', variables('nsName'), parameters('queueNames')[copyIndex()])]",
            "copy": {"name": "queues", "count": "[length(parameters('queueNames'))]"},
            "dependsOn": ["[resourceId('Microsoft.ServiceBus/namespaces', variables('nsName'))]"],
            "properties": {},
        },
    ],
    "outputs": {
        "blobEndpoint": {"type": "string", "value": "[reference(parameters('storageName')).primaryEndpoints.blob]"},
        "namespace": {"type": "string", "value": "[variables('nsName')]"},
    },
}


@pytest.fixture
def deployments(cred, mgmt):
    with DeploymentsMgmtClient(cred, SUB, **mgmt) as c:
        yield c


def body(**params):
    return {
        "properties": {
            "mode": "Incremental",
            "template": TEMPLATE,
            "parameters": {k: {"value": v} for k, v in params.items()},
        }
    }


def test_deploy_template_then_use_resources(deployments, cred, mgmt, rg, uniq, emu):
    storage_name = uniq("tplacct")
    result = deployments.deployments.begin_create_or_update(rg, "main", body(storageName=storage_name)).result()
    assert result.name == "main"
    assert result.properties.provisioning_state == "Succeeded"
    assert result.properties.outputs["blobEndpoint"]["value"] == f"http://{storage_name}.blob.core.localhost:{emu.port}/"
    ns = result.properties.outputs["namespace"]["value"]
    assert len(result.properties.output_resources) == 5

    with StorageManagementClient(cred, SUB, **mgmt) as storage:
        assert storage.storage_accounts.get_properties(rg, storage_name).provisioning_state == "Succeeded"
        key = storage.storage_accounts.list_keys(rg, storage_name)["keys"][0].value
        containers = [c.name for c in storage.blob_containers.list(rg, storage_name)]
        assert containers == ["reports"]
    blob = BlobServiceClient(f"http://{storage_name}.blob.core.localhost:{emu.port}/", credential=key)
    blob.get_container_client("reports").upload_blob("r.txt", b"deployed")
    assert blob.get_container_client("reports").download_blob("r.txt").readall() == b"deployed"

    with ServiceBusManagementClient(cred, SUB, **mgmt) as sb:
        assert sorted(q.name for q in sb.queues.list_by_namespace(rg, ns)) == ["alpha", "beta"]

    assert deployments.deployments.check_existence(rg, "main") is True
    assert "main" in [d.name for d in deployments.deployments.list_by_resource_group(rg)]
    assert deployments.deployments.get(rg, "main").properties.template_hash
    ops = list(deployments.deployment_operations.list(rg, "main"))
    assert len(ops) == 5
    assert all(o.properties.provisioning_state == "Succeeded" for o in ops)
    exported = deployments.deployments.export_template(rg, "main")
    assert exported.template["resources"]


def test_validate_then_invalid_parameters(deployments, rg, uniq):
    v = deployments.deployments.begin_validate(rg, "check", body(storageName=uniq("tplacct"))).result()
    assert v.properties.provisioning_state == "Succeeded"
    assert deployments.deployments.check_existence(rg, "check") is False
    with pytest.raises(HttpResponseError) as e:
        deployments.deployments.begin_validate(rg, "check", body(storageName="ab")).result()
    assert e.value.status_code == 400
    # a missing required parameter
    with pytest.raises(HttpResponseError) as e:
        deployments.deployments.begin_create_or_update(rg, "bad", body()).result()
    assert e.value.status_code == 400
    assert "storageName" in str(e.value)


def test_failed_resource_raises_from_the_poller(deployments, rg):
    tpl = {"resources": [{"type": "Microsoft.Storage/storageAccounts", "apiVersion": "2023-05-01", "name": "BAD_NAME", "location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2"}]}
    with pytest.raises(HttpResponseError) as e:
        deployments.deployments.begin_create_or_update(rg, "failing", {"properties": {"mode": "Incremental", "template": tpl}}).result()
    assert "DeploymentFailed" in str(e.value) or "At least one resource" in str(e.value)
    assert deployments.deployments.get(rg, "failing").properties.provisioning_state == "Failed"


def test_delete_deployment_record(deployments, rg):
    tpl = {"resources": []}
    deployments.deployments.begin_create_or_update(rg, "empty", {"properties": {"mode": "Incremental", "template": tpl}}).result()
    deployments.deployments.begin_delete(rg, "empty").result()
    with pytest.raises(ResourceNotFoundError):
        deployments.deployments.get(rg, "empty")
