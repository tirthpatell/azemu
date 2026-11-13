"""Cosmos DB and Event Grid topics through the management SDKs, then the data plane SDKs."""

import pytest
from azure.core.credentials import AzureKeyCredential
from azure.core.exceptions import ResourceNotFoundError
from azure.cosmos import CosmosClient
from azure.cosmos.documents import SSLConfiguration
from azure.eventgrid import EventGridPublisherClient
from azure.mgmt.cosmosdb import CosmosDBManagementClient
from azure.mgmt.eventgrid import EventGridManagementClient

from conftest import SUB


@pytest.fixture
def cdb(cred, mgmt):
    with CosmosDBManagementClient(cred, SUB, **mgmt) as c:
        yield c


@pytest.fixture
def egm(cred, mgmt):
    with EventGridManagementClient(cred, SUB, **mgmt) as c:
        yield c


def make_account(cdb, rg, name):
    return cdb.database_accounts.begin_create_or_update(
        rg, name,
        {"location": "eastus", "kind": "GlobalDocumentDB", "properties": {"databaseAccountOfferType": "Standard", "locations": [{"locationName": "East US", "failoverPriority": 0}]}},
    ).result()


def cosmos_client(acct, key, emu):
    return CosmosClient(acct.document_endpoint, credential=key, connection_verify=emu.ca_file, ssl_config=SSLConfiguration(), retry_total=1, retry_throttle_total=1)



def test_cosmos_account_database_container(cdb, rg, uniq, emu):
    name = uniq("cosmos-")
    acct = make_account(cdb, rg, name)
    assert acct.document_endpoint == f"https://{name}.documents.localhost:{emu.port}/"
    assert acct.kind == "GlobalDocumentDB"
    assert acct.provisioning_state == "Succeeded"
    cdb.sql_resources.begin_create_update_sql_database(rg, name, "db1", {"properties": {"resource": {"id": "db1"}, "options": {"throughput": 400}}}).result()
    cdb.sql_resources.begin_create_update_sql_container(
        rg, name, "db1", "items",
        {"properties": {"resource": {"id": "items", "partitionKey": {"paths": ["/pk"], "kind": "Hash"}}}},
    ).result()
    assert [d.name for d in cdb.sql_resources.list_sql_databases(rg, name)] == ["db1"]
    cont = cdb.sql_resources.get_sql_container(rg, name, "db1", "items")
    assert cont.resource.partition_key.paths == ["/pk"]
    tp = cdb.sql_resources.get_sql_database_throughput(rg, name, "db1")
    assert tp.resource.throughput == 400
    key = cdb.database_accounts.list_keys(rg, name)["primaryMasterKey"]
    client = cosmos_client(acct, key, emu)
    container = client.get_database_client("db1").get_container_client("items")
    container.create_item({"id": "a", "pk": "x", "n": 1})
    assert container.read_item("a", partition_key="x")["n"] == 1
    cs = cdb.database_accounts.list_connection_strings(rg, name)
    assert cs.connection_strings[0].connection_string.startswith(f"AccountEndpoint=https://{name}.documents.localhost")
    cdb.sql_resources.begin_delete_sql_container(rg, name, "db1", "items").result()
    with pytest.raises(Exception):
        container.read_item("a", partition_key="x")
    cdb.database_accounts.begin_delete(rg, name).result()
    with pytest.raises(ResourceNotFoundError):
        cdb.database_accounts.get(rg, name)


def test_cosmos_readonly_keys_and_regenerate(cdb, rg, uniq):
    name = uniq("cosmos-")
    make_account(cdb, rg, name)
    ro = cdb.database_accounts.get_read_only_keys(rg, name)
    assert ro.primary_readonly_master_key
    before = cdb.database_accounts.list_keys(rg, name)["primaryMasterKey"]
    cdb.database_accounts.begin_regenerate_key(rg, name, {"keyKind": "primary"}).result()
    after = cdb.database_accounts.list_keys(rg, name)["primaryMasterKey"]
    assert after != before


def test_event_grid_topic(egm, rg, uniq, emu):
    name = uniq("topic-")
    topic = egm.topics.begin_create_or_update(rg, name, {"location": "eastus", "properties": {"inputSchema": "CloudEventSchemaV1_0"}}).result()
    assert topic.name == name
    assert topic.endpoint == f"https://{name}.eventgrid.localhost:{emu.port}/api/events"
    assert topic.input_schema == "CloudEventSchemaV1_0"
    keys = egm.topics.list_shared_access_keys(rg, name)
    assert keys.key1 and keys.key2
    new = egm.topics.begin_regenerate_key(rg, name, {"keyName": "key1"}).result()
    assert new.key1 != keys.key1
    # The Event Grid service sees the topic and accepts events on its endpoint.
    import json
    import urllib.request

    req = urllib.request.Request(f"http://localhost:{emu.port}/_azemu/eventgrid/topics/{name}")
    with urllib.request.urlopen(req) as r:
        assert json.load(r)["name"] == name
    # The publisher refuses https without the CA; the same port answers plain http.
    with EventGridPublisherClient(topic.endpoint.replace("https://", "http://"), AzureKeyCredential(new.key1), retry_total=0) as pub:
        # the topic was created with CloudEventSchemaV1_0, so the data plane wants cloud events
        from azure.core.exceptions import HttpResponseError
        from azure.core.messaging import CloudEvent

        pub.send([CloudEvent(source="/test", type="arm.test", data={"a": 1})])
        with pytest.raises(HttpResponseError):
            pub.send([{"id": "1", "subject": "s", "data": {"a": 1}, "eventType": "t", "eventTime": "2026-01-01T00:00:00Z", "dataVersion": "1.0"}])
    assert name in [t.name for t in egm.topics.list_by_resource_group(rg)]
    egm.topics.begin_delete(rg, name).result()
    with pytest.raises(ResourceNotFoundError):
        egm.topics.get(rg, name)


def test_cosmos_account_properties_are_accepted_and_returned(cdb, rg, uniq, raw):
    name = uniq("cosmos-")
    path = f"/subscriptions/{SUB}/resourceGroups/{rg}/providers/Microsoft.DocumentDB/databaseAccounts/{name}"
    body = {
        "location": "eastus",
        "kind": "GlobalDocumentDB",
        "properties": {
            "databaseAccountOfferType": "Standard",
            "locations": [{"locationName": "East US", "failoverPriority": 0}],
            "enableAllVersionsAndDeletesChangeFeed": True,
            "capabilities": [{"name": "EnableServerless"}],
            "enableAnalyticalStorage": True,
            "minimalTlsVersion": "Tls12",
            "disableLocalAuth": True,
            "publicNetworkAccess": "Disabled",
            "ipRules": [{"ipAddressOrRange": "10.0.0.1"}],
            "capacity": {"totalThroughputLimit": 4000},
            "cors": [{"allowedOrigins": "https://example.com"}],
            "networkAclBypass": "AzureServices",
            "consistencyPolicy": {"defaultConsistencyLevel": "Strong"},
            "backupPolicy": None,   # the azurerm provider sends null; Azure answers with the default policy
        },
    }
    status, got = raw("PUT", path, body, api="2024-11-15")
    assert got["properties"]["backupPolicy"]["type"] == "Periodic"
    assert [f["locationName"] for f in got["properties"]["failoverPolicies"]] == ["East US"]
    # the name check the azurerm provider makes before it creates an account
    assert raw("HEAD", f"/providers/Microsoft.DocumentDB/databaseAccountNames/{name}", api="2024-11-15")[0] == 200
    assert raw("HEAD", f"/providers/Microsoft.DocumentDB/databaseAccountNames/{name}x", api="2024-11-15")[0] == 404
    assert status in (200, 201, 202), got
    status, got = raw("GET", path, api="2024-11-15")
    assert status == 200
    p = got["properties"]
    assert p["enableAllVersionsAndDeletesChangeFeed"] is True
    assert p["capabilities"] == [{"name": "EnableServerless"}]
    assert p["enableAnalyticalStorage"] is True
    assert p["disableLocalAuth"] is True
    assert p["publicNetworkAccess"] == "Disabled"
    assert p["ipRules"] == [{"ipAddressOrRange": "10.0.0.1"}]
    assert p["capacity"] == {"totalThroughputLimit": 4000}
    assert p["cors"] == [{"allowedOrigins": "https://example.com"}]
    assert p["networkAclBypass"] == "AzureServices"
    assert p["consistencyPolicy"]["defaultConsistencyLevel"] == "Strong"
    # what the account does not set comes back with Azure's defaults
    assert p["enableBurstCapacity"] is False
    assert p["enablePartitionMerge"] is False
    assert p["defaultIdentity"] == "FirstPartyIdentity"
    # PATCH changes one property and keeps the rest
    status, got = raw("PATCH", path, {"properties": {"enableAllVersionsAndDeletesChangeFeed": False}}, api="2024-11-15")
    assert status in (200, 202), got
    p = raw("GET", path, api="2024-11-15")[1]["properties"]
    assert p["enableAllVersionsAndDeletesChangeFeed"] is False
    assert p["enableAnalyticalStorage"] is True
