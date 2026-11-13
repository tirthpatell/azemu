using System.Collections.Concurrent;
using System.Net;
using Microsoft.Azure.Cosmos;
using Newtonsoft.Json.Linq;

namespace Azemu.DotnetTests.Cosmos;

public class CosmosBatchTests : CosmosBase, IClassFixture<CosmosFixture>
{
    public CosmosBatchTests(CosmosFixture f) : base(f) { }

    [Fact]
    public async Task TransactionalBatch_CreateReplaceDeleteReadPatch_AllApplyTogether()
    {
        var (client, _, c) = await Setup(); using var _c = client;
        await c.CreateItemAsync(D("old"), new PartitionKey("p1"));
        await c.CreateItemAsync(D("gone"), new PartitionKey("p1"));
        var batch = c.CreateTransactionalBatch(new PartitionKey("p1"))
            .CreateItem(D("new1", n: 1))
            .UpsertItem(D("new2", n: 2))
            .ReplaceItem("old", D("old", n: 50))
            .DeleteItem("gone")
            .ReadItem("old")
            .PatchItem("new1", new[] { PatchOperation.Increment("/n", 10) });
        using var r = await batch.ExecuteAsync();
        Assert.True(r.IsSuccessStatusCode, r.ErrorMessage);
        Assert.Equal(6, r.Count);
        Assert.All(r, op => Assert.True(op.IsSuccessStatusCode));
        Assert.Equal(HttpStatusCode.Created, r[0].StatusCode);
        Assert.Equal(HttpStatusCode.OK, r[2].StatusCode);
        Assert.Equal(HttpStatusCode.NoContent, r[3].StatusCode);
        Assert.Equal(50, r.GetOperationResultAtIndex<Doc>(4).Resource.N);
        Assert.True(r.RequestCharge > 0);
        Assert.Equal(11, (await c.ReadItemAsync<Doc>("new1", new PartitionKey("p1"))).Resource.N);
        Assert.Equal(HttpStatusCode.NotFound, (await Fails(async () => await c.ReadItemAsync<Doc>("gone", new PartitionKey("p1")))).StatusCode);
    }

    [Fact]
    public async Task TransactionalBatch_OneFailure_RollsBackAll_AndOthersAreFailedDependency()
    {
        var (client, _, c) = await Setup(); using var _c = client;
        await c.CreateItemAsync(D("exists"), new PartitionKey("p1"));
        using var r = await c.CreateTransactionalBatch(new PartitionKey("p1"))
            .CreateItem(D("fresh"))
            .CreateItem(D("exists"))
            .CreateItem(D("fresh2"))
            .ExecuteAsync();
        Assert.False(r.IsSuccessStatusCode);
        Assert.Equal(HttpStatusCode.Conflict, r.StatusCode);
        Assert.Equal(HttpStatusCode.FailedDependency, r[0].StatusCode);
        Assert.Equal(HttpStatusCode.Conflict, r[1].StatusCode);
        Assert.Equal(HttpStatusCode.FailedDependency, r[2].StatusCode);
        Assert.Equal(HttpStatusCode.NotFound, (await Fails(async () => await c.ReadItemAsync<Doc>("fresh", new PartitionKey("p1")))).StatusCode);
    }

    [Fact]
    public async Task TransactionalBatch_IfMatchEtagMismatch_FailsWith412()
    {
        var (client, _, c) = await Setup(); using var _c = client;
        await c.CreateItemAsync(D("a"), new PartitionKey("p1"));
        using var r = await c.CreateTransactionalBatch(new PartitionKey("p1"))
            .ReplaceItem("a", D("a", n: 2), new TransactionalBatchItemRequestOptions { IfMatchEtag = "\"00000000-0000-0000-0000-000000000000\"" })
            .ExecuteAsync();
        Assert.False(r.IsSuccessStatusCode);
        Assert.Equal(HttpStatusCode.PreconditionFailed, r[0].StatusCode);
    }

    [Fact]
    public async Task TransactionalBatch_WithMoreThan100Operations_IsRejected()
    {
        var (client, _, c) = await Setup(); using var _c = client;
        var b = c.CreateTransactionalBatch(new PartitionKey("p1"));
        for (int i = 0; i < 101; i++) b.CreateItem(D("i" + i));
        using var r = await b.ExecuteAsync();
        Assert.False(r.IsSuccessStatusCode);
        Assert.Equal(HttpStatusCode.BadRequest, r.StatusCode);
    }

    [Fact]
    public async Task TransactionalBatch_Stream_OperationsWork()
    {
        var (client, _, c) = await Setup(); using var _c = client;
        var json = "{\"id\":\"s1\",\"pk\":\"p1\",\"n\":3}";
        using var r = await c.CreateTransactionalBatch(new PartitionKey("p1"))
            .CreateItemStream(new MemoryStream(System.Text.Encoding.UTF8.GetBytes(json)))
            .ReadItem("s1").ExecuteAsync();
        Assert.True(r.IsSuccessStatusCode);
        using var read = r[1].ResourceStream;
        Assert.Contains("\"n\":3", await new StreamReader(read).ReadToEndAsync());
    }

    [Fact]
    public async Task BulkExecution_HundredCreatesInParallel_AllSucceed()
    {
        var (client, _, c) = await Setup(bulk: true, throughput: 1000); using var _c = client;
        var tasks = Enumerable.Range(0, 100).Select(i => c.CreateItemAsync(D("b" + i, "p" + (i % 5), i), new PartitionKey("p" + (i % 5)))).ToList();
        var results = await Task.WhenAll(tasks);
        Assert.All(results, r => Assert.Equal(HttpStatusCode.Created, r.StatusCode));
        Assert.Equal(100, (await All(c.GetItemQueryIterator<int>("SELECT VALUE COUNT(1) FROM c"))).Single());
    }

    [Fact]
    public async Task BulkExecution_OneConflict_FailsOnlyThatOperation()
    {
        var (client, _, c) = await Setup(bulk: true); using var _c = client;
        await c.CreateItemAsync(D("dup"), new PartitionKey("p1"));
        var tasks = new List<Task<ItemResponse<Doc>>> { c.CreateItemAsync(D("ok1"), new PartitionKey("p1")), c.CreateItemAsync(D("dup"), new PartitionKey("p1")), c.CreateItemAsync(D("ok2"), new PartitionKey("p1")) };
        var outcomes = new List<string>();
        foreach (var t in tasks)
        {
            try { outcomes.Add((await t).StatusCode.ToString()); }
            catch (CosmosException e) { outcomes.Add(e.StatusCode.ToString()); }
        }
        Assert.Equal(new[] { "Created", "Conflict", "Created" }, outcomes);
    }

    [Fact]
    public async Task BulkExecution_UpsertReadReplaceDelete_Work()
    {
        var (client, _, c) = await Setup(bulk: true); using var _c = client;
        await Task.WhenAll(Enumerable.Range(0, 20).Select(i => c.UpsertItemAsync(D("u" + i, "p1", i), new PartitionKey("p1"))));
        var reads = await Task.WhenAll(Enumerable.Range(0, 20).Select(i => c.ReadItemAsync<Doc>("u" + i, new PartitionKey("p1"))));
        Assert.Equal(Enumerable.Range(0, 20), reads.Select(r => r.Resource.N));
        await Task.WhenAll(Enumerable.Range(0, 20).Select(i => c.ReplaceItemAsync(D("u" + i, "p1", i + 100), "u" + i, new PartitionKey("p1"))));
        await Task.WhenAll(Enumerable.Range(0, 20).Select(i => c.DeleteItemAsync<Doc>("u" + i, new PartitionKey("p1"))));
        Assert.Equal(0, (await All(c.GetItemQueryIterator<int>("SELECT VALUE COUNT(1) FROM c"))).Single());
    }
}

public class CosmosChangeFeedTests : CosmosBase, IClassFixture<CosmosFixture>
{
    public CosmosChangeFeedTests(CosmosFixture f) : base(f) { }

    static async Task<List<T>> Drain<T>(FeedIterator<T> it)
    {
        var l = new List<T>();
        while (it.HasMoreResults)
        {
            var page = await it.ReadNextAsync();
            if (page.StatusCode == HttpStatusCode.NotModified) break;
            l.AddRange(page);
        }
        return l;
    }

    [Fact]
    public async Task PullModel_FromBeginning_ReturnsEveryCreateAndReplaceOnce()
    {
        var (client, _, c) = await Setup(workAroundCo3: true); using var _c = client;
        for (int i = 0; i < 5; i++) await c.CreateItemAsync(D("a" + i, "p" + (i % 2), i), new PartitionKey("p" + (i % 2)));
        await c.ReplaceItemAsync(D("a0", "p0", 100), "a0", new PartitionKey("p0"));
        var docs = await Drain(c.GetChangeFeedIterator<Doc>(ChangeFeedStartFrom.Beginning(), ChangeFeedMode.Incremental));
        Assert.Equal(5, docs.Count);
        Assert.Equal(100, docs.Single(d => d.Id == "a0").N);
    }

    [Fact]
    public async Task PullModel_ReadingPastTheLastChange_Returns304NotModified()
    {
        var (client, _, c) = await Setup(); using var _c = client;   // no workaround here
        await c.CreateItemAsync(D("a"), new PartitionKey("p1"));
        var it = c.GetChangeFeedIterator<Doc>(ChangeFeedStartFrom.Beginning(), ChangeFeedMode.Incremental);
        Assert.Single(await it.ReadNextAsync());
        var page = await it.ReadNextAsync();
        Assert.Equal(HttpStatusCode.NotModified, page.StatusCode);
    }

    [Fact]
    public async Task PullModel_ContinuationToken_ReturnsOnlyNewChanges()
    {
        var (client, _, c) = await Setup(workAroundCo3: true); using var _c = client;
        await c.CreateItemAsync(D("first"), new PartitionKey("p1"));
        var it = c.GetChangeFeedIterator<Doc>(ChangeFeedStartFrom.Beginning(), ChangeFeedMode.Incremental);
        string? token = null;
        var seen = new List<string>();
        while (it.HasMoreResults)
        {
            var page = await it.ReadNextAsync();
            token = page.ContinuationToken;
            if (page.StatusCode == HttpStatusCode.NotModified) break;
            seen.AddRange(page.Select(d => d.Id));
        }
        Assert.Equal(new[] { "first" }, seen);
        await c.CreateItemAsync(D("second"), new PartitionKey("p1"));
        var it2 = c.GetChangeFeedIterator<Doc>(ChangeFeedStartFrom.ContinuationToken(token!), ChangeFeedMode.Incremental);
        Assert.Equal(new[] { "second" }, (await Drain(it2)).Select(d => d.Id));
    }

    [Fact]
    public async Task PullModel_StartFromNow_SkipsExistingItems()
    {
        var (client, _, c) = await Setup(workAroundCo3: true); using var _c = client;
        await c.CreateItemAsync(D("before"), new PartitionKey("p1"));
        var it = c.GetChangeFeedIterator<Doc>(ChangeFeedStartFrom.Now(), ChangeFeedMode.Incremental);
        Assert.Empty(await Drain(it));
        await c.CreateItemAsync(D("after"), new PartitionKey("p1"));
        Assert.Equal(new[] { "after" }, (await Drain(it)).Select(d => d.Id));
    }

    [Fact]
    public async Task PullModel_ForOnePartitionKey_ReturnsOnlyItsItems()
    {
        var (client, _, c) = await Setup(workAroundCo3: true); using var _c = client;
        for (int i = 0; i < 6; i++) await c.CreateItemAsync(D("a" + i, "p" + (i % 3), i), new PartitionKey("p" + (i % 3)));
        var docs = await Drain(c.GetChangeFeedIterator<Doc>(ChangeFeedStartFrom.Beginning(FeedRange.FromPartitionKey(new PartitionKey("p1"))), ChangeFeedMode.Incremental));
        Assert.Equal(new[] { "a1", "a4" }, docs.Select(d => d.Id).OrderBy(x => x).ToArray());
    }

    [Fact]
    public async Task PullModel_FeedRanges_CoverTheContainer()
    {
        var (client, _, c) = await Setup(workAroundCo3: true); using var _c = client;
        for (int i = 0; i < 4; i++) await c.CreateItemAsync(D("a" + i, "p" + i, i), new PartitionKey("p" + i));
        var ranges = await c.GetFeedRangesAsync();
        Assert.NotEmpty(ranges);
        var total = 0;
        foreach (var r in ranges) total += (await Drain(c.GetChangeFeedIterator<Doc>(ChangeFeedStartFrom.Beginning(r), ChangeFeedMode.Incremental))).Count;
        Assert.Equal(4, total);
    }

    [Fact]
    public async Task PullModel_StartFromTime_SkipsOlderChanges()
    {
        var (client, _, c) = await Setup(workAroundCo3: true); using var _c = client;
        await c.CreateItemAsync(D("old"), new PartitionKey("p1"));
        await Task.Delay(1500);
        var mark = DateTime.UtcNow;
        await Task.Delay(500);
        await c.CreateItemAsync(D("recent"), new PartitionKey("p1"));
        var docs = await Drain(c.GetChangeFeedIterator<Doc>(ChangeFeedStartFrom.Time(mark), ChangeFeedMode.Incremental));
        Assert.Equal(new[] { "recent" }, docs.Select(d => d.Id));
    }

    [Fact]
    public async Task PullModel_DeletesAreNotInTheIncrementalFeed()
    {
        var (client, _, c) = await Setup(workAroundCo3: true); using var _c = client;
        await c.CreateItemAsync(D("a"), new PartitionKey("p1"));
        await c.DeleteItemAsync<Doc>("a", new PartitionKey("p1"));
        Assert.Empty(await Drain(c.GetChangeFeedIterator<Doc>(ChangeFeedStartFrom.Beginning(), ChangeFeedMode.Incremental)));
    }

    [Fact]
    public async Task AllVersionsAndDeletes_ShowsCreateReplaceAndDelete()
    {
        var (client, _, c) = await Setup(workAroundCo3: true); using var _c = client;
        var it = c.GetChangeFeedIterator<ChangeFeedItem<Doc>>(ChangeFeedStartFrom.Now(), ChangeFeedMode.AllVersionsAndDeletes);
        await Drain(it);
        await c.CreateItemAsync(D("a", n: 1), new PartitionKey("p1"));
        await c.ReplaceItemAsync(D("a", n: 2), "a", new PartitionKey("p1"));
        await c.DeleteItemAsync<Doc>("a", new PartitionKey("p1"));
        var items = await Drain(it);
        Assert.Equal(new[] { ChangeFeedOperationType.Create, ChangeFeedOperationType.Replace, ChangeFeedOperationType.Delete }, items.Select(i => i.Metadata.OperationType).ToArray());
        Assert.Equal("a", items[2].Previous?.Id ?? items[2].Metadata.Id);
    }

    [Fact]
    public async Task Processor_WithALeaseContainer_DeliversExistingAndNewItems()
    {
        var (client, db, c) = await Setup(workAroundCo3: true); using var _c = client;
        for (int i = 0; i < 5; i++) await c.CreateItemAsync(D("a" + i, "p" + (i % 2), i), new PartitionKey("p" + (i % 2)));
        var leases = (await db.CreateContainerAsync(new ContainerProperties("leases", "/id"))).Container;
        var seen = new ConcurrentBag<string>();
        var processor = c.GetChangeFeedProcessorBuilder<Doc>("proc1", (ChangeFeedProcessorContext ctx, IReadOnlyCollection<Doc> changes, CancellationToken ct) =>
            { foreach (var d in changes) seen.Add(d.Id); return Task.CompletedTask; })
            .WithInstanceName("inst1").WithLeaseContainer(leases).WithPollInterval(TimeSpan.FromMilliseconds(300)).WithStartTime(DateTime.MinValue.ToUniversalTime()).Build();
        await processor.StartAsync();
        Assert.True(await Wait.Until(() => seen.Distinct().Count() == 5, 30));
        await c.CreateItemAsync(D("late", "p1", 9), new PartitionKey("p1"));
        Assert.True(await Wait.Until(() => seen.Contains("late"), 30));
        await processor.StopAsync();
        var leaseDocs = await All(leases.GetItemQueryIterator<JObject>("SELECT * FROM c"));
        Assert.NotEmpty(leaseDocs);
    }

    [Fact]
    public async Task Processor_ErrorInTheHandler_IsReportedToTheErrorDelegate()
    {
        var (client, db, c) = await Setup(workAroundCo3: true); using var _c = client;
        await c.CreateItemAsync(D("a"), new PartitionKey("p1"));
        var leases = (await db.CreateContainerAsync(new ContainerProperties("leases", "/id"))).Container;
        var errors = new ConcurrentBag<string>();
        var processor = c.GetChangeFeedProcessorBuilder<Doc>("proc2", (ChangeFeedProcessorContext ctx, IReadOnlyCollection<Doc> changes, CancellationToken ct) => throw new InvalidOperationException("boom"))
            .WithErrorNotification((lease, ex) => { errors.Add(ex.InnerException?.Message ?? ex.Message); return Task.CompletedTask; })
            .WithInstanceName("i").WithLeaseContainer(leases).WithPollInterval(TimeSpan.FromMilliseconds(300)).WithStartTime(DateTime.MinValue.ToUniversalTime()).Build();
        await processor.StartAsync();
        Assert.True(await Wait.Until(() => !errors.IsEmpty, 30));
        await processor.StopAsync();
        Assert.Contains("boom", errors.First());
    }
}
