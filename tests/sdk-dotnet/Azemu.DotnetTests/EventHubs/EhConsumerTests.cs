using Azure.Messaging.EventHubs;
using Azure.Messaging.EventHubs.Consumer;
using Azure.Messaging.EventHubs.Primitives;
using Azure.Messaging.EventHubs.Producer;

namespace Azemu.DotnetTests.EventHubs;

public class EhConsumerTests : EhBase, IClassFixture<EhFixture>
{
    public EhConsumerTests(EhFixture f) : base(f) { }

    async Task Send(string partition, params string[] bodies)
    {
        await using var p = Producer();
        await p.SendAsync(bodies.Select(b => new EventData(b)), new SendEventOptions { PartitionId = partition });
    }

    [Fact]
    public async Task Earliest_ReadsEverythingFromTheStartInOrder()
    {
        await Send("0", "a", "b", "c");
        await using var c = Consumer();
        var got = await Read(c, "0", EventPosition.Earliest, 3);
        Assert.Equal(new[] { "a", "b", "c" }, got.Select(Text));
        Assert.Equal(got.Select(g => g.SequenceNumber).OrderBy(x => x), got.Select(g => g.SequenceNumber));
        Assert.Equal(got[0].SequenceNumber + 2, got[2].SequenceNumber);
    }

    [Fact]
    public async Task Latest_SkipsOldEvents_AndSeesEventsSentAfterTheLinkOpened()
    {
        await Send("0", "old");
        await using var r = Receiver("0", EventPosition.Latest);
        Assert.Empty(await r.ReceiveBatchAsync(10, TimeSpan.FromSeconds(1)));   // opens the link
        await Send("0", "new1", "new2");
        var got = await Receive(r, 2);
        Assert.Equal(new[] { "new1", "new2" }, got.Select(Text));
    }

    [Fact]
    public async Task FromSequenceNumber_Inclusive_StartsAtThatEvent()
    {
        await Send("1", "e0", "e1", "e2", "e3");
        await using var c = Consumer();
        var all = await Read(c, "1", EventPosition.Earliest, 4);
        var third = all[2].SequenceNumber;
        var got = await Read(c, "1", EventPosition.FromSequenceNumber(third), 2);
        Assert.Equal(new[] { "e2", "e3" }, got.Select(Text));
    }

    [Fact]
    public async Task FromSequenceNumber_Exclusive_StartsAfterThatEvent()
    {
        await Send("1", "e0", "e1", "e2");
        await using var c = Consumer();
        var all = await Read(c, "1", EventPosition.Earliest, 3);
        var got = await Read(c, "1", EventPosition.FromSequenceNumber(all[0].SequenceNumber, isInclusive: false), 2);
        Assert.Equal(new[] { "e1", "e2" }, got.Select(Text));
    }

    [Fact]
    public async Task FromOffset_StartsAtThatEvent()
    {
        await Send("2", "o0", "o1", "o2");
        await using var c = Consumer();
        var all = await Read(c, "2", EventPosition.Earliest, 3);
        var got = await Read(c, "2", EventPosition.FromOffset(all[1].Offset), 2);
        Assert.Equal(new[] { "o1", "o2" }, got.Select(Text));
    }

    [Fact]
    public async Task FromEnqueuedTime_ReadsOnlyEventsEnqueuedAtOrAfterThatTime()
    {
        await Send("3", "before");
        await Task.Delay(1500);
        var mark = DateTimeOffset.UtcNow;
        await Task.Delay(500);
        await Send("3", "after1", "after2");
        await using var c = Consumer();
        var got = await Read(c, "3", EventPosition.FromEnqueuedTime(mark), 2);
        Assert.Equal(new[] { "after1", "after2" }, got.Select(Text));
        Assert.All(got, g => Assert.True(g.EnqueuedTime >= mark.AddSeconds(-1)));
    }

    [Fact]
    public async Task FromSequenceNumber_BeyondTheEnd_Fails()
    {
        await Send("0", "one");
        await using var c = Consumer();
        var ex = await Assert.ThrowsAnyAsync<Exception>(async () => await Read(c, "0", EventPosition.FromSequenceNumber(5000), 1, 5));
        Assert.True(ex is EventHubsException or ArgumentException, ex.GetType().FullName);
    }

    [Fact]
    public async Task ConsumerGroups_ReadTheSameEventsIndependently()
    {
        await CreateHub(2, "cg-a", "cg-b");
        await Send("0", "x", "y");
        await using var a = Consumer("cg-a");
        await using var b = Consumer("cg-b");
        Assert.Equal(new[] { "x", "y" }, (await Read(a, "0", EventPosition.Earliest, 2)).Select(Text));
        Assert.Equal(new[] { "x", "y" }, (await Read(b, "0", EventPosition.Earliest, 2)).Select(Text));
    }

    [Fact]
    public async Task AdminCreatedHub_HasTheRequestedPartitionCount()
    {
        await CreateHub(2);
        await using var p = Producer();
        Assert.Equal(new[] { "0", "1" }, (await p.GetPartitionIdsAsync()).OrderBy(x => x).ToArray());
    }

    [Fact]
    public async Task ReadWithMaximumWaitTime_ReturnsEmptyEventsWhenIdle()
    {
        await Send("0", "x");
        await using var c = Consumer();
        var seenNull = false; var seenData = false;
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(15));
        await foreach (var pe in c.ReadEventsFromPartitionAsync("0", EventPosition.Earliest, new ReadEventOptions { MaximumWaitTime = TimeSpan.FromSeconds(1) }, cts.Token))
        {
            if (pe.Data == null) { seenNull = true; Assert.Equal("0", pe.Partition.PartitionId); break; }
            seenData = true;
        }
        Assert.True(seenData);
        Assert.True(seenNull);
    }

    [Fact]
    public async Task LastEnqueuedEventProperties_AreTrackedWhenRequested()
    {
        await Send("0", "a", "b", "c");
        await using var r = Receiver("0", EventPosition.Earliest, cfg: o => o.TrackLastEnqueuedEventProperties = true);
        var got = await Receive(r, 3);
        Assert.Equal(3, got.Count);
        var props = r.ReadLastEnqueuedEventProperties();
        Assert.Equal(got[2].SequenceNumber, props.SequenceNumber);
        Assert.NotNull(props.EnqueuedTime);
        Assert.NotNull(props.LastReceivedTime);
    }

    [Fact]
    public async Task ConsumerClient_LastEnqueuedEventProperties_OnPartitionEvent()
    {
        await Send("0", "a", "b");
        await using var c = Consumer();
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(15));
        await foreach (var pe in c.ReadEventsFromPartitionAsync("0", EventPosition.Earliest, new ReadEventOptions { TrackLastEnqueuedEventProperties = true }, cts.Token))
        {
            if (pe.Data == null) continue;
            var last = pe.Partition.ReadLastEnqueuedEventProperties();
            Assert.NotNull(last.SequenceNumber);
            Assert.True(last.SequenceNumber >= pe.Data.SequenceNumber);
            break;
        }
    }

    [Fact]
    public async Task OwnerLevel_HigherLevelReceiverDisconnectsTheLowerOne()
    {
        await Send("0", "x");
        await using var low = Receiver("0", EventPosition.Earliest, cfg: o => o.OwnerLevel = 5);
        Assert.Single(await Receive(low, 1));
        await using var high = Receiver("0", EventPosition.Earliest, cfg: o => o.OwnerLevel = 10);
        Assert.Single(await Receive(high, 1));
        await Send("0", "next");
        var ex = await Assert.ThrowsAsync<EventHubsException>(async () =>
        {
            for (int i = 0; i < 10; i++) await low.ReceiveBatchAsync(10, TimeSpan.FromSeconds(1));
        });
        Assert.Equal(EventHubsException.FailureReason.ConsumerDisconnected, ex.Reason);
    }

    [Fact]
    public async Task OwnerLevel_LowerLevelReceiverCannotAttachNextToAHigherOne()
    {
        await Send("0", "x");
        await using var high = Receiver("0", EventPosition.Earliest, cfg: o => o.OwnerLevel = 10);
        Assert.Single(await Receive(high, 1));
        await using var low = Receiver("0", EventPosition.Earliest, cfg: o => o.OwnerLevel = 5);
        // the SDK keeps a link error it met while closing and throws it on the next receive call: the first call returns empty
        var ex = await Assert.ThrowsAsync<EventHubsException>(async () => { for (int i = 0; i < 5; i++) await low.ReceiveBatchAsync(1, TimeSpan.FromSeconds(1)); });
        Assert.Equal(EventHubsException.FailureReason.ConsumerDisconnected, ex.Reason);
    }

    [Fact]
    public async Task ReceiverWithoutOwnerLevel_SixthReceiverInTheGroupIsRefused()
    {
        await Send("0", "x");
        var rs = new List<PartitionReceiver>();
        try
        {
            for (int i = 0; i < 5; i++) { var r = Receiver("0", EventPosition.Earliest); rs.Add(r); await r.ReceiveBatchAsync(1, TimeSpan.FromSeconds(1)); }
            await using var sixth = Receiver("0", EventPosition.Earliest);
            var ex = await Assert.ThrowsAsync<EventHubsException>(async () => { for (int i = 0; i < 5; i++) await sixth.ReceiveBatchAsync(1, TimeSpan.FromSeconds(1)); });
            Assert.Equal(EventHubsException.FailureReason.QuotaExceeded, ex.Reason);
        }
        finally { foreach (var r in rs) await r.DisposeAsync(); }
    }

    [Fact]
    public async Task PartitionReceiver_ReceiveBatch_OnAnEmptyPartition_ReturnsEmpty()
    {
        await using var r = Receiver("3", EventPosition.Earliest);
        var sw = System.Diagnostics.Stopwatch.StartNew();
        Assert.Empty(await r.ReceiveBatchAsync(5, TimeSpan.FromSeconds(2)));
        Assert.InRange(sw.Elapsed.TotalSeconds, 1, 8);
    }

    [Fact]
    public async Task PartitionReceiver_GetPartitionProperties_Matches()
    {
        await Send("2", "a");
        await using var r = Receiver("2", EventPosition.Earliest);
        var pp = await r.GetPartitionPropertiesAsync();
        Assert.Equal("2", pp.Id);
        Assert.False(pp.IsEmpty);
    }

    [Fact]
    public async Task Consumer_ReadingAMissingPartition_Fails()
    {
        await using var c = Consumer();
        await Assert.ThrowsAnyAsync<Exception>(async () => await Read(c, "42", EventPosition.Earliest, 1, 5));
    }

    [Fact]
    public async Task WebSocketTransport_PartitionReceiver()
    {
        await Send("0", "ws");
        await using var r = Receiver("0", EventPosition.Earliest, t: EventHubsTransportType.AmqpWebSockets);
        Assert.Equal("ws", Text((await Receive(r, 1)).Single()));
    }

    [Fact]
    public async Task ManyEvents_InSeveralBatches_AreAllReadBack()
    {
        await using var p = Producer();
        for (int b = 0; b < 5; b++)
            await p.SendAsync(Enumerable.Range(0, 100).Select(i => new EventData($"{b}-{i}")), new SendEventOptions { PartitionId = "0" });
        await using var c = Consumer();
        var got = await Read(c, "0", EventPosition.Earliest, 500, 40);
        Assert.Equal(500, got.Count);
        Assert.Equal(500, got.Select(Text).Distinct().Count());
    }
}

public class EhNoAutoTests : EhBase, IClassFixture<EhNoAutoFixture>
{
    public EhNoAutoTests(EhNoAutoFixture f) : base(f) { }
    protected override bool CreatesHubWhenNeeded => false;

    [Fact]
    public async Task MissingEventHub_FailsWithResourceNotFound()
    {
        await using var p = Producer();
        var ex = await Assert.ThrowsAsync<EventHubsException>(async () => await p.GetPartitionIdsAsync());
        Assert.Equal(EventHubsException.FailureReason.ResourceNotFound, ex.Reason);
    }

    [Fact]
    public async Task MissingConsumerGroup_FailsWithResourceNotFound()
    {
        await CreateHub(2);
        await using var c = Consumer("nope-group");
        var ex = await Assert.ThrowsAsync<EventHubsException>(async () => await Read(c, "0", EventPosition.Earliest, 1, 5));
        Assert.Equal(EventHubsException.FailureReason.ResourceNotFound, ex.Reason);
    }

    [Fact]
    public async Task ExistingHub_StillWorks()
    {
        await CreateHub(2, "cg1");
        await using var p = Producer();
        await p.SendAsync(new[] { new EventData("ok") }, new SendEventOptions { PartitionId = "1" });
        await using var c = Consumer("cg1");
        Assert.Equal("ok", Text((await Read(c, "1", EventPosition.Earliest, 1)).Single()));
    }
}
