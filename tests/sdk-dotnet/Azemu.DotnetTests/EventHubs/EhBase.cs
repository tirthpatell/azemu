using System.Net.Http.Json;
using Azure.Messaging.EventHubs;
using Azure.Messaging.EventHubs.Consumer;
using Azure.Messaging.EventHubs.Primitives;
using Azure.Messaging.EventHubs.Producer;

namespace Azemu.DotnetTests.EventHubs;

public sealed class EhFixture : EmuFixture
{
    public EhFixture() : base("eventhubs") { }
}

public sealed class EhNoAutoFixture : EmuFixture
{
    public EhNoAutoFixture() : base("eventhubs-noauto", new() { ["AZEMU_EVENTHUBS_AUTOCREATE"] = "0" }) { }
}

public abstract class EhBase
{
    protected readonly Emu Emu;
    protected readonly string Ns;
    protected readonly string Hub;
    protected EhBase(EmuFixture f)
    {
        Emu = f.Emu;
        Ns = Names.Unique("ns");
        Hub = Names.Unique("hub");
    }

    protected string Conn(string? hub = null) => Emu.EhConn(Ns, hub ?? Hub);

    /// <summary>
    /// The emulator does not create an event hub because a client names it (AZEMU_EVENTHUBS_AUTOCREATE is off by default, as on
    /// Azure). A test that does not call <see cref="CreateHub"/> itself gets this class's hub (4 partitions, through the admin API)
    /// the first time it makes a client. The tests of missing hubs (<c>EhNoAutoTests</c>) turn this off.
    /// </summary>
    protected virtual bool CreatesHubWhenNeeded => true;
    bool hubCreated;
    protected void EnsureHub()
    {
        if (hubCreated || !CreatesHubWhenNeeded) return;
        CreateHub(4).GetAwaiter().GetResult();
    }

    protected EventHubConnectionOptions Opts(EventHubsTransportType t = EventHubsTransportType.AmqpTcp)
    {
        var o = new EventHubConnectionOptions { TransportType = t, CertificateValidationCallback = (s, c, ch, e) => Emu.ValidateCert(s, c, ch, e) };
        if (t == EventHubsTransportType.AmqpWebSockets) o.CustomEndpointAddress = new Uri($"https://{Ns}.eventhub.localhost:{Emu.Port}");
        return o;
    }

    protected EventHubProducerClient Producer(EventHubsTransportType t = EventHubsTransportType.AmqpTcp, string? hub = null, EventHubProducerClientOptions? o = null)
    {
        o ??= new EventHubProducerClientOptions();
        if (hub == null) EnsureHub();
        o.ConnectionOptions = Opts(t);
        o.RetryOptions = new EventHubsRetryOptions { MaximumRetries = 0, TryTimeout = TimeSpan.FromSeconds(30) };
        return new EventHubProducerClient(Conn(hub), o);
    }

    protected EventHubConsumerClient Consumer(string cg = EventHubConsumerClient.DefaultConsumerGroupName, EventHubsTransportType t = EventHubsTransportType.AmqpTcp, string? hub = null)
    {
        if (hub == null) EnsureHub();
        var o = new EventHubConsumerClientOptions { ConnectionOptions = Opts(t), RetryOptions = new EventHubsRetryOptions { MaximumRetries = 0, TryTimeout = TimeSpan.FromSeconds(30) } };
        return new EventHubConsumerClient(cg, Conn(hub), o);
    }

    protected PartitionReceiver Receiver(string partition, EventPosition pos, string cg = EventHubConsumerClient.DefaultConsumerGroupName, Action<PartitionReceiverOptions>? cfg = null, EventHubsTransportType t = EventHubsTransportType.AmqpTcp)
    {
        EnsureHub();
        var o = new PartitionReceiverOptions { ConnectionOptions = Opts(t), RetryOptions = new EventHubsRetryOptions { MaximumRetries = 0, TryTimeout = TimeSpan.FromSeconds(30) } };
        cfg?.Invoke(o);
        return new PartitionReceiver(cg, partition, pos, Conn(), o);
    }

    /// <summary>Reads from one partition until <paramref name="count"/> events arrived or the time is up.</summary>
    protected static async Task<List<EventData>> Read(EventHubConsumerClient c, string partition, EventPosition pos, int count, int seconds = 15, ReadEventOptions? opt = null)
    {
        var list = new List<EventData>();
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(seconds));
        try
        {
            await foreach (var pe in c.ReadEventsFromPartitionAsync(partition, pos, opt ?? new ReadEventOptions(), cts.Token))
            {
                if (pe.Data == null) continue;
                list.Add(pe.Data);
                if (list.Count >= count) break;
            }
        }
        catch (OperationCanceledException) { }
        return list;
    }

    protected static async Task<List<EventData>> Receive(PartitionReceiver r, int count, int seconds = 15)
    {
        var list = new List<EventData>();
        var end = DateTime.UtcNow.AddSeconds(seconds);
        while (list.Count < count && DateTime.UtcNow < end)
            list.AddRange(await r.ReceiveBatchAsync(count - list.Count, TimeSpan.FromSeconds(2)));
        return list;
    }

    protected static string Text(EventData e) => e.EventBody.ToString();

    protected HttpClient Http() => Emu.NewHttpClient();

    /// <summary>Creates the hub through azemu's admin API with a given partition count and consumer groups.</summary>
    protected async Task CreateHub(int partitions, params string[] consumerGroups)
    {
        hubCreated = true;
        using var h = new HttpClient();
        var r = await h.PutAsJsonAsync($"http://127.0.0.1:{Emu.Port}/_azemu/eventhubs/{Ns}/hubs/{Hub}", new { partitionCount = partitions, consumerGroups });
        r.EnsureSuccessStatusCode();
    }
}
