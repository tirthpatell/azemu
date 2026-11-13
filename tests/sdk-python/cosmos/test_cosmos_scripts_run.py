# Some code samples and query examples in this file are reproduced from Microsoft Learn and from Microsoft's SDKs (MIT licence, Copyright Microsoft); see NOTICE and LICENSES/MIT-Microsoft.txt.
"""Running stored procedures, triggers and user defined functions through the `azure-cosmos` SDK.

The scripts are the samples of "How to write stored procedures, triggers, and user-defined functions"
(learn.microsoft.com/azure/cosmos-db/nosql/how-to-write-stored-procedures-triggers-udfs) and the sample scripts of
github.com/Azure/azure-cosmosdb-js-server, or small variations of them.
"""

import urllib.parse

import pytest
from azure.cosmos import documents
from azure.cosmos.exceptions import CosmosHttpResponseError, CosmosResourceNotFoundError

pytestmark = pytest.mark.not_on_vnext(reason="vnext has no server-side JavaScript")

HELLO = """function () {
    var context = getContext();
    var response = context.getResponse();
    response.setBody("Hello, World");
}"""

CREATE_ITEMS = """function createToDoItems(items) {
    var collection = getContext().getCollection();
    var collectionLink = collection.getSelfLink();
    var count = 0;
    if (!items) throw new Error("The array is undefined or null.");
    var numItems = items.length;
    if (numItems == 0) {
        getContext().getResponse().setBody(0);
        return;
    }
    tryCreate(items[count], callback);
    function tryCreate(item, callback) {
        var options = { disableAutomaticIdGeneration: false };
        var isAccepted = collection.createDocument(collectionLink, item, options, callback);
        if (!isAccepted) getContext().getResponse().setBody(count);
    }
    function callback(err, item, options) {
        if (err) throw err;
        count++;
        if (count >= numItems) {
            getContext().getResponse().setBody(count);
        } else {
            tryCreate(items[count], callback);
        }
    }
}"""

PRE_TIMESTAMP = """function validateToDoItemTimestamp() {
    var context = getContext();
    var request = context.getRequest();
    var itemToCreate = request.getBody();
    if (!("timestamp" in itemToCreate)) {
        var ts = new Date();
        itemToCreate["timestamp"] = ts.getTime();
    }
    request.setBody(itemToCreate);
}"""

POST_METADATA = """function updateMetadata() {
    var context = getContext();
    var container = context.getCollection();
    var response = context.getResponse();
    var createdItem = response.getBody();
    var filterQuery = 'SELECT * FROM root r WHERE r.id = "_metadata"';
    var accept = container.queryDocuments(container.getSelfLink(), filterQuery, updateMetadataCallback);
    if(!accept) throw "Unable to update metadata, abort";
    function updateMetadataCallback(err, items, responseOptions) {
        if(err) throw new Error("Error" + err.message);
        if(items.length != 1) throw 'Unable to find metadata document';
        var metadataItem = items[0];
        metadataItem.createdItems += 1;
        metadataItem.createdNames += " " + createdItem.id;
        var accept = container.replaceDocument(metadataItem._self, metadataItem, function(err, itemReplaced) {
            if(err) throw "Unable to update metadata, abort";
        });
        if(!accept) throw "Unable to update metadata, abort";
        return;
    }
}"""

TAX = """function tax(income) {
    if (income == undefined)
        throw 'no input';
    if (income < 1000)
        return income * 0.1;
    else if (income < 10000)
        return income * 0.2;
    else
        return income * 0.4;
}"""


def sproc(container, id, body):
    return container.scripts.create_stored_procedure({"id": id, "serverScript": body})


def trigger(container, id, body, kind, op):
    return container.scripts.create_trigger({"id": id, "serverScript": body, "triggerType": kind, "triggerOperation": op})


def ids(container, pk):
    return sorted(d["id"] for d in container.query_items("SELECT * FROM c", partition_key=pk))


# ---- stored procedures --------------------------------------------------------
def test_hello_world(container):
    sproc(container, "hello", HELLO)
    assert container.scripts.execute_stored_procedure("hello", partition_key="p") == "Hello, World"


def test_parameters_and_the_response_body_of_any_json_type(container):
    sproc(container, "echo", "function (a, b) { getContext().getResponse().setBody({sum: a + b, list: [a, b], none: null}); }")
    assert container.scripts.execute_stored_procedure("echo", partition_key="p", params=[2, 3]) == {"sum": 5, "list": [2, 3], "none": None}
    sproc(container, "num", "function () { getContext().getResponse().setBody(42); }")
    assert container.scripts.execute_stored_procedure("num", partition_key="p") == 42
    sproc(container, "nothing", "function () { var x = 1; }")
    assert container.scripts.execute_stored_procedure("nothing", partition_key="p") in (None, "")


def test_the_sproc_object_can_be_passed_as_well_as_its_id(container):
    created = sproc(container, "hello", HELLO)
    assert container.scripts.execute_stored_procedure(created, partition_key="p") == "Hello, World"


def test_create_documents_from_a_sproc_and_read_them_back(container):
    sproc(container, "create", CREATE_ITEMS)
    items = [{"id": "a", "pk": "p", "name": "Groceries"}, {"id": "b", "pk": "p", "name": "Doctor"}, {"pk": "p", "name": "No id"}]
    assert container.scripts.execute_stored_procedure("create", partition_key="p", params=[items]) == 3
    assert len(ids(container, "p")) == 3
    assert container.read_item("a", partition_key="p")["name"] == "Groceries"


def test_an_exception_rolls_back_every_write_and_is_a_400(container):
    sproc(container, "create", CREATE_ITEMS)
    items = [{"id": "a", "pk": "p"}, {"id": "b", "pk": "p"}, {"id": "a", "pk": "p"}]
    with pytest.raises(CosmosHttpResponseError) as e:
        container.scripts.execute_stored_procedure("create", partition_key="p", params=[items])
    assert e.value.status_code == 400
    assert "Encountered exception while executing function" in e.value.message
    assert ids(container, "p") == []


def test_throw_new_error_message_is_in_the_answer(container):
    sproc(container, "boom", "function () { getContext().getCollection().createDocument(getContext().getCollection().getSelfLink(), {id: 'x', pk: 'p'}); throw new Error('this failed'); }")
    with pytest.raises(CosmosHttpResponseError) as e:
        container.scripts.execute_stored_procedure("boom", partition_key="p")
    assert e.value.status_code == 400 and "Error: this failed" in e.value.message
    assert ids(container, "p") == []


def test_a_sproc_sees_only_its_partition(container):
    container.create_item({"id": "mine", "pk": "p"})
    container.create_item({"id": "theirs", "pk": "q"})
    sproc(container, "count", "function () { __.queryDocuments(__.getSelfLink(), 'SELECT VALUE c.id FROM c', function (e, docs) { getContext().getResponse().setBody(docs); }); }")
    assert container.scripts.execute_stored_procedure("count", partition_key="p") == ["mine"]
    assert container.scripts.execute_stored_procedure("count", partition_key="q") == ["theirs"]


def test_bulk_import_resumes_after_a_call_is_not_accepted(container):
    """The documented bulk import: createDocument answers false when the script has used its budget; the client calls again."""
    body = """function bulkImport(items) {
    var container = getContext().getCollection();
    var containerLink = container.getSelfLink();
    var count = 0;
    if (!items) throw new Error("The array is undefined or null.");
    var itemsLength = items.length;
    if (itemsLength == 0) { getContext().getResponse().setBody(0); }
    tryCreate(items[count], callback);
    function tryCreate(item, callback) {
        var isAccepted = container.createDocument(containerLink, item, callback);
        if (!isAccepted) getContext().getResponse().setBody(count);
    }
    function callback(err, item, options) {
        if (err) throw err;
        count++;
        if (count >= itemsLength) { getContext().getResponse().setBody(count); } else { tryCreate(items[count], callback); }
    }
}"""
    sproc(container, "import", body)
    items = [{"id": f"i{n:04d}", "pk": "p", "n": n} for n in range(300)]
    done, calls = 0, 0
    while done < len(items):
        imported = container.scripts.execute_stored_procedure("import", partition_key="p", params=[items[done:]])
        assert imported > 0
        done += imported
        calls += 1
        assert calls < 50
    assert len(ids(container, "p")) == 300


def test_script_logging_header_and_enable_flag(container):
    sproc(container, "log", "function () { console.log('hello'); console.log('n =', 3); getContext().getResponse().setBody(1); }")
    container.scripts.execute_stored_procedure("log", partition_key="p", enable_script_logging=True)
    seen = container.client_connection.last_response_headers
    assert urllib.parse.unquote(seen["x-ms-documentdb-script-log-results"]) == "hello\nn = 3"
    container.scripts.execute_stored_procedure("log", partition_key="p")
    assert "x-ms-documentdb-script-log-results" not in container.client_connection.last_response_headers


def test_request_charge_and_session_token_are_returned(container):
    sproc(container, "hello", HELLO)
    container.scripts.execute_stored_procedure("hello", partition_key="p")
    seen = container.client_connection.last_response_headers
    assert float(seen["x-ms-request-charge"]) > 0
    assert "x-ms-session-token" in seen


def test_executing_a_missing_sproc_is_404(container):
    with pytest.raises(CosmosResourceNotFoundError):
        container.scripts.execute_stored_procedure("nope", partition_key="p")


def test_an_infinite_loop_is_a_clean_error_and_the_service_goes_on(container):
    sproc(container, "spin", "function () { while (true) {} }")
    with pytest.raises(CosmosHttpResponseError) as e:
        container.scripts.execute_stored_procedure("spin", partition_key="p")
    assert e.value.status_code in (400, 408)
    sproc(container, "hello", HELLO)
    assert container.scripts.execute_stored_procedure("hello", partition_key="p") == "Hello, World"


def test_the_javascript_query_api(container):
    for n in range(5):
        container.create_item({"id": f"d{n}", "pk": "p", "n": n, "Tags": [f"t{n}", "all"]})
    sproc(container, "chain", """function () {
        __.chain().filter(function (d) { return d.n >= 3; }).sortByDescending(function (d) { return d.n; }).pluck("Tags").flatten().value();
    }""")
    assert container.scripts.execute_stored_procedure("chain", partition_key="p") == ["t4", "all", "t3", "all"]


# ---- triggers ----------------------------------------------------------------
def test_pre_trigger_adds_a_timestamp(container):
    trigger(container, "ts", PRE_TIMESTAMP, documents.TriggerType.Pre, documents.TriggerOperation.Create)
    created = container.create_item({"id": "a", "pk": "p"}, pre_trigger_include="ts")
    assert created["timestamp"] > 1_600_000_000_000
    assert container.read_item("a", partition_key="p")["timestamp"] == created["timestamp"]
    plain = container.create_item({"id": "b", "pk": "p"})
    assert "timestamp" not in plain
    keeps = container.create_item({"id": "c", "pk": "p", "timestamp": 7}, pre_trigger_include="ts")
    assert keeps["timestamp"] == 7


def test_post_trigger_updates_the_metadata_document(container):
    trigger(container, "meta", POST_METADATA, documents.TriggerType.Post, documents.TriggerOperation.Create)
    container.create_item({"id": "_metadata", "pk": "p", "createdItems": 0, "createdNames": ""})
    container.create_item({"id": "artist_profile_1023", "pk": "p"}, post_trigger_include="meta")
    container.create_item({"id": "second", "pk": "p"}, post_trigger_include="meta")
    meta = container.read_item("_metadata", partition_key="p")
    assert meta["createdItems"] == 2 and meta["createdNames"] == " artist_profile_1023 second"


def test_a_throwing_post_trigger_undoes_the_item(container):
    trigger(container, "bad", "function () { throw new Error('no'); }", documents.TriggerType.Post, documents.TriggerOperation.All)
    with pytest.raises(CosmosHttpResponseError) as e:
        container.create_item({"id": "never", "pk": "p"}, post_trigger_include="bad")
    assert e.value.status_code == 400
    assert ids(container, "p") == []


def test_triggers_on_replace_upsert_and_delete(container):
    trigger(container, "stamp", "function () { var r = getContext().getRequest(); var b = r.getBody(); b.op = r.getOperationType(); r.setBody(b); }",
            documents.TriggerType.Pre, documents.TriggerOperation.All)
    trigger(container, "tomb", "function () { __.createDocument(__.getSelfLink(), {id: 'tomb', pk: 'p'}); }",
            documents.TriggerType.Post, documents.TriggerOperation.Delete)
    created = container.create_item({"id": "a", "pk": "p"}, pre_trigger_include="stamp")
    assert created["op"] == "Create"
    replaced = container.replace_item("a", {"id": "a", "pk": "p", "v": 2}, pre_trigger_include="stamp")
    assert replaced["op"] == "Replace" and replaced["v"] == 2
    upserted = container.upsert_item({"id": "a", "pk": "p", "v": 3}, pre_trigger_include="stamp")
    assert upserted["op"] == "Upsert"
    container.delete_item("a", partition_key="p", post_trigger_include="tomb")
    assert ids(container, "p") == ["tomb"]


def test_a_trigger_for_another_operation_or_type_is_refused(container):
    trigger(container, "createonly", PRE_TIMESTAMP, documents.TriggerType.Pre, documents.TriggerOperation.Create)
    container.create_item({"id": "a", "pk": "p"})
    with pytest.raises(CosmosHttpResponseError) as e:
        container.replace_item("a", {"id": "a", "pk": "p"}, pre_trigger_include="createonly")
    assert e.value.status_code == 400
    with pytest.raises(CosmosHttpResponseError) as e:
        container.create_item({"id": "b", "pk": "p"}, post_trigger_include="createonly")
    assert e.value.status_code == 400
    with pytest.raises(CosmosHttpResponseError) as e:
        container.create_item({"id": "b", "pk": "p"}, pre_trigger_include="missing")
    assert e.value.status_code == 400


# ---- user defined functions --------------------------------------------------
def test_tax_udf_sample(container):
    container.scripts.create_user_defined_function({"id": "Tax", "serverScript": TAX})
    for i, income in enumerate([500, 5000, 70000, 100000]):
        container.create_item({"id": f"i{i}", "pk": "p", "name": "x", "income": income})
    got = list(container.query_items("SELECT * FROM Incomes t WHERE udf.Tax(t.income) > 20000", partition_key="p"))
    assert sorted(d["id"] for d in got) == ["i2", "i3"]
    got = list(container.query_items("SELECT t.id, udf.Tax(t.income) AS tax FROM t ORDER BY t.id", enable_cross_partition_query=True))
    assert [d["tax"] for d in got] == [50, 1000, 28000, 40000]


def test_udf_with_parameters_across_partitions(container):
    container.scripts.create_user_defined_function({"id": "double", "serverScript": "function (x) { return x * 2; }"})
    for i in range(6):
        container.create_item({"id": f"d{i}", "pk": f"p{i % 3}", "n": i})
    got = list(container.query_items("SELECT VALUE udf.double(c.n) FROM c WHERE c.n >= @m", parameters=[{"name": "@m", "value": 3}],
                                     enable_cross_partition_query=True))
    assert sorted(got) == [6, 8, 10]
    got = list(container.query_items("SELECT VALUE COUNT(1) FROM c WHERE udf.double(c.n) > 4", enable_cross_partition_query=True))
    assert got == [3]


def test_udf_errors_and_unknown_udfs_are_400(container):
    container.scripts.create_user_defined_function({"id": "Tax", "serverScript": TAX})
    container.create_item({"id": "a", "pk": "p"})
    with pytest.raises(CosmosHttpResponseError) as e:
        list(container.query_items("SELECT VALUE udf.Tax(c.missing) FROM c", partition_key="p"))
    assert e.value.status_code == 400 and "no input" in e.value.message
    with pytest.raises(CosmosHttpResponseError) as e:
        list(container.query_items("SELECT VALUE udf.Nope(1) FROM c", partition_key="p"))
    assert e.value.status_code == 400
