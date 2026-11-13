// Some code samples and query examples in this file are reproduced from Microsoft Learn and from Microsoft's SDKs (MIT licence, Copyright Microsoft); see NOTICE and LICENSES/MIT-Microsoft.txt.
using System.Net;
using Microsoft.Azure.Cosmos;
using Microsoft.Azure.Cosmos.Scripts;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;

namespace Azemu.DotnetTests.Cosmos;

/// <summary>
/// Server-side scripts through the .NET SDK: stored procedure execution, triggers on item operations, a UDF in a query. The scripts are
/// the samples of "How to write stored procedures, triggers, and user-defined functions"
/// (learn.microsoft.com/azure/cosmos-db/nosql/how-to-write-stored-procedures-triggers-udfs).
/// </summary>
public class CosmosScriptTests : CosmosBase, IClassFixture<CosmosFixture>
{
    public CosmosScriptTests(CosmosFixture f) : base(f) { }

    const string CreateItems = @"function createToDoItems(items) {
    var collection = getContext().getCollection();
    var collectionLink = collection.getSelfLink();
    var count = 0;
    if (!items) throw new Error('The array is undefined or null.');
    var numItems = items.length;
    if (numItems == 0) { getContext().getResponse().setBody(0); return; }
    tryCreate(items[count], callback);
    function tryCreate(item, callback) {
        var options = { disableAutomaticIdGeneration: false };
        var isAccepted = collection.createDocument(collectionLink, item, options, callback);
        if (!isAccepted) getContext().getResponse().setBody(count);
    }
    function callback(err, item, options) {
        if (err) throw err;
        count++;
        if (count >= numItems) { getContext().getResponse().setBody(count); } else { tryCreate(items[count], callback); }
    }
}";

    const string PreTimestamp = @"function validateToDoItemTimestamp() {
    var context = getContext();
    var request = context.getRequest();
    var itemToCreate = request.getBody();
    if (!('timestamp' in itemToCreate)) {
        var ts = new Date();
        itemToCreate['timestamp'] = ts.getTime();
    }
    request.setBody(itemToCreate);
}";

    const string PostMetadata = @"function updateMetadata() {
    var context = getContext();
    var container = context.getCollection();
    var response = context.getResponse();
    var createdItem = response.getBody();
    var filterQuery = 'SELECT * FROM root r WHERE r.id = ""_metadata""';
    var accept = container.queryDocuments(container.getSelfLink(), filterQuery, updateMetadataCallback);
    if(!accept) throw 'Unable to update metadata, abort';
    function updateMetadataCallback(err, items, responseOptions) {
        if(err) throw new Error('Error' + err.message);
        if(items.length != 1) throw 'Unable to find metadata document';
        var metadataItem = items[0];
        metadataItem.createdItems += 1;
        metadataItem.createdNames += ' ' + createdItem.id;
        var accept = container.replaceDocument(metadataItem._self, metadataItem, function(err, itemReplaced) {
            if(err) throw 'Unable to update metadata, abort';
        });
        if(!accept) throw 'Unable to update metadata, abort';
        return;
    }
}";

    const string Tax = @"function tax(income) {
    if (income == undefined)
        throw 'no input';
    if (income < 1000)
        return income * 0.1;
    else if (income < 10000)
        return income * 0.2;
    else
        return income * 0.4;
}";

    static async Task<List<string>> Ids(Container c, string pk)
    {
        var l = new List<string>();
        var it = c.GetItemQueryIterator<JObject>("SELECT * FROM c", requestOptions: new QueryRequestOptions { PartitionKey = new PartitionKey(pk) });
        while (it.HasMoreResults) l.AddRange((await it.ReadNextAsync()).Select(d => (string)d["id"]!));
        l.Sort(StringComparer.Ordinal);
        return l;
    }

    [Fact]
    public async Task HelloWorld_ReturnsTheResponseBody_WithChargeAndSessionToken()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.Scripts.CreateStoredProcedureAsync(new StoredProcedureProperties("hello", "function () { var context = getContext(); var response = context.getResponse(); response.setBody('Hello, World'); }"));
        var r = await c.Scripts.ExecuteStoredProcedureAsync<string>("hello", new PartitionKey("p"), Array.Empty<dynamic>());
        Assert.Equal(HttpStatusCode.OK, r.StatusCode);
        Assert.Equal("Hello, World", r.Resource);
        Assert.True(r.RequestCharge > 0);
        Assert.False(string.IsNullOrEmpty(r.Headers.Session));
    }

    [Fact]
    public async Task CreateItems_FromAnArrayParameter_ThenAnExceptionRollsEverythingBack()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.Scripts.CreateStoredProcedureAsync(new StoredProcedureProperties("create", CreateItems));
        dynamic[] items = { new { id = "a", pk = "p" }, new { id = "b", pk = "p" }, new { pk = "p", name = "generated id" } };
        var ok = await c.Scripts.ExecuteStoredProcedureAsync<int>("create", new PartitionKey("p"), new dynamic[] { items });
        Assert.Equal(3, ok.Resource);
        Assert.Equal(3, (await Ids(c, "p")).Count);

        dynamic[] dup = { new { id = "x", pk = "q" }, new { id = "y", pk = "q" }, new { id = "x", pk = "q" } };
        var ex = await Fails(async () => await c.Scripts.ExecuteStoredProcedureAsync<int>("create", new PartitionKey("q"), new dynamic[] { dup }));
        Assert.Equal(HttpStatusCode.BadRequest, ex.StatusCode);
        Assert.Contains("Encountered exception while executing function", ex.Message);
        Assert.Empty(await Ids(c, "q"));
    }

    [Fact]
    public async Task ScriptLogging_IsReturnedWhenAskedFor()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.Scripts.CreateStoredProcedureAsync(new StoredProcedureProperties("log", "function () { console.log('hello'); getContext().getResponse().setBody(1); }"));
        var withLog = await c.Scripts.ExecuteStoredProcedureAsync<int>("log", new PartitionKey("p"), Array.Empty<dynamic>(), new StoredProcedureRequestOptions { EnableScriptLogging = true });
        Assert.Equal("hello", withLog.ScriptLog);
        var without = await c.Scripts.ExecuteStoredProcedureAsync<int>("log", new PartitionKey("p"), Array.Empty<dynamic>());
        Assert.True(string.IsNullOrEmpty(without.ScriptLog));
    }

    [Fact]
    public async Task PreTrigger_AddsATimestamp_AndOnlyWhenNamed()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.Scripts.CreateTriggerAsync(new TriggerProperties { Id = "ts", Body = PreTimestamp, TriggerOperation = TriggerOperation.Create, TriggerType = TriggerType.Pre });
        var withTrigger = await c.CreateItemAsync(new JObject { ["id"] = "a", ["pk"] = "p" }, new PartitionKey("p"), new ItemRequestOptions { PreTriggers = new List<string> { "ts" } });
        Assert.True((long)withTrigger.Resource["timestamp"]! > 1_600_000_000_000);
        var without = await c.CreateItemAsync(new JObject { ["id"] = "b", ["pk"] = "p" }, new PartitionKey("p"));
        Assert.Null(without.Resource["timestamp"]);
        var stored = await c.ReadItemAsync<JObject>("a", new PartitionKey("p"));
        Assert.Equal((long)withTrigger.Resource["timestamp"]!, (long)stored.Resource["timestamp"]!);
    }

    [Fact]
    public async Task PostTrigger_UpdatesTheMetadataDocument_AndAThrowingOneUndoesTheItem()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.Scripts.CreateTriggerAsync(new TriggerProperties { Id = "meta", Body = PostMetadata, TriggerOperation = TriggerOperation.Create, TriggerType = TriggerType.Post });
        await c.CreateItemAsync(new JObject { ["id"] = "_metadata", ["pk"] = "p", ["createdItems"] = 0, ["createdNames"] = "" }, new PartitionKey("p"));
        var opts = new ItemRequestOptions { PostTriggers = new List<string> { "meta" } };
        await c.CreateItemAsync(new JObject { ["id"] = "one", ["pk"] = "p" }, new PartitionKey("p"), opts);
        await c.CreateItemAsync(new JObject { ["id"] = "two", ["pk"] = "p" }, new PartitionKey("p"), opts);
        var meta = (await c.ReadItemAsync<JObject>("_metadata", new PartitionKey("p"))).Resource;
        Assert.Equal(2, (int)meta["createdItems"]!);
        Assert.Equal(" one two", (string)meta["createdNames"]!);
        var ex = await Fails(async () => await c.CreateItemAsync(new JObject { ["id"] = "n", ["pk"] = "other" }, new PartitionKey("other"), opts));
        Assert.Equal(HttpStatusCode.BadRequest, ex.StatusCode);
        Assert.Empty(await Ids(c, "other"));
    }

    [Fact]
    public async Task Triggers_RunOnReplaceUpsertAndDelete()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.Scripts.CreateTriggerAsync(new TriggerProperties { Id = "stamp", TriggerOperation = TriggerOperation.All, TriggerType = TriggerType.Pre,
            Body = "function () { var r = getContext().getRequest(); var b = r.getBody(); b.op = r.getOperationType(); r.setBody(b); }" });
        await c.Scripts.CreateTriggerAsync(new TriggerProperties { Id = "tomb", TriggerOperation = TriggerOperation.Delete, TriggerType = TriggerType.Post,
            Body = "function () { __.createDocument(__.getSelfLink(), {id: 'tomb', pk: 'p'}); }" });
        var pre = new ItemRequestOptions { PreTriggers = new List<string> { "stamp" } };
        var created = await c.CreateItemAsync(new JObject { ["id"] = "a", ["pk"] = "p" }, new PartitionKey("p"), pre);
        Assert.Equal("Create", (string)created.Resource["op"]!);
        var replaced = await c.ReplaceItemAsync(new JObject { ["id"] = "a", ["pk"] = "p", ["v"] = 2 }, "a", new PartitionKey("p"), pre);
        Assert.Equal("Replace", (string)replaced.Resource["op"]!);
        var upserted = await c.UpsertItemAsync(new JObject { ["id"] = "a", ["pk"] = "p", ["v"] = 3 }, new PartitionKey("p"), pre);
        Assert.Equal("Upsert", (string)upserted.Resource["op"]!);
        await c.DeleteItemAsync<JObject>("a", new PartitionKey("p"), new ItemRequestOptions { PostTriggers = new List<string> { "tomb" } });
        Assert.Equal(new List<string> { "tomb" }, await Ids(c, "p"));
    }

    [Fact]
    public async Task Udf_TaxSample_InWhereAndSelect_AndItsErrorIsA400()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.Scripts.CreateUserDefinedFunctionAsync(new UserDefinedFunctionProperties { Id = "Tax", Body = Tax });
        var incomes = new[] { 500, 5000, 70000, 100000 };
        for (var i = 0; i < incomes.Length; i++)
            await c.CreateItemAsync(new JObject { ["id"] = "i" + i, ["pk"] = i % 2 == 0 ? "a" : "b", ["income"] = incomes[i] }, new PartitionKey(i % 2 == 0 ? "a" : "b"));
        var where = await All(c.GetItemQueryIterator<JObject>("SELECT * FROM Incomes t WHERE udf.Tax(t.income) > 20000"));
        Assert.Equal(new[] { "i2", "i3" }, where.Select(d => (string)d["id"]!).OrderBy(x => x, StringComparer.Ordinal).ToArray());
        var tax = await All(c.GetItemQueryIterator<double>("SELECT VALUE udf.Tax(t.income) FROM t"));
        Assert.Equal(new double[] { 50, 1000, 28000, 40000 }, tax.OrderBy(x => x).ToArray());
        var ex = await Fails(async () => await All(c.GetItemQueryIterator<JObject>("SELECT VALUE udf.Tax(t.nothing) FROM t", requestOptions: new QueryRequestOptions { PartitionKey = new PartitionKey("a") })));
        Assert.Equal(HttpStatusCode.BadRequest, ex.StatusCode);
        Assert.Contains("no input", ex.Message);
    }
}
