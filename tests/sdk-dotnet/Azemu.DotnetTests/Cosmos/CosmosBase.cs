using System.Net;
using Microsoft.Azure.Cosmos;
using Newtonsoft.Json;

namespace Azemu.DotnetTests.Cosmos;

public sealed class CosmosFixture : EmuFixture
{
    public CosmosFixture() : base("cosmos") { }
}

public class Doc
{
    [JsonProperty("id")] public string Id { get; set; } = "";
    [JsonProperty("pk")] public string Pk { get; set; } = "";
    [JsonProperty("n")] public int N { get; set; }
    [JsonProperty("name")] public string? Name { get; set; }
    [JsonProperty("tags")] public List<string>? Tags { get; set; }
    [JsonProperty("ttl", NullValueHandling = NullValueHandling.Ignore)] public int? Ttl { get; set; }
    [JsonProperty("nested", NullValueHandling = NullValueHandling.Ignore)] public Nested? Nested { get; set; }
}

public class Nested
{
    [JsonProperty("a")] public int A { get; set; }
    [JsonProperty("b")] public string? B { get; set; }
}

public abstract class CosmosBase
{
    public const string Key = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";
    protected readonly Emu Emu;
    protected readonly string Account;
    protected CosmosBase(EmuFixture f) { Emu = f.Emu; Account = Names.Unique("cos"); }

    /// <summary>Adds "x-ms-item-count: 0" to a 304 answer that has none (finding CO3: the SDK does int.Parse on it).</summary>
    sealed class ItemCountHandler : DelegatingHandler
    {
        public ItemCountHandler(HttpMessageHandler inner) : base(inner) { }
        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var r = await base.SendAsync(request, ct);
            if (r.StatusCode == HttpStatusCode.NotModified && !r.Headers.Contains("x-ms-item-count")) r.Headers.TryAddWithoutValidation("x-ms-item-count", "0");
            return r;
        }
    }

    protected CosmosClient Client(ConnectionMode mode = ConnectionMode.Gateway, bool bulk = false, Action<CosmosClientOptions>? cfg = null, bool workAroundCo3 = false)
    {
        var o = new CosmosClientOptions
        {
            ConnectionMode = mode,
            AllowBulkExecution = bulk,
            HttpClientFactory = () => workAroundCo3 ? new HttpClient(new ItemCountHandler(Emu.Handler())) : Emu.NewHttpClient(),
            LimitToEndpoint = true,
            RequestTimeout = TimeSpan.FromSeconds(30),
            MaxRetryAttemptsOnRateLimitedRequests = 0,
        };
        cfg?.Invoke(o);
        return new CosmosClient(Emu.CosmosUri(Account).ToString(), Key, o);
    }

    protected async Task<(CosmosClient client, Database db, Container c)> Setup(string pkPath = "/pk", Action<ContainerProperties>? cfg = null, bool bulk = false, int? throughput = null, bool workAroundCo3 = false)
    {
        var client = Client(bulk: bulk, workAroundCo3: workAroundCo3);
        var db = (await client.CreateDatabaseAsync(Names.Unique("db"))).Database;
        var props = new ContainerProperties(Names.Unique("c"), pkPath);
        cfg?.Invoke(props);
        var c = (await db.CreateContainerAsync(props, throughput)).Container;
        return (client, db, c);
    }

    protected static Doc D(string id, string pk = "p1", int n = 0, string? name = null) => new() { Id = id, Pk = pk, N = n, Name = name ?? ("name-" + id) };

    protected static async Task<List<T>> All<T>(FeedIterator<T> it)
    {
        var l = new List<T>();
        while (it.HasMoreResults) l.AddRange(await it.ReadNextAsync());
        return l;
    }

    protected static async Task<CosmosException> Fails(Func<Task> f)
    {
        var ex = await Assert.ThrowsAsync<CosmosException>(f);
        return ex;
    }
}
