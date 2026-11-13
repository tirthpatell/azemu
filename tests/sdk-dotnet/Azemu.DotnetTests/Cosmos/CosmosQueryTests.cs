using System.Net;
using Microsoft.Azure.Cosmos;
using Microsoft.Azure.Cosmos.Linq;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;
using System.Collections.ObjectModel;

namespace Azemu.DotnetTests.Cosmos;

public class CosmosQueryTests : CosmosBase, IClassFixture<CosmosFixture>
{
    public CosmosQueryTests(CosmosFixture f) : base(f) { }

    /// <summary>40 items: pk p0..p3, n 0..39, name "item-n", tags, nested.</summary>
    async Task<(CosmosClient, Container)> Seeded(Action<ContainerProperties>? cfg = null)
    {
        var (client, _, c) = await Setup(cfg: cfg, throughput: 400);
        for (int i = 0; i < 40; i++)
            await c.CreateItemAsync(new Doc { Id = "i" + i, Pk = "p" + (i % 4), N = i, Name = "item-" + i, Tags = i % 2 == 0 ? new() { "even", "t" + (i % 3) } : new() { "odd" }, Nested = new() { A = i % 5, B = i % 2 == 0 ? "x" : "y" } }, new PartitionKey("p" + (i % 4)));
        return (client, c);
    }

    [Fact]
    public async Task Query_WithParameters_FiltersAndProjects()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var q = new QueryDefinition("SELECT c.id, c.n FROM c WHERE c.n >= @lo AND c.n < @hi AND c.name != @skip").WithParameter("@lo", 10).WithParameter("@hi", 13).WithParameter("@skip", "item-11");
        var r = await All(c.GetItemQueryIterator<JObject>(q));
        Assert.Equal(new[] { "i10", "i12" }, r.Select(x => (string)x["id"]!).OrderBy(x => x).ToArray());
    }

    [Fact]
    public async Task Query_SinglePartition_ReturnsOnlyThatPartition()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var r = await All(c.GetItemQueryIterator<Doc>("SELECT * FROM c", requestOptions: new QueryRequestOptions { PartitionKey = new PartitionKey("p2") }));
        Assert.Equal(10, r.Count);
        Assert.All(r, d => Assert.Equal("p2", d.Pk));
    }

    [Fact]
    public async Task Query_CrossPartition_ReturnsEverything()
    {
        var (client, c) = await Seeded(); using var _c = client;
        Assert.Equal(40, (await All(c.GetItemQueryIterator<Doc>("SELECT * FROM c"))).Count);
    }

    [Fact]
    public async Task OrderBy_CrossPartition_SortsAscendingAndDescending()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var asc = (await All(c.GetItemQueryIterator<int>("SELECT VALUE c.n FROM c ORDER BY c.n"))).ToList();
        Assert.Equal(Enumerable.Range(0, 40), asc);
        var desc = (await All(c.GetItemQueryIterator<int>("SELECT VALUE c.n FROM c ORDER BY c.n DESC"))).ToList();
        Assert.Equal(Enumerable.Range(0, 40).Reverse(), desc);
    }

    [Fact]
    public async Task OrderBy_OnStringsAndNestedPaths_WithSmallPages()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var r = await All(c.GetItemQueryIterator<JObject>("SELECT c.id, c.nested.a AS a FROM c ORDER BY c.nested.a DESC", requestOptions: new QueryRequestOptions { MaxItemCount = 3 }));
        Assert.Equal(40, r.Count);
        Assert.Equal(r.Select(x => (int)x["a"]!).OrderByDescending(x => x), r.Select(x => (int)x["a"]!));
        var names = await All(c.GetItemQueryIterator<string>("SELECT VALUE c.name FROM c ORDER BY c.name"));
        Assert.Equal(Enumerable.Range(0, 40).Select(i => "item-" + i).OrderBy(x => x, StringComparer.Ordinal), names);
    }

    [Fact]
    public async Task OrderBy_TwoFields_WithoutACompositeIndex_Fails400()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var ex = await Fails(async () => await All(c.GetItemQueryIterator<JObject>("SELECT c.id FROM c ORDER BY c.nested.a DESC, c.n ASC")));
        Assert.Equal(HttpStatusCode.BadRequest, ex.StatusCode);
        Assert.Contains("composite index", ex.Message);
    }

    [Fact]
    public async Task Aggregates_CountSumAvgMinMax_CrossPartition()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var r = (await All(c.GetItemQueryIterator<JObject>("SELECT COUNT(1) AS cnt, SUM(c.n) AS s, AVG(c.n) AS a, MIN(c.n) AS lo, MAX(c.n) AS hi FROM c"))).Single();
        Assert.Equal(40, (int)r["cnt"]!);
        Assert.Equal(780, (int)r["s"]!);
        Assert.Equal(19.5, (double)r["a"]!, 3);
        Assert.Equal(0, (int)r["lo"]!);
        Assert.Equal(39, (int)r["hi"]!);
    }

    [Fact]
    public async Task SelectValueCount_ReturnsOneNumber()
    {
        var (client, c) = await Seeded(); using var _c = client;
        Assert.Equal(new[] { 20 }, await All(c.GetItemQueryIterator<int>("SELECT VALUE COUNT(1) FROM c WHERE c.n % 2 = 0")));
    }

    [Fact]
    public async Task GroupBy_CountsPerGroup()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var r = await All(c.GetItemQueryIterator<JObject>("SELECT c.pk, COUNT(1) AS cnt, SUM(c.n) AS s FROM c GROUP BY c.pk"));
        Assert.Equal(4, r.Count);
        Assert.All(r, g => Assert.Equal(10, (int)g["cnt"]!));
        Assert.Equal(780, r.Sum(g => (int)g["s"]!));
    }

    [Fact]
    public async Task GroupBy_OnNestedValue()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var r = await All(c.GetItemQueryIterator<JObject>("SELECT c.nested.b AS b, COUNT(1) AS cnt FROM c GROUP BY c.nested.b"));
        Assert.Equal(new[] { ("x", 20), ("y", 20) }, r.Select(g => ((string)g["b"]!, (int)g["cnt"]!)).OrderBy(x => x.Item1).ToArray());
    }

    [Fact]
    public async Task Distinct_ReturnsUniqueValues()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var r = await All(c.GetItemQueryIterator<string>("SELECT DISTINCT VALUE c.pk FROM c"));
        Assert.Equal(new[] { "p0", "p1", "p2", "p3" }, r.OrderBy(x => x).ToArray());
    }

    [Fact]
    public async Task Top_LimitsTheCount()
    {
        var (client, c) = await Seeded(); using var _c = client;
        Assert.Equal(5, (await All(c.GetItemQueryIterator<Doc>("SELECT TOP 5 * FROM c"))).Count);
    }

    [Fact]
    public async Task OffsetLimit_WithOrderBy_SkipsAndTakes()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var r = await All(c.GetItemQueryIterator<int>("SELECT VALUE c.n FROM c ORDER BY c.n OFFSET 5 LIMIT 7"));
        Assert.Equal(Enumerable.Range(5, 7), r);
    }

    [Fact]
    public async Task Paging_WithMaxItemCount_ResumesFromTheContinuationToken()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var opts = new QueryRequestOptions { MaxItemCount = 7, PartitionKey = new PartitionKey("p1") };
        var it = c.GetItemQueryIterator<Doc>("SELECT * FROM c ORDER BY c.n", requestOptions: opts);
        var first = await it.ReadNextAsync();
        Assert.Equal(7, first.Count);
        var token = first.ContinuationToken;
        Assert.False(string.IsNullOrEmpty(token));
        var rest = new List<Doc>();
        var it2 = c.GetItemQueryIterator<Doc>("SELECT * FROM c ORDER BY c.n", token, opts);
        while (it2.HasMoreResults) rest.AddRange(await it2.ReadNextAsync());
        Assert.Equal(10, first.Count + rest.Count);
        Assert.Equal(first.Concat(rest).Select(d => d.N), Enumerable.Range(0, 40).Where(i => i % 4 == 1));
    }

    [Fact]
    public async Task Paging_CrossPartitionOrderBy_ContinuationTokenResumes()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var opts = new QueryRequestOptions { MaxItemCount = 6 };
        var it = c.GetItemQueryIterator<Doc>("SELECT * FROM c ORDER BY c.n", requestOptions: opts);
        var first = await it.ReadNextAsync();
        var token = first.ContinuationToken;
        var rest = await All(c.GetItemQueryIterator<Doc>("SELECT * FROM c ORDER BY c.n", token, opts));
        Assert.Equal(Enumerable.Range(0, 40), first.Concat(rest).Select(d => d.N));
    }

    [Fact]
    public async Task Join_OverAnArray_ProducesOneRowPerElement()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var r = await All(c.GetItemQueryIterator<JObject>("SELECT c.id, t FROM c JOIN t IN c.tags WHERE c.n < 4"));
        Assert.Equal(2 + 1 + 2 + 1, r.Count);
    }

    [Fact]
    public async Task BuiltInFunctions_Contains_StartsWith_Upper_ArrayContains()
    {
        var (client, c) = await Seeded(); using var _c = client;
        Assert.Equal(13, (await All(c.GetItemQueryIterator<Doc>("SELECT * FROM c WHERE CONTAINS(c.name, '1')"))).Count);   // item-1, 10..19, 21, 31
        Assert.Equal(new[] { "ITEM-3" }, await All(c.GetItemQueryIterator<string>("SELECT VALUE UPPER(c.name) FROM c WHERE STARTSWITH(c.name, 'item-3', false) AND c.n = 3")));
        Assert.Equal(20, (await All(c.GetItemQueryIterator<Doc>("SELECT * FROM c WHERE ARRAY_CONTAINS(c.tags, 'even')"))).Count);
        Assert.Equal(20, (await All(c.GetItemQueryIterator<Doc>("SELECT * FROM c WHERE ARRAY_LENGTH(c.tags) = 1"))).Count);
    }

    [Fact]
    public async Task Subquery_AndExists()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var r = await All(c.GetItemQueryIterator<int>("SELECT VALUE c.n FROM c WHERE EXISTS(SELECT VALUE t FROM t IN c.tags WHERE t = 't2') AND c.n < 20"));
        Assert.Equal(new[] { 2, 8, 14 }, r.OrderBy(x => x).ToArray());
    }

    [Fact]
    public async Task Query_IsNull_IsDefined_TypeFunctions()
    {
        var (client, _, c) = await Setup(); using var _c = client;
        await c.CreateItemAsync(new { id = "1", pk = "p", v = (string?)null }, new PartitionKey("p"));
        await c.CreateItemAsync(new { id = "2", pk = "p", v = "x" }, new PartitionKey("p"));
        await c.CreateItemAsync(new { id = "3", pk = "p" }, new PartitionKey("p"));
        Assert.Equal(new[] { "1" }, await All(c.GetItemQueryIterator<string>("SELECT VALUE c.id FROM c WHERE IS_NULL(c.v)")));
        Assert.Equal(new[] { "1", "2" }, (await All(c.GetItemQueryIterator<string>("SELECT VALUE c.id FROM c WHERE IS_DEFINED(c.v)"))).OrderBy(x => x).ToArray());
        Assert.Equal(new[] { "2" }, await All(c.GetItemQueryIterator<string>("SELECT VALUE c.id FROM c WHERE IS_STRING(c.v)")));
    }

    [Fact]
    public async Task Query_WithSyntaxError_Fails400()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var ex = await Fails(async () => await All(c.GetItemQueryIterator<Doc>("SELEKT * FROM c")));
        Assert.Equal(HttpStatusCode.BadRequest, ex.StatusCode);
    }

    [Fact]
    public async Task Linq_Where_OrderBy_Select_Count()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var q = c.GetItemLinqQueryable<Doc>(allowSynchronousQueryExecution: false).Where(d => d.N >= 30 && d.Pk == "p2").OrderBy(d => d.N).Select(d => d.Name);
        var names = await All(q.ToFeedIterator());
        Assert.Equal(new[] { "item-30", "item-34", "item-38" }, names);
        var count = await c.GetItemLinqQueryable<Doc>().Where(d => d.N < 10).CountAsync();
        Assert.Equal(10, count.Resource);
        var sum = await c.GetItemLinqQueryable<Doc>().Select(d => d.N).SumAsync();
        Assert.Equal(780, sum.Resource);
    }

    [Fact]
    public async Task Linq_SelectMany_Skip_Take_AndToQueryDefinition()
    {
        var (client, c) = await Seeded(); using var _c = client;
        var q = c.GetItemLinqQueryable<Doc>().OrderBy(d => d.N).Skip(3).Take(4).Select(d => d.N);
        Assert.Equal(new[] { 3, 4, 5, 6 }, await All(q.ToFeedIterator()));
        var def = c.GetItemLinqQueryable<Doc>().Where(d => d.Name!.StartsWith("item-1")).ToQueryDefinition();
        Assert.Contains("STARTSWITH", def.QueryText);
        Assert.Equal(11, (await All(c.GetItemQueryIterator<Doc>(def))).Count);
    }

    [Fact]
    public async Task QueryStreamIterator_ReturnsDocumentsEnvelope()
    {
        var (client, c) = await Seeded(); using var _c = client;
        using var it = c.GetItemQueryStreamIterator("SELECT c.id FROM c WHERE c.n < 3", requestOptions: new QueryRequestOptions { PartitionKey = new PartitionKey("p0") });
        using var r = await it.ReadNextAsync();
        Assert.True(r.IsSuccessStatusCode);
        var o = JObject.Parse(await new StreamReader(r.Content).ReadToEndAsync());
        Assert.Equal(1, o["_count"]!.Value<int>());
        Assert.Equal("i0", (string)o["Documents"]![0]!["id"]!);
    }

    [Fact]
    public async Task ContainerQuery_AndDatabaseQuery_ReturnMetadata()
    {
        var (client, db, c) = await Setup(); using var _c = client;
        var found = await All(db.GetContainerQueryIterator<ContainerProperties>("SELECT * FROM c WHERE c.id = '" + c.Id + "'"));
        Assert.Equal("/pk", Assert.Single(found).PartitionKeyPath);
    }

    [Fact]
    public async Task Query_OverAContainerWithCompositeIndex_OrdersByTwoFields()
    {
        var (client, _, c) = await Setup(cfg: p => p.IndexingPolicy.CompositeIndexes.Add(new Collection<CompositePath> { new() { Path = "/n", Order = CompositePathSortOrder.Ascending }, new() { Path = "/name", Order = CompositePathSortOrder.Descending } }));
        using var _c = client;
        for (int i = 0; i < 6; i++) await c.CreateItemAsync(D("i" + i, "p" + (i % 2), i / 2, "n" + i), new PartitionKey("p" + (i % 2)));
        var r = await All(c.GetItemQueryIterator<Doc>("SELECT * FROM c ORDER BY c.n ASC, c.name DESC"));
        Assert.Equal(new[] { "i1", "i0", "i3", "i2", "i5", "i4" }, r.Select(d => d.Id));
    }
}
