using Azure;
using Azure.Data.Tables;
using Azure.Data.Tables.Models;
using Azure.Data.Tables.Sas;
using Azure.Storage.Sas;

namespace Azemu.DotnetTests.Storage;

public class Customer : ITableEntity
{
    public string PartitionKey { get; set; } = "";
    public string RowKey { get; set; } = "";
    public DateTimeOffset? Timestamp { get; set; }
    public ETag ETag { get; set; }
    public string? Name { get; set; }
    public int Age { get; set; }
    public long Big { get; set; }
    public double Score { get; set; }
    public bool Active { get; set; }
    public Guid Id { get; set; }
    public DateTimeOffset Joined { get; set; }
    public byte[]? Blob { get; set; }
}

public class TableTests : StorageBase, IClassFixture<StorageFixture>
{
    public TableTests(StorageFixture f) : base(f) { }

    TableServiceClient Svc() => new(TableUri, new TableSharedKeyCredential(Account, AnyKey), Opts(new TableClientOptions()));
    async Task<TableClient> NewTable()
    {
        var t = Svc().GetTableClient("t" + Names.Unique("x").Substring(0, 16));
        await t.CreateAsync();
        return t;
    }

    static Customer C(string pk, string rk, int age = 1, string? name = null) => new() { PartitionKey = pk, RowKey = rk, Age = age, Name = name ?? "n" + rk, Big = 1L << 40, Score = 2.5, Active = true, Id = Guid.NewGuid(), Joined = new DateTimeOffset(2024, 1, 2, 3, 4, 5, TimeSpan.Zero), Blob = new byte[] { 1, 2, 3 } };

    [Fact]
    public async Task CreateTable_Twice_Fails409TableAlreadyExists()
    {
        var t = await NewTable();
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await t.CreateAsync());
        Assert.Equal(409, ex.Status);
        Assert.Equal("TableAlreadyExists", ex.ErrorCode);
    }

    [Fact]
    public async Task CreateTableIfNotExists_SecondCallSeesTheConflictAnswer()
    {
        var t = Svc().GetTableClient("t" + Names.Unique("x").Substring(0, 12));
        Assert.InRange((await t.CreateIfNotExistsAsync()).GetRawResponse().Status, 200, 204);
        // the Tables SDK returns the TableItem both times; the second answer is the 409
        Assert.Equal(409, (await t.CreateIfNotExistsAsync()).GetRawResponse().Status);
    }

    [Fact]
    public async Task InvalidTableName_Fails400()
    {
        var ex = await Assert.ThrowsAnyAsync<Exception>(async () => await Svc().GetTableClient("1-bad-name!").CreateAsync());
        Assert.True(ex is RequestFailedException { Status: 400 } or ArgumentException, ex.ToString());
    }

    [Fact]
    public async Task QueryTables_ListsCreatedTables_WithFilter()
    {
        var svc = Svc();
        var t = await NewTable();
        var found = new List<string>();
        await foreach (var ti in svc.QueryAsync(x => x.Name == t.Name)) found.Add(ti.Name);
        Assert.Equal(new[] { t.Name }, found);
        await svc.DeleteTableAsync(t.Name);
        Assert.Empty(await Collect(svc.QueryAsync(x => x.Name == t.Name)));
    }

    static async Task<List<T>> Collect<T>(AsyncPageable<T> p) where T : notnull { var l = new List<T>(); await foreach (var i in p) l.Add(i); return l; }

    [Fact]
    public async Task AddEntity_AllEdmTypes_RoundTrip()
    {
        var t = await NewTable();
        var c = C("p", "r1");
        var r = await t.AddEntityAsync(c);
        Assert.False(string.IsNullOrEmpty(r.Headers.ETag?.ToString()));
        var got = (await t.GetEntityAsync<Customer>("p", "r1")).Value;
        Assert.Equal(c.Name, got.Name); Assert.Equal(c.Age, got.Age); Assert.Equal(c.Big, got.Big); Assert.Equal(c.Score, got.Score);
        Assert.True(got.Active); Assert.Equal(c.Id, got.Id); Assert.Equal(c.Joined, got.Joined); Assert.Equal(c.Blob, got.Blob);
        Assert.NotNull(got.Timestamp);
        Assert.False(got.ETag == default);
    }

    [Fact]
    public async Task TableEntity_DynamicProperties_KeepTheirClrTypes()
    {
        var t = await NewTable();
        var e = new TableEntity("p", "dyn") { ["s"] = "text", ["i"] = 5, ["l"] = 5L, ["d"] = 1.5, ["b"] = true, ["g"] = Guid.NewGuid(), ["dt"] = new DateTime(2020, 1, 1, 0, 0, 0, DateTimeKind.Utc), ["dto"] = DateTimeOffset.UnixEpoch, ["bin"] = new byte[] { 9 }, ["nul"] = null };
        await t.AddEntityAsync(e);
        var g = (await t.GetEntityAsync<TableEntity>("p", "dyn")).Value;
        Assert.IsType<string>(g["s"]); Assert.IsType<int>(g["i"]); Assert.IsType<long>(g["l"]); Assert.IsType<double>(g["d"]);
        Assert.IsType<bool>(g["b"]); Assert.IsType<Guid>(g["g"]); Assert.IsType<DateTimeOffset>(g["dt"]); Assert.IsType<byte[]>(g["bin"]);
        Assert.Equal(1.5, g.GetDouble("d")); Assert.Equal(5L, g.GetInt64("l"));
        Assert.False(g.ContainsKey("nul") && g["nul"] != null);
    }

    [Fact]
    public async Task AddEntity_Twice_Fails409EntityAlreadyExists()
    {
        var t = await NewTable();
        await t.AddEntityAsync(C("p", "r"));
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await t.AddEntityAsync(C("p", "r")));
        Assert.Equal(409, ex.Status);
        Assert.Equal("EntityAlreadyExists", ex.ErrorCode);
    }

    [Fact]
    public async Task GetEntity_Missing_Fails404ResourceNotFound()
    {
        var t = await NewTable();
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await t.GetEntityAsync<TableEntity>("p", "nope"));
        Assert.Equal(404, ex.Status);
        Assert.Equal("ResourceNotFound", ex.ErrorCode);
        Assert.False((await t.GetEntityIfExistsAsync<TableEntity>("p", "nope")).HasValue);
    }

    [Fact]
    public async Task KeysWithSpecialCharacters_RoundTrip()
    {
        var t = await NewTable();
        foreach (var (pk, rk) in new[] { ("part with space", "row 1"), ("p'quote", "r'q"), ("ünï", "日本"), ("a+b", "c=d&e"), ("pct%20", "x%2Fy") })
        {
            await t.AddEntityAsync(new TableEntity(pk, rk) { ["v"] = 1 });
            var g = (await t.GetEntityAsync<TableEntity>(pk, rk)).Value;
            Assert.Equal(pk, g.PartitionKey); Assert.Equal(rk, g.RowKey);
            await t.DeleteEntityAsync(pk, rk);
        }
    }

    [Fact]
    public async Task UpdateEntity_Merge_KeepsOtherProperties_Replace_DropsThem()
    {
        var t = await NewTable();
        await t.AddEntityAsync(new TableEntity("p", "r") { ["a"] = 1, ["b"] = 2 });
        await t.UpdateEntityAsync(new TableEntity("p", "r") { ["b"] = 20, ["c"] = 30 }, ETag.All, TableUpdateMode.Merge);
        var m = (await t.GetEntityAsync<TableEntity>("p", "r")).Value;
        Assert.Equal(1, m.GetInt32("a")); Assert.Equal(20, m.GetInt32("b")); Assert.Equal(30, m.GetInt32("c"));
        await t.UpdateEntityAsync(new TableEntity("p", "r") { ["z"] = 9 }, ETag.All, TableUpdateMode.Replace);
        var r = (await t.GetEntityAsync<TableEntity>("p", "r")).Value;
        Assert.False(r.ContainsKey("a")); Assert.Equal(9, r.GetInt32("z"));
    }

    [Fact]
    public async Task UpsertEntity_InsertsThenReplaces()
    {
        var t = await NewTable();
        await t.UpsertEntityAsync(new TableEntity("p", "r") { ["a"] = 1 });
        await t.UpsertEntityAsync(new TableEntity("p", "r") { ["b"] = 2 }, TableUpdateMode.Replace);
        var g = (await t.GetEntityAsync<TableEntity>("p", "r")).Value;
        Assert.False(g.ContainsKey("a")); Assert.Equal(2, g.GetInt32("b"));
    }

    [Fact]
    public async Task UpdateEntity_WithAStaleEtag_Fails412UpdateConditionNotSatisfied()
    {
        var t = await NewTable();
        var first = (await t.AddEntityAsync(new TableEntity("p", "r") { ["a"] = 1 })).Headers.ETag!.Value;
        await t.UpdateEntityAsync(new TableEntity("p", "r") { ["a"] = 2 }, ETag.All);
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await t.UpdateEntityAsync(new TableEntity("p", "r") { ["a"] = 3 }, first));
        Assert.Equal(412, ex.Status);
        Assert.Equal("UpdateConditionNotSatisfied", ex.ErrorCode);
    }

    [Fact]
    public async Task DeleteEntity_RemovesIt_AndDeletingAgainReturnsThe404WithoutThrowing()
    {
        var t = await NewTable();
        await t.AddEntityAsync(new TableEntity("p", "r"));
        await t.DeleteEntityAsync("p", "r");
        Assert.False((await t.GetEntityIfExistsAsync<TableEntity>("p", "r")).HasValue);
        // the Tables SDK does not throw when the entity is already gone: it returns the 404 answer
        Assert.Equal(404, (await t.DeleteEntityAsync("p", "r", ETag.All)).Status);
    }

    async Task<TableClient> Seed()
    {
        var t = await NewTable();
        for (int i = 0; i < 30; i++) await t.AddEntityAsync(C("p" + (i % 3), "r" + i.ToString("D2"), i, "name" + i));
        return t;
    }

    [Fact]
    public async Task Query_WithODataFilter_AndLinqExpression_ReturnTheSameRows()
    {
        var t = await Seed();
        var odata = await Collect(t.QueryAsync<Customer>("PartitionKey eq 'p1' and Age ge 10 and Age lt 20"));
        var linq = await Collect(t.QueryAsync<Customer>(c => c.PartitionKey == "p1" && c.Age >= 10 && c.Age < 20));
        Assert.Equal(new[] { 10, 13, 16, 19 }, odata.Select(c => c.Age).OrderBy(x => x).ToArray());
        Assert.Equal(odata.Select(c => c.RowKey).OrderBy(x => x), linq.Select(c => c.RowKey).OrderBy(x => x));
    }

    [Fact]
    public async Task Query_Filters_OnStringsBoolsGuidsDatesAndNot()
    {
        var t = await Seed();
        var g = (await Collect(t.QueryAsync<Customer>("RowKey eq 'r05'"))).Single().Id;
        Assert.Single(await Collect(t.QueryAsync<Customer>($"Id eq guid'{g}'")));
        Assert.Equal(30, (await Collect(t.QueryAsync<Customer>("Active eq true and Joined eq datetime'2024-01-02T03:04:05Z'"))).Count);
        Assert.Equal(29, (await Collect(t.QueryAsync<Customer>("not (RowKey eq 'r05')"))).Count);
        Assert.Equal(1, (await Collect(t.QueryAsync<Customer>("Big eq 1099511627776L and Age eq 7"))).Count);
        Assert.Equal(30, (await Collect(t.QueryAsync<Customer>("Score gt 2.4 and Score lt 2.6"))).Count);
    }

    [Fact]
    public async Task Query_WithSelect_ReturnsOnlyTheRequestedProperties()
    {
        var t = await Seed();
        var rows = await Collect(t.QueryAsync<TableEntity>(filter: "PartitionKey eq 'p0'", select: new[] { "Name" }));
        Assert.Equal(10, rows.Count);
        Assert.All(rows, r => { Assert.True(r.ContainsKey("Name")); Assert.False(r.ContainsKey("Age")); });
    }

    [Fact]
    public async Task Query_Paging_MaxPerPage_ReturnsContinuationPages()
    {
        var t = await Seed();
        var pages = 0; var total = 0;
        await foreach (var page in t.QueryAsync<TableEntity>(maxPerPage: 7).AsPages()) { pages++; total += page.Values.Count; Assert.True(page.Values.Count <= 7); }
        Assert.Equal(30, total);
        Assert.Equal(5, pages);
    }

    [Fact]
    public async Task Transaction_AddUpdateDelete_AllApplyAtomically()
    {
        var t = await NewTable();
        await t.AddEntityAsync(new TableEntity("p", "upd") { ["v"] = 1 });
        await t.AddEntityAsync(new TableEntity("p", "del") { ["v"] = 1 });
        var actions = new List<TableTransactionAction>
        {
            new(TableTransactionActionType.Add, new TableEntity("p", "new") { ["v"] = 1 }),
            new(TableTransactionActionType.UpdateMerge, new TableEntity("p", "upd") { ["w"] = 2 }, ETag.All),
            new(TableTransactionActionType.Delete, new TableEntity("p", "del"), ETag.All),
            new(TableTransactionActionType.UpsertReplace, new TableEntity("p", "ups") { ["v"] = 5 }),
        };
        var r = await t.SubmitTransactionAsync(actions);
        Assert.Equal(4, r.Value.Count);
        Assert.All(r.Value, x => Assert.InRange(x.Status, 200, 204));
        Assert.Equal(2, (await t.GetEntityAsync<TableEntity>("p", "upd")).Value.GetInt32("w"));
        Assert.False((await t.GetEntityIfExistsAsync<TableEntity>("p", "del")).HasValue);
    }

    [Fact]
    public async Task Transaction_OneFailure_RollsBackAll_AndReportsTheFailedIndex()
    {
        var t = await NewTable();
        await t.AddEntityAsync(new TableEntity("p", "exists"));
        var ex = await Assert.ThrowsAsync<TableTransactionFailedException>(async () => await t.SubmitTransactionAsync(new[]
        {
            new TableTransactionAction(TableTransactionActionType.Add, new TableEntity("p", "fresh")),
            new TableTransactionAction(TableTransactionActionType.Add, new TableEntity("p", "exists")),
        }));
        Assert.Equal(1, ex.FailedTransactionActionIndex);
        Assert.Equal(409, ex.Status);
        Assert.False((await t.GetEntityIfExistsAsync<TableEntity>("p", "fresh")).HasValue);
    }

    [Fact]
    public async Task Transaction_AcrossPartitions_IsRefused400()
    {
        var t = await NewTable();
        var ex = await Assert.ThrowsAnyAsync<Exception>(async () => await t.SubmitTransactionAsync(new[]
        {
            new TableTransactionAction(TableTransactionActionType.Add, new TableEntity("p1", "a")),
            new TableTransactionAction(TableTransactionActionType.Add, new TableEntity("p2", "b")),
        }));
        Assert.True(ex is TableTransactionFailedException { Status: 400 } or RequestFailedException { Status: 400 } or ArgumentException, ex.ToString());
    }

    [Fact]
    public async Task Transaction_Of100Entities_Works_And101IsRefused()
    {
        var t = await NewTable();
        var ok = Enumerable.Range(0, 100).Select(i => new TableTransactionAction(TableTransactionActionType.Add, new TableEntity("p", "a" + i)));
        Assert.Equal(100, (await t.SubmitTransactionAsync(ok)).Value.Count);
        var tooMany = Enumerable.Range(0, 101).Select(i => new TableTransactionAction(TableTransactionActionType.Add, new TableEntity("p", "b" + i)));
        await Assert.ThrowsAnyAsync<Exception>(async () => await t.SubmitTransactionAsync(tooMany));
    }

    [Fact]
    public async Task LargeProperty_Of60KbString_RoundTrips_AndOver64KbIsRefused()
    {
        var t = await NewTable();
        await t.AddEntityAsync(new TableEntity("p", "big") { ["s"] = new string('x', 30_000) });
        Assert.Equal(30_000, ((string)(await t.GetEntityAsync<TableEntity>("p", "big")).Value["s"]).Length);
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await t.AddEntityAsync(new TableEntity("p", "huge") { ["s"] = new string('x', 40_000) }));
        Assert.Equal(400, ex.Status);
    }

    [Fact]
    public async Task ServiceSas_ReadOnly_AllowsQueryButNotAdd()
    {
        var t = await NewTable();
        await t.AddEntityAsync(new TableEntity("p", "r") { ["v"] = 1 });
        var sas = t.GenerateSasUri(TableSasPermissions.Read, DateTimeOffset.UtcNow.AddMinutes(10));
        var second = new TableClient(sas, Opts(new TableClientOptions()));
        Assert.Single(await Collect(second.QueryAsync<TableEntity>()));
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await second.AddEntityAsync(new TableEntity("p", "x")));
        Assert.Equal(403, ex.Status);
    }

    [Fact]
    public async Task SasWithPartitionKeyRange_RestrictsAccess()
    {
        var t = await NewTable();
        await t.AddEntityAsync(new TableEntity("allowed", "r"));
        await t.AddEntityAsync(new TableEntity("other", "r"));
        var sas = t.GenerateSasUri(new TableSasBuilder(t.Name, TableSasPermissions.Read, DateTimeOffset.UtcNow.AddMinutes(10)) { PartitionKeyStart = "allowed", PartitionKeyEnd = "allowed" });
        var second = new TableClient(sas, Opts(new TableClientOptions()));
        Assert.Equal("allowed", (await second.GetEntityAsync<TableEntity>("allowed", "r")).Value.PartitionKey);
        await Assert.ThrowsAsync<RequestFailedException>(async () => await second.GetEntityAsync<TableEntity>("other", "r"));
    }

    [Fact]
    public async Task AccountSas_ForTables_Works()
    {
        var svc = Svc();
        var sas = svc.GenerateSasUri(TableAccountSasPermissions.All, TableAccountSasResourceTypes.All, DateTimeOffset.UtcNow.AddMinutes(10));
        var second = new TableServiceClient(sas, Opts(new TableClientOptions()));
        var t = second.GetTableClient("t" + Names.Unique("x").Substring(0, 12));
        await t.CreateAsync();
        await t.AddEntityAsync(new TableEntity("p", "r"));
        Assert.Single(await Collect(t.QueryAsync<TableEntity>()));
    }

    [Fact]
    public async Task AccessPolicy_SetAndGet()
    {
        var t = await NewTable();
        await t.SetAccessPolicyAsync(new[] { new TableSignedIdentifier("pol", new TableAccessPolicy(DateTimeOffset.UtcNow.AddMinutes(-1), DateTimeOffset.UtcNow.AddHours(1), "r")) });
        var p = (await t.GetAccessPoliciesAsync()).Value;
        Assert.Equal("pol", Assert.Single(p).Id);
    }

    [Fact]
    public async Task DevAccount_SharedKey_RightWorks_WrongRefused()
    {
        var uri = new Uri($"https://devstoreaccount1.table.core.localhost:{Emu.Port}");
        var good = new TableServiceClient(uri, new TableSharedKeyCredential("devstoreaccount1", DevKey), Opts(new TableClientOptions()));
        var name = "t" + Names.Unique("x").Substring(0, 12);
        await good.GetTableClient(name).CreateAsync();
        var bad = new TableServiceClient(uri, new TableSharedKeyCredential("devstoreaccount1", AnyKey), Opts(new TableClientOptions()));
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await bad.GetTableClient(name).AddEntityAsync(new TableEntity("p", "r")));
        Assert.Equal(403, ex.Status);
    }
}
