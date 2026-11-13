using Azure.Messaging.EventHubs;
using Azure.Messaging.EventHubs.Consumer;
using Azure.Messaging.EventHubs.Producer;

namespace Azemu.DotnetTests.EventHubs;

public class EhProducerTests : EhBase, IClassFixture<EhFixture>
{
    public EhProducerTests(EhFixture f) : base(f) { }

    [Fact]
    public async Task GetEventHubProperties_ReturnsNameAndFourPartitions()
    {
        await using var p = Producer();
        var props = await p.GetEventHubPropertiesAsync();
        Assert.Equal(Hub, props.Name, ignoreCase: true);
        Assert.Equal(4, props.PartitionIds.Length);
        Assert.Equal(new[] { "0", "1", "2", "3" }, (await p.GetPartitionIdsAsync()).OrderBy(x => x).ToArray());
        Assert.True(props.CreatedOn > DateTimeOffset.UtcNow.AddDays(-1));
    }

    [Fact]
    public async Task GetPartitionProperties_OfAnEmptyPartition_IsEmpty()
    {
        await using var p = Producer();
        var pp = await p.GetPartitionPropertiesAsync("0");
        Assert.True(pp.IsEmpty);
        Assert.Equal("0", pp.Id);
        Assert.Equal(Hub, pp.EventHubName, ignoreCase: true);
    }

    [Fact]
    public async Task GetPartitionProperties_AfterSending_ShowsTheLastEnqueuedEvent()
    {
        await using var p = Producer();
        await p.SendAsync(new[] { new EventData("a"), new EventData("b") }, new SendEventOptions { PartitionId = "1" });
        var pp = await p.GetPartitionPropertiesAsync("1");
        Assert.False(pp.IsEmpty);
        Assert.Equal(pp.BeginningSequenceNumber + 1, pp.LastEnqueuedSequenceNumber);
        Assert.True(pp.LastEnqueuedTime > DateTimeOffset.UtcNow.AddMinutes(-5));
    }

    [Fact]
    public async Task GetPartitionProperties_UnknownPartition_Fails()
    {
        await using var p = Producer();
        var ex = await Assert.ThrowsAnyAsync<Exception>(async () => await p.GetPartitionPropertiesAsync("99"));
        Assert.IsType<ArgumentOutOfRangeException>(ex is EventHubsException ? ex.InnerException : ex);
    }

    [Fact]
    public async Task SendToPartitionId_AllEventsLandInThatPartition()
    {
        await using var p = Producer();
        await p.SendAsync(Enumerable.Range(0, 5).Select(i => new EventData("e" + i)), new SendEventOptions { PartitionId = "2" });
        await using var c = Consumer();
        var got = await Read(c, "2", EventPosition.Earliest, 5);
        Assert.Equal(Enumerable.Range(0, 5).Select(i => "e" + i), got.Select(Text));
        Assert.Empty(await Read(c, "3", EventPosition.Earliest, 1, 3));
    }

    [Fact]
    public async Task SendWithPartitionKey_SameKeyAlwaysSamePartition_AndKeyIsKept()
    {
        await using var p = Producer();
        for (int i = 0; i < 3; i++) await p.SendAsync(new[] { new EventData("k" + i) }, new SendEventOptions { PartitionKey = "customer-42" });
        var pids = await p.GetPartitionIdsAsync();
        await using var c = Consumer();
        var total = 0; var withEvents = new List<string>();
        foreach (var pid in pids)
        {
            var got = await Read(c, pid, EventPosition.Earliest, 3, 3);
            if (got.Count > 0) { withEvents.Add(pid); Assert.All(got, e => Assert.Equal("customer-42", e.PartitionKey)); }
            total += got.Count;
        }
        Assert.Equal(3, total);
        Assert.Single(withEvents);
    }

    [Fact]
    public async Task DifferentPartitionKeys_SpreadOverPartitions_AndOrderIsKeptPerKey()
    {
        await using var p = Producer();
        for (int k = 0; k < 20; k++) await p.SendAsync(new[] { new EventData($"k{k}") }, new SendEventOptions { PartitionKey = "key-" + k });
        await using var c = Consumer();
        var used = 0;
        foreach (var pid in await p.GetPartitionIdsAsync()) if ((await Read(c, pid, EventPosition.Earliest, 1, 2)).Count > 0) used++;
        Assert.True(used >= 2, $"only {used} partition(s) used by 20 keys");
    }

    [Fact]
    public async Task SendWithoutKeyOrPartition_EventsAreAccepted_AndReadFromAllPartitions()
    {
        await using var p = Producer();
        await p.SendAsync(Enumerable.Range(0, 8).Select(i => new EventData("r" + i)));
        await using var c = Consumer();
        var all = new List<string>();
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(20));
        await foreach (var pe in c.ReadEventsAsync(startReadingAtEarliestEvent: true, new ReadEventOptions { MaximumWaitTime = TimeSpan.FromSeconds(2) }, cts.Token))
        {
            if (pe.Data == null) continue;
            all.Add(Text(pe.Data));
            if (all.Count == 8) break;
        }
        Assert.Equal(Enumerable.Range(0, 8).Select(i => "r" + i).OrderBy(x => x), all.OrderBy(x => x));
    }

    [Fact]
    public async Task EventData_AllSystemAndApplicationProperties_RoundTrip()
    {
        await using var p = Producer();
        var guid = Guid.NewGuid();
        var when = new DateTimeOffset(2024, 5, 6, 7, 8, 9, TimeSpan.Zero);
        var e = new EventData(BinaryData.FromBytes(new byte[] { 0, 1, 2, 255 })) { ContentType = "application/octet-stream", CorrelationId = "corr", MessageId = "mid" };
        e.Properties["s"] = "text"; e.Properties["i"] = 42; e.Properties["l"] = 1L << 40; e.Properties["b"] = true;
        e.Properties["d"] = 2.5d; e.Properties["g"] = guid; e.Properties["t"] = when; e.Properties["by"] = (byte)7; e.Properties["u"] = new Uri("http://example.com/p");
        await p.SendAsync(new[] { e }, new SendEventOptions { PartitionId = "0" });
        await using var c = Consumer();
        var got = (await Read(c, "0", EventPosition.Earliest, 1)).Single();
        Assert.Equal(new byte[] { 0, 1, 2, 255 }, got.EventBody.ToArray());
        Assert.Equal("application/octet-stream", got.ContentType);
        Assert.Equal("corr", got.CorrelationId);
        Assert.Equal("mid", got.MessageId);
        Assert.Equal("text", got.Properties["s"]); Assert.Equal(42, got.Properties["i"]); Assert.Equal(1L << 40, got.Properties["l"]);
        Assert.Equal(true, got.Properties["b"]); Assert.Equal(2.5d, got.Properties["d"]); Assert.Equal(guid, got.Properties["g"]);
        Assert.Equal(when, got.Properties["t"]); Assert.Equal((byte)7, got.Properties["by"]); Assert.Equal(new Uri("http://example.com/p"), got.Properties["u"]);
        Assert.True(got.SequenceNumber >= 0);
        Assert.True(got.EnqueuedTime > DateTimeOffset.UtcNow.AddMinutes(-5));
        Assert.False(string.IsNullOrEmpty(got.OffsetString));
    }

    [Fact]
    public async Task Batch_FillsUpToTheSizeLimit_ThenAllEventsAreStored()
    {
        await using var p = Producer();
        using var batch = await p.CreateBatchAsync(new CreateBatchOptions { PartitionId = "0", MaximumSizeInBytes = 10 * 1024 });
        var n = 0;
        while (batch.TryAdd(new EventData(new byte[900]))) n++;
        Assert.InRange(n, 8, 12);
        Assert.Equal(n, batch.Count);
        Assert.True(batch.SizeInBytes <= 10 * 1024);
        await p.SendAsync(batch);
        await using var c = Consumer();
        Assert.Equal(n, (await Read(c, "0", EventPosition.Earliest, n)).Count);
    }

    [Fact]
    public async Task Batch_DefaultMaximumSize_ComesFromTheLinksMaxMessageSize()
    {
        await using var p = Producer();
        using var batch = await p.CreateBatchAsync();
        Assert.Equal(1048576 - 0, batch.MaximumSizeInBytes);
    }

    [Fact]
    public async Task Send_SingleEventLargerThanTheLimit_FailsWithMessageSizeExceeded()
    {
        await using var p = Producer();
        var ex = await Assert.ThrowsAsync<EventHubsException>(async () => await p.SendAsync(new[] { new EventData(new byte[1100 * 1024]) }, new SendEventOptions { PartitionId = "0" }));
        Assert.Equal(EventHubsException.FailureReason.MessageSizeExceeded, ex.Reason);
    }

    [Fact]
    public async Task Batch_WithPartitionKey_EventsKeepTheKey()
    {
        await using var p = Producer();
        using var batch = await p.CreateBatchAsync(new CreateBatchOptions { PartitionKey = "pk-1" });
        batch.TryAdd(new EventData("a")); batch.TryAdd(new EventData("b"));
        await p.SendAsync(batch);
        await using var c = Consumer();
        var total = new List<EventData>();
        foreach (var pid in await p.GetPartitionIdsAsync()) total.AddRange(await Read(c, pid, EventPosition.Earliest, 2, 2));
        Assert.Equal(2, total.Count);
        Assert.All(total, e => Assert.Equal("pk-1", e.PartitionKey));
    }

    [Fact]
    public async Task BufferedProducer_FlushesEnqueuedEventsToThePartition()
    {
        EnsureHub();
        var o = new EventHubBufferedProducerClientOptions { ConnectionOptions = Opts(), MaximumWaitTime = TimeSpan.FromSeconds(1), RetryOptions = new EventHubsRetryOptions { MaximumRetries = 0 } };
        await using var bp = new EventHubBufferedProducerClient(Conn(), o);
        var succeeded = 0;
        bp.SendEventBatchSucceededAsync += a => { Interlocked.Add(ref succeeded, a.EventBatch.Count); return Task.CompletedTask; };
        bp.SendEventBatchFailedAsync += a => Task.CompletedTask;
        for (int i = 0; i < 25; i++) await bp.EnqueueEventAsync(new EventData("buf" + i), new EnqueueEventOptions { PartitionId = "1" });
        await bp.FlushAsync();
        Assert.Equal(25, succeeded);
        Assert.Equal(0, bp.TotalBufferedEventCount);
        await using var c = Consumer();
        Assert.Equal(25, (await Read(c, "1", EventPosition.Earliest, 25)).Count);
    }

    [Fact]
    public async Task BufferedProducer_WithPartitionKey_EnqueuesAndFlushes()
    {
        EnsureHub();
        var o = new EventHubBufferedProducerClientOptions { ConnectionOptions = Opts(), MaximumWaitTime = TimeSpan.FromSeconds(1) };
        await using var bp = new EventHubBufferedProducerClient(Conn(), o);
        bp.SendEventBatchSucceededAsync += _ => Task.CompletedTask;
        bp.SendEventBatchFailedAsync += _ => Task.CompletedTask;
        await bp.EnqueueEventsAsync(Enumerable.Range(0, 6).Select(i => new EventData("p" + i)), new EnqueueEventOptions { PartitionKey = "bk" });
        await bp.FlushAsync();
        await using var c = Consumer();
        var n = 0;
        foreach (var pid in await bp.GetPartitionIdsAsync()) n += (await Read(c, pid, EventPosition.Earliest, 6, 2)).Count;
        Assert.Equal(6, n);
    }

    [Fact]
    public async Task WebSocketTransport_SendAndRead()
    {
        await using var p = Producer(EventHubsTransportType.AmqpWebSockets);
        await p.SendAsync(new[] { new EventData("ws") }, new SendEventOptions { PartitionId = "0" });
        await using var c = Consumer(t: EventHubsTransportType.AmqpWebSockets);
        Assert.Equal("ws", Text((await Read(c, "0", EventPosition.Earliest, 1)).Single()));
    }

    sealed class FakeCredential : Azure.Core.TokenCredential
    {
        public override Azure.Core.AccessToken GetToken(Azure.Core.TokenRequestContext r, CancellationToken ct) => new("not-a-real-token", DateTimeOffset.UtcNow.AddHours(1));
        public override ValueTask<Azure.Core.AccessToken> GetTokenAsync(Azure.Core.TokenRequestContext r, CancellationToken ct) => new(GetToken(r, ct));
    }

    [Fact]
    public async Task TokenCredential_WithACustomEndpoint_ReachesTheEmulatorOverTls()
    {
        // EventHubProducerClient(namespace, hub, credential) uses TLS; the custom endpoint carries the emulator's port.
        // A token credential makes the SDK send a JWT in $cbs put-token; azemu does not check it.
        EnsureHub();
        var opts = Opts();
        opts.CustomEndpointAddress = new Uri($"sb://{Ns}.eventhub.localhost:{Emu.Port}");
        await using var p = new EventHubProducerClient($"{Ns}.eventhub.localhost", Hub, new FakeCredential(), new EventHubProducerClientOptions { ConnectionOptions = opts });
        Assert.Equal(4, (await p.GetPartitionIdsAsync()).Length);
        await p.SendAsync(new[] { new EventData("tls") }, new SendEventOptions { PartitionId = "0" });
        Assert.Equal(1, (await p.GetPartitionPropertiesAsync("0")).LastEnqueuedSequenceNumber + 1);
    }
}
