using System.Net;
using System.Text;
using Microsoft.Azure.Cosmos;
using Newtonsoft.Json;

namespace Azemu.DotnetTests.Cosmos;

public class CosmosCrudTests : CosmosBase, IClassFixture<CosmosFixture>
{
    public CosmosCrudTests(CosmosFixture f) : base(f) { }

    [Fact]
    public async Task ReadAccount_ShowsTheRegionsTheClientCanUse()
    {
        using var c = Client();
        var acct = await c.ReadAccountAsync();
        Assert.NotEmpty(acct.ReadableRegions);
        Assert.NotEmpty(acct.WritableRegions);
        Assert.False(string.IsNullOrEmpty(acct.Id));
    }

    [Fact]
    public async Task CreateDatabase_Returns201_AndTwiceReturns409Conflict()
    {
        using var c = Client();
        var id = Names.Unique("db");
        var r = await c.CreateDatabaseAsync(id);
        Assert.Equal(HttpStatusCode.Created, r.StatusCode);
        Assert.Equal(id, r.Resource.Id);
        Assert.False(string.IsNullOrEmpty(r.Resource.ETag));
        var ex = await Fails(async () => await c.CreateDatabaseAsync(id));
        Assert.Equal(HttpStatusCode.Conflict, ex.StatusCode);
    }

    [Fact]
    public async Task CreateDatabaseIfNotExists_Returns201ThenOk()
    {
        using var c = Client();
        var id = Names.Unique("db");
        Assert.Equal(HttpStatusCode.Created, (await c.CreateDatabaseIfNotExistsAsync(id)).StatusCode);
        Assert.Equal(HttpStatusCode.OK, (await c.CreateDatabaseIfNotExistsAsync(id)).StatusCode);
    }

    [Fact]
    public async Task ReadAndDeleteDatabase_ThenReadIs404()
    {
        using var c = Client();
        var db = (await c.CreateDatabaseAsync(Names.Unique("db"))).Database;
        Assert.Equal(HttpStatusCode.OK, (await db.ReadAsync()).StatusCode);
        Assert.Equal(HttpStatusCode.NoContent, (await db.DeleteAsync()).StatusCode);
        Assert.Equal(HttpStatusCode.NotFound, (await Fails(async () => await db.ReadAsync())).StatusCode);
    }

    [Fact]
    public async Task ListDatabases_ContainsTheCreatedOnes()
    {
        using var c = Client();
        var ids = new[] { Names.Unique("db"), Names.Unique("db") };
        foreach (var i in ids) await c.CreateDatabaseAsync(i);
        var found = (await All(c.GetDatabaseQueryIterator<DatabaseProperties>())).Select(d => d.Id).ToList();
        foreach (var i in ids) Assert.Contains(i, found);
        var q = await All(c.GetDatabaseQueryIterator<DatabaseProperties>(new QueryDefinition("SELECT * FROM d WHERE d.id = @id").WithParameter("@id", ids[0])));
        Assert.Single(q);
    }

    [Fact]
    public async Task CreateContainer_KeepsThePartitionKeyPathAndDefaults()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        var p = (await c.ReadContainerAsync()).Resource;
        Assert.Equal("/pk", p.PartitionKeyPath);
        Assert.Equal(IndexingMode.Consistent, p.IndexingPolicy.IndexingMode);
        Assert.True(p.IndexingPolicy.Automatic);
        Assert.NotNull(p.ETag);
    }

    [Fact]
    public async Task CreateContainer_WithManualThroughput_ReadsItBack()
    {
        var (client, _, c) = await Setup(throughput: 600);
        using var _c = client;
        Assert.Equal(600, await c.ReadThroughputAsync());
        var tp = await c.ReadThroughputAsync(new RequestOptions());
        Assert.Equal(600, tp.Resource.Throughput);
        await c.ReplaceThroughputAsync(800);
        Assert.Equal(800, await c.ReadThroughputAsync());
    }

    [Fact]
    public async Task ReplaceContainer_ChangesIndexingPolicyAndDefaultTtl()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        var p = (await c.ReadContainerAsync()).Resource;
        p.DefaultTimeToLive = 3600;
        p.IndexingPolicy.ExcludedPaths.Add(new ExcludedPath { Path = "/big/*" });
        var r = await c.ReplaceContainerAsync(p);
        Assert.Equal(3600, r.Resource.DefaultTimeToLive);
        Assert.Contains(r.Resource.IndexingPolicy.ExcludedPaths, e => e.Path == "/big/*");
    }

    [Fact]
    public async Task UniqueKeyPolicy_RejectsASecondItemWithTheSameKey()
    {
        var (client, _, c) = await Setup(cfg: p => p.UniqueKeyPolicy.UniqueKeys.Add(new UniqueKey { Paths = { "/name" } }));
        using var _c = client;
        await c.CreateItemAsync(D("1", name: "same"), new PartitionKey("p1"));
        var ex = await Fails(async () => await c.CreateItemAsync(D("2", name: "same"), new PartitionKey("p1")));
        Assert.Equal(HttpStatusCode.Conflict, ex.StatusCode);
        await c.CreateItemAsync(D("3", pk: "p2", name: "same"), new PartitionKey("p2"));   // another partition: allowed
    }

    [Fact]
    public async Task DeleteContainer_ThenItemsFailWith404()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.DeleteContainerAsync();
        Assert.Equal(HttpStatusCode.NotFound, (await Fails(async () => await c.ReadContainerAsync())).StatusCode);
    }

    [Fact]
    public async Task CreateItem_Returns201WithEtagAndRequestCharge()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        var r = await c.CreateItemAsync(D("a", n: 5), new PartitionKey("p1"));
        Assert.Equal(HttpStatusCode.Created, r.StatusCode);
        Assert.Equal(5, r.Resource.N);
        Assert.False(string.IsNullOrEmpty(r.ETag));
        Assert.True(r.RequestCharge > 0);
        Assert.False(string.IsNullOrEmpty(r.Headers.Session));
        Assert.False(string.IsNullOrEmpty(r.ActivityId));
    }

    [Fact]
    public async Task CreateItem_Duplicate_Fails409()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.CreateItemAsync(D("a"), new PartitionKey("p1"));
        Assert.Equal(HttpStatusCode.Conflict, (await Fails(async () => await c.CreateItemAsync(D("a"), new PartitionKey("p1")))).StatusCode);
    }

    [Fact]
    public async Task SameIdInTwoPartitions_AreTwoItems()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.CreateItemAsync(D("a", "p1", 1), new PartitionKey("p1"));
        await c.CreateItemAsync(D("a", "p2", 2), new PartitionKey("p2"));
        Assert.Equal(1, (await c.ReadItemAsync<Doc>("a", new PartitionKey("p1"))).Resource.N);
        Assert.Equal(2, (await c.ReadItemAsync<Doc>("a", new PartitionKey("p2"))).Resource.N);
    }

    [Fact]
    public async Task ReadItem_ReturnsTheDocument_AndMissingIs404()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.CreateItemAsync(new Doc { Id = "a", Pk = "p1", N = 7, Name = "héllo ✓", Tags = new() { "x", "y" }, Nested = new() { A = 1, B = "b" } }, new PartitionKey("p1"));
        var r = (await c.ReadItemAsync<Doc>("a", new PartitionKey("p1"))).Resource;
        Assert.Equal("héllo ✓", r.Name);
        Assert.Equal(new[] { "x", "y" }, r.Tags);
        Assert.Equal(1, r.Nested!.A);
        var ex = await Fails(async () => await c.ReadItemAsync<Doc>("nope", new PartitionKey("p1")));
        Assert.Equal(HttpStatusCode.NotFound, ex.StatusCode);
    }

    [Theory]
    [InlineData("with space")]
    [InlineData("ünïcode-日本")]
    [InlineData("a+b=c&d")]
    [InlineData("semi;colon,comma")]
    [InlineData("quote'dq\"")]
    public async Task ItemIds_WithSpecialCharacters_RoundTrip(string id)
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.CreateItemAsync(D(id), new PartitionKey("p1"));
        Assert.Equal(id, (await c.ReadItemAsync<Doc>(id, new PartitionKey("p1"))).Resource.Id);
        Assert.Equal(id, (await c.ReplaceItemAsync(D(id, n: 2), id, new PartitionKey("p1"))).Resource.Id);
        Assert.Equal(HttpStatusCode.NoContent, (await c.DeleteItemAsync<Doc>(id, new PartitionKey("p1"))).StatusCode);
    }

    [Fact]
    public async Task ReplaceItem_ChangesTheDocumentAndTheEtag()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        var created = await c.CreateItemAsync(D("a", n: 1), new PartitionKey("p1"));
        var r = await c.ReplaceItemAsync(D("a", n: 2), "a", new PartitionKey("p1"));
        Assert.Equal(HttpStatusCode.OK, r.StatusCode);
        Assert.Equal(2, r.Resource.N);
        Assert.NotEqual(created.ETag, r.ETag);
    }

    [Fact]
    public async Task ReplaceItem_Missing_Fails404()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        Assert.Equal(HttpStatusCode.NotFound, (await Fails(async () => await c.ReplaceItemAsync(D("zz"), "zz", new PartitionKey("p1")))).StatusCode);
    }

    [Fact]
    public async Task UpsertItem_CreatesThenReplaces()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        Assert.Equal(HttpStatusCode.Created, (await c.UpsertItemAsync(D("a", n: 1), new PartitionKey("p1"))).StatusCode);
        var r = await c.UpsertItemAsync(D("a", n: 2), new PartitionKey("p1"));
        Assert.Equal(HttpStatusCode.OK, r.StatusCode);
        Assert.Equal(2, r.Resource.N);
    }

    [Fact]
    public async Task DeleteItem_Returns204_ThenReadIs404_AndDeleteAgainIs404()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.CreateItemAsync(D("a"), new PartitionKey("p1"));
        Assert.Equal(HttpStatusCode.NoContent, (await c.DeleteItemAsync<Doc>("a", new PartitionKey("p1"))).StatusCode);
        Assert.Equal(HttpStatusCode.NotFound, (await Fails(async () => await c.ReadItemAsync<Doc>("a", new PartitionKey("p1")))).StatusCode);
        Assert.Equal(HttpStatusCode.NotFound, (await Fails(async () => await c.DeleteItemAsync<Doc>("a", new PartitionKey("p1")))).StatusCode);
    }

    [Fact]
    public async Task IfMatch_WithAStaleEtag_Fails412_AndWithTheCurrentOneWorks()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        var created = await c.CreateItemAsync(D("a", n: 1), new PartitionKey("p1"));
        await c.ReplaceItemAsync(D("a", n: 2), "a", new PartitionKey("p1"));
        var ex = await Fails(async () => await c.ReplaceItemAsync(D("a", n: 3), "a", new PartitionKey("p1"), new ItemRequestOptions { IfMatchEtag = created.ETag }));
        Assert.Equal(HttpStatusCode.PreconditionFailed, ex.StatusCode);
        var current = (await c.ReadItemAsync<Doc>("a", new PartitionKey("p1"))).ETag;
        Assert.Equal(3, (await c.ReplaceItemAsync(D("a", n: 3), "a", new PartitionKey("p1"), new ItemRequestOptions { IfMatchEtag = current })).Resource.N);
    }

    [Fact]
    public async Task IfMatch_OnDelete_WithAStaleEtag_Fails412()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.CreateItemAsync(D("a"), new PartitionKey("p1"));
        var ex = await Fails(async () => await c.DeleteItemAsync<Doc>("a", new PartitionKey("p1"), new ItemRequestOptions { IfMatchEtag = "\"00000000-0000-0000-0000-000000000000\"" }));
        Assert.Equal(HttpStatusCode.PreconditionFailed, ex.StatusCode);
    }

    [Fact]
    public async Task IfNoneMatch_WithTheCurrentEtag_Returns304NotModified()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        var created = await c.CreateItemAsync(D("a"), new PartitionKey("p1"));
        var ex = await Fails(async () => await c.ReadItemAsync<Doc>("a", new PartitionKey("p1"), new ItemRequestOptions { IfNoneMatchEtag = created.ETag }));
        Assert.Equal(HttpStatusCode.NotModified, ex.StatusCode);
    }

    [Fact]
    public async Task EnableContentResponseOnWrite_False_ReturnsNoBody()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        var r = await c.CreateItemAsync(D("a"), new PartitionKey("p1"), new ItemRequestOptions { EnableContentResponseOnWrite = false });
        Assert.Equal(HttpStatusCode.Created, r.StatusCode);
        Assert.Null(r.Resource);
        Assert.False(string.IsNullOrEmpty(r.ETag));
    }

    [Fact]
    public async Task StreamApis_ReturnStatusAndBytes_WithoutThrowing()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        var json = JsonConvert.SerializeObject(D("s1", n: 9));
        using (var create = await c.CreateItemStreamAsync(new MemoryStream(Encoding.UTF8.GetBytes(json)), new PartitionKey("p1")))
            Assert.Equal(HttpStatusCode.Created, create.StatusCode);
        using (var read = await c.ReadItemStreamAsync("s1", new PartitionKey("p1")))
        {
            Assert.True(read.IsSuccessStatusCode);
            var d = JsonConvert.DeserializeObject<Doc>(await new StreamReader(read.Content).ReadToEndAsync());
            Assert.Equal(9, d!.N);
        }
        using (var missing = await c.ReadItemStreamAsync("nope", new PartitionKey("p1")))
        {
            Assert.Equal(HttpStatusCode.NotFound, missing.StatusCode);
            Assert.False(missing.IsSuccessStatusCode);
        }
        using (var dup = await c.CreateItemStreamAsync(new MemoryStream(Encoding.UTF8.GetBytes(json)), new PartitionKey("p1")))
            Assert.Equal(HttpStatusCode.Conflict, dup.StatusCode);
        using (var del = await c.DeleteItemStreamAsync("s1", new PartitionKey("p1")))
            Assert.Equal(HttpStatusCode.NoContent, del.StatusCode);
    }

    [Fact]
    public async Task ItemWithoutId_IsRejectedWith400()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        using var r = await c.CreateItemStreamAsync(new MemoryStream(Encoding.UTF8.GetBytes("{\"pk\":\"p1\"}")), new PartitionKey("p1"));
        Assert.Equal(HttpStatusCode.BadRequest, r.StatusCode);
    }

    [Fact]
    public async Task PartitionKeyInBodyDifferentFromHeader_IsRejectedWith400()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        using var r = await c.CreateItemStreamAsync(new MemoryStream(Encoding.UTF8.GetBytes("{\"id\":\"x\",\"pk\":\"other\"}")), new PartitionKey("p1"));
        Assert.Equal(HttpStatusCode.BadRequest, r.StatusCode);
    }

    [Fact]
    public async Task Item_LargerThanTwoMegabytes_Fails413()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        var big = D("big"); big.Name = new string('x', 2_200_000);
        var ex = await Fails(async () => await c.CreateItemAsync(big, new PartitionKey("p1")));
        Assert.Equal(HttpStatusCode.RequestEntityTooLarge, ex.StatusCode);
    }

    [Fact]
    public async Task PartitionKeyTypes_Number_Bool_Null_AndNone_AreSeparatePartitions()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.CreateItemStreamAsync(new MemoryStream(Encoding.UTF8.GetBytes("{\"id\":\"n\",\"pk\":5}")), new PartitionKey(5));
        await c.CreateItemStreamAsync(new MemoryStream(Encoding.UTF8.GetBytes("{\"id\":\"b\",\"pk\":true}")), new PartitionKey(true));
        await c.CreateItemStreamAsync(new MemoryStream(Encoding.UTF8.GetBytes("{\"id\":\"z\",\"pk\":null}")), PartitionKey.Null);
        await c.CreateItemStreamAsync(new MemoryStream(Encoding.UTF8.GetBytes("{\"id\":\"u\"}")), PartitionKey.None);
        foreach (var (id, pk) in new[] { ("n", new PartitionKey(5)), ("b", new PartitionKey(true)), ("z", PartitionKey.Null), ("u", PartitionKey.None) })
        {
            using var r = await c.ReadItemStreamAsync(id, pk);
            Assert.True(r.IsSuccessStatusCode, $"{id}: {r.StatusCode}");
        }
        using var wrong = await c.ReadItemStreamAsync("n", new PartitionKey(6));
        Assert.Equal(HttpStatusCode.NotFound, wrong.StatusCode);
    }

    [Fact]
    public async Task HierarchicalPartitionKeys_CrudAndPrefixQuery()
    {
        var client = Client();
        using var _c = client;
        var db = (await client.CreateDatabaseAsync(Names.Unique("db"))).Database;
        var c = (await db.CreateContainerAsync(new ContainerProperties(Names.Unique("c"), new List<string> { "/tenant", "/user" }))).Container;
        var pk = new PartitionKeyBuilder().Add("t1").Add("u1").Build();
        var item = new { id = "h1", tenant = "t1", user = "u1", v = 1 };
        Assert.Equal(HttpStatusCode.Created, (await c.CreateItemAsync(item, pk)).StatusCode);
        await c.CreateItemAsync(new { id = "h2", tenant = "t1", user = "u2", v = 2 }, new PartitionKeyBuilder().Add("t1").Add("u2").Build());
        await c.CreateItemAsync(new { id = "h3", tenant = "t2", user = "u1", v = 3 }, new PartitionKeyBuilder().Add("t2").Add("u1").Build());
        Assert.Equal(1, (await c.ReadItemAsync<dynamic>("h1", pk)).Resource.v.Value);
        var prefix = new PartitionKeyBuilder().Add("t1").Build();
        var q = await All(c.GetItemQueryIterator<dynamic>("SELECT c.id FROM c", requestOptions: new QueryRequestOptions { PartitionKey = prefix }));
        Assert.Equal(new[] { "h1", "h2" }, q.Select(x => (string)x.id).OrderBy(x => x).ToArray());
        var all = await All(c.GetItemQueryIterator<dynamic>("SELECT c.id FROM c"));
        Assert.Equal(3, all.Count);
        Assert.Equal(HttpStatusCode.NoContent, (await c.DeleteItemAsync<dynamic>("h1", pk)).StatusCode);
    }

    [Fact]
    public async Task DeleteAllItemsByPartitionKey_RemovesOnlyThatPartition()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        for (int i = 0; i < 3; i++) { await c.CreateItemAsync(D("a" + i, "del"), new PartitionKey("del")); await c.CreateItemAsync(D("b" + i, "keep"), new PartitionKey("keep")); }
        var r = await c.DeleteAllItemsByPartitionKeyStreamAsync(new PartitionKey("del"));
        Assert.True(r.IsSuccessStatusCode, r.ErrorMessage);
        Assert.Equal(3, (await All(c.GetItemQueryIterator<Doc>("SELECT * FROM c"))).Count);
    }

    [Fact]
    public async Task Ttl_ContainerDefault_ExpiresItems()
    {
        var (client, _, c) = await Setup(cfg: p => p.DefaultTimeToLive = 2);
        using var _c = client;
        await c.CreateItemAsync(D("a"), new PartitionKey("p1"));
        await Task.Delay(TimeSpan.FromSeconds(4));
        Assert.Equal(HttpStatusCode.NotFound, (await Fails(async () => await c.ReadItemAsync<Doc>("a", new PartitionKey("p1")))).StatusCode);
    }

    [Fact]
    public async Task Ttl_PerItemValue_OverridesTheContainerDefault()
    {
        var (client, _, c) = await Setup(cfg: p => p.DefaultTimeToLive = -1);
        using var _c = client;
        var shortLived = D("short"); shortLived.Ttl = 2;
        await c.CreateItemAsync(shortLived, new PartitionKey("p1"));
        await c.CreateItemAsync(D("forever"), new PartitionKey("p1"));
        await Task.Delay(TimeSpan.FromSeconds(4));
        Assert.Equal(HttpStatusCode.NotFound, (await Fails(async () => await c.ReadItemAsync<Doc>("short", new PartitionKey("p1")))).StatusCode);
        Assert.Equal(HttpStatusCode.OK, (await c.ReadItemAsync<Doc>("forever", new PartitionKey("p1"))).StatusCode);
    }

    [Fact]
    public async Task ConsistencyLevel_Eventual_AndSessionToken_AreAccepted()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        var w = await c.CreateItemAsync(D("a"), new PartitionKey("p1"));
        var r = await c.ReadItemAsync<Doc>("a", new PartitionKey("p1"), new ItemRequestOptions { ConsistencyLevel = ConsistencyLevel.Eventual });
        Assert.Equal(HttpStatusCode.OK, r.StatusCode);
        var s = await c.ReadItemAsync<Doc>("a", new PartitionKey("p1"), new ItemRequestOptions { SessionToken = w.Headers.Session });
        Assert.Equal(HttpStatusCode.OK, s.StatusCode);
    }

    [Fact]
    public async Task Patch_AllOperations_ChangeTheDocument()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.CreateItemAsync(new Doc { Id = "a", Pk = "p1", N = 1, Name = "old", Tags = new() { "x" }, Nested = new() { A = 1, B = "b" } }, new PartitionKey("p1"));
        var r = await c.PatchItemAsync<Doc>("a", new PartitionKey("p1"), new[]
        {
            PatchOperation.Set("/name", "new"),
            PatchOperation.Increment("/n", 10),
            PatchOperation.Add("/tags/-", "y"),
            PatchOperation.Replace("/nested/a", 99),
            PatchOperation.Remove("/nested/b"),
            PatchOperation.Add("/extra", 1),
        });
        Assert.Equal(HttpStatusCode.OK, r.StatusCode);
        Assert.Equal("new", r.Resource.Name);
        Assert.Equal(11, r.Resource.N);
        Assert.Equal(new[] { "x", "y" }, r.Resource.Tags);
        Assert.Equal(99, r.Resource.Nested!.A);
        Assert.Null(r.Resource.Nested.B);
    }

    [Fact]
    public async Task Patch_Move_RelocatesAProperty()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.CreateItemAsync(new { id = "m", pk = "p1", from = "v" }, new PartitionKey("p1"));
        var r = await c.PatchItemAsync<dynamic>("m", new PartitionKey("p1"), new[] { PatchOperation.Move("/from", "/to") });
        Assert.Equal("v", (string)r.Resource.to);
        Assert.Null((object?)r.Resource.from);
    }

    [Fact]
    public async Task Patch_WithAFilterPredicateThatIsFalse_Fails412()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.CreateItemAsync(D("a", n: 1), new PartitionKey("p1"));
        var ex = await Fails(async () => await c.PatchItemAsync<Doc>("a", new PartitionKey("p1"), new[] { PatchOperation.Set("/n", 5) }, new PatchItemRequestOptions { FilterPredicate = "FROM c WHERE c.n = 99" }));
        Assert.Equal(HttpStatusCode.PreconditionFailed, ex.StatusCode);
        var ok = await c.PatchItemAsync<Doc>("a", new PartitionKey("p1"), new[] { PatchOperation.Set("/n", 5) }, new PatchItemRequestOptions { FilterPredicate = "FROM c WHERE c.n = 1" });
        Assert.Equal(5, ok.Resource.N);
    }

    [Fact]
    public async Task Patch_Missing_Fails404()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        var ex = await Fails(async () => await c.PatchItemAsync<Doc>("nope", new PartitionKey("p1"), new[] { PatchOperation.Set("/n", 1) }));
        Assert.Equal(HttpStatusCode.NotFound, ex.StatusCode);
    }

    [Fact]
    public async Task Patch_MoreThanTenOperations_Fails400()
    {
        var (client, _, c) = await Setup();
        using var _c = client;
        await c.CreateItemAsync(D("a"), new PartitionKey("p1"));
        var ops = Enumerable.Range(0, 11).Select(i => PatchOperation.Set("/f" + i, i)).ToList();
        var ex = await Fails(async () => await c.PatchItemAsync<Doc>("a", new PartitionKey("p1"), ops));
        Assert.Equal(HttpStatusCode.BadRequest, ex.StatusCode);
    }

    [Fact]
    public async Task DirectMode_MetadataWorks_ButItemRequestsDoNotReachAnAddress()
    {
        // Direct mode (the SDK default) needs rntbd store addresses from the gateway: azemu has none.
        using var client = Client(ConnectionMode.Direct, cfg: o => { o.RequestTimeout = TimeSpan.FromSeconds(8); o.OpenTcpConnectionTimeout = TimeSpan.FromSeconds(3); });
        var db = (await client.CreateDatabaseAsync(Names.Unique("db"))).Database;
        var c = (await db.CreateContainerAsync(Names.Unique("c"), "/pk")).Container;
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(90));
        var sw = System.Diagnostics.Stopwatch.StartNew();
        var ex = await Assert.ThrowsAsync<CosmosException>(async () => await c.CreateItemAsync(D("a"), new PartitionKey("p1"), cancellationToken: cts.Token));
        // azemu answers GET //addresses/ (the address lookup of direct mode) with 501 and says so
        Assert.Equal(HttpStatusCode.NotImplemented, ex.StatusCode);
        Assert.Contains("addresses", ex.Message);
    }
}
