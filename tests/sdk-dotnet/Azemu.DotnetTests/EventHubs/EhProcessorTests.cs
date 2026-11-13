using System.Collections.Concurrent;
using Azure.Messaging.EventHubs;
using Azure.Messaging.EventHubs.Consumer;
using Azure.Messaging.EventHubs.Producer;
using Azure.Storage;
using Azure.Storage.Blobs;

namespace Azemu.DotnetTests.EventHubs;

/// <summary>EventProcessorClient with the blob checkpoint store, which lives in azemu's Blob service.</summary>
public class EhProcessorTests : EhBase, IClassFixture<EhFixture>
{
    public EhProcessorTests(EhFixture f) : base(f) { }

    async Task<BlobContainerClient> Container()
    {
        var acct = Names.Unique("ehacct");
        var key = Convert.ToBase64String(new byte[32]);
        // a precaution: a HEAD error that carries a content type makes the SDK throw XmlException instead of reporting a missing blob
        var bo = new BlobClientOptions { Transport = Emu.TransportWithoutHeadContentType() };
        var c = new BlobContainerClient(new Uri($"{Emu.BlobUri(acct).GetLeftPart(UriPartial.Authority)}/{Names.Unique("ckpt")}"), new StorageSharedKeyCredential(acct, key), bo);
        await c.CreateAsync();
        return c;
    }

    EventProcessorClient Processor(BlobContainerClient store, string cg = "$Default")
    {
        EnsureHub();
        var o = new EventProcessorClientOptions
        {
            ConnectionOptions = Opts(),
            LoadBalancingUpdateInterval = TimeSpan.FromSeconds(1),
            PartitionOwnershipExpirationInterval = TimeSpan.FromSeconds(6),
            RetryOptions = new EventHubsRetryOptions { MaximumRetries = 1, TryTimeout = TimeSpan.FromSeconds(20) },
            MaximumWaitTime = TimeSpan.FromSeconds(1),
        };
        return new EventProcessorClient(store, cg, Conn(), o);
    }

    async Task SendTo(string partition, params string[] bodies)
    {
        await using var p = Producer();
        await p.SendAsync(bodies.Select(b => new EventData(b)), new SendEventOptions { PartitionId = partition });
    }

    [Fact]
    public async Task OneProcessor_ReadsAllPartitions_AndWritesCheckpointBlobs()
    {
        var store = await Container();
        for (int i = 0; i < 4; i++) await SendTo(i.ToString(), $"p{i}-a", $"p{i}-b");
        var seen = new ConcurrentBag<string>();
        var errors = new ConcurrentBag<Exception>();
        var proc = Processor(store);
        proc.ProcessEventAsync += async a =>
        {
            if (a.Data == null) return;
            seen.Add(a.Data.EventBody.ToString());
            await a.UpdateCheckpointAsync();
        };
        proc.ProcessErrorAsync += a => { errors.Add(a.Exception); return Task.CompletedTask; };
        await proc.StartProcessingAsync();
        Assert.True(await Wait.Until(() => seen.Count >= 8, 40));
        await proc.StopProcessingAsync();
        Assert.Empty(errors);
        Assert.Equal(8, seen.Distinct().Count());
        var names = new List<string>();
        await foreach (var b in store.GetBlobsAsync()) names.Add(b.Name);
        Assert.Equal(4, names.Count(n => n.Contains("/checkpoint/")));
        Assert.Equal(4, names.Count(n => n.Contains("/ownership/")));
    }

    [Fact]
    public async Task Restart_ContinuesAfterTheLastCheckpoint_WithoutReplayingOldEvents()
    {
        var store = await Container();
        await SendTo("0", "old1", "old2");
        var first = new ConcurrentBag<string>();
        var p1 = Processor(store);
        p1.ProcessEventAsync += async a => { if (a.Data == null) return; first.Add(a.Data.EventBody.ToString()); await a.UpdateCheckpointAsync(); };
        p1.ProcessErrorAsync += _ => Task.CompletedTask;
        await p1.StartProcessingAsync();
        Assert.True(await Wait.Until(() => first.Count >= 2, 40));
        await p1.StopProcessingAsync();

        await SendTo("0", "new1");
        var second = new ConcurrentBag<string>();
        var p2 = Processor(store);
        p2.ProcessEventAsync += async a => { if (a.Data == null) return; second.Add(a.Data.EventBody.ToString()); await a.UpdateCheckpointAsync(); };
        p2.ProcessErrorAsync += _ => Task.CompletedTask;
        await p2.StartProcessingAsync();
        Assert.True(await Wait.Until(() => second.Count >= 1, 40));
        await Task.Delay(2000);
        await p2.StopProcessingAsync();
        Assert.Equal(new[] { "new1" }, second.ToArray());
    }

    [Fact]
    public async Task TwoProcessors_ShareThePartitionsBetweenThem()
    {
        var store = await Container();
        var a = new ConcurrentDictionary<string, byte>();
        var b = new ConcurrentDictionary<string, byte>();
        var p1 = Processor(store); var p2 = Processor(store);
        p1.ProcessEventAsync += x => { if (x.Data != null) a[x.Partition.PartitionId] = 1; return Task.CompletedTask; };
        p2.ProcessEventAsync += x => { if (x.Data != null) b[x.Partition.PartitionId] = 1; return Task.CompletedTask; };
        p1.PartitionInitializingAsync += x => { a[x.PartitionId] = 1; return Task.CompletedTask; };
        p2.PartitionInitializingAsync += x => { b[x.PartitionId] = 1; return Task.CompletedTask; };
        p1.ProcessErrorAsync += _ => Task.CompletedTask; p2.ProcessErrorAsync += _ => Task.CompletedTask;
        await p1.StartProcessingAsync();
        Assert.True(await Wait.Until(() => a.Count == 4, 30));
        await p2.StartProcessingAsync();
        // the second processor claims partitions until the split is even: two each
        Assert.True(await Wait.Until(() => p1.IsRunning && !b.IsEmpty && p2.IsRunning, 60));
        var done = await Wait.UntilAsync(async () =>
        {
            var owners = new Dictionary<string, string>();
            await foreach (var blob in store.GetBlobsAsync(Azure.Storage.Blobs.Models.BlobTraits.Metadata, Azure.Storage.Blobs.Models.BlobStates.None, null, default))
                if (blob.Name.Contains("/ownership/") && blob.Metadata.TryGetValue("ownerid", out var o)) owners[blob.Name] = o;
            return owners.Count == 4 && owners.Values.Distinct().Count() == 2 && owners.Values.GroupBy(x => x).All(g => g.Count() == 2);
        }, 60);
        await p1.StopProcessingAsync(); await p2.StopProcessingAsync();
        Assert.True(done, "ownership did not settle at two partitions per processor");
    }

    [Fact]
    public async Task Processor_WithACustomConsumerGroup_ReadsItsOwnCopy()
    {
        var store = await Container();
        await CreateHub(2, "cgp");
        await SendTo("0", "x");
        var seen = new ConcurrentBag<string>();
        var p = Processor(store, "cgp");
        p.ProcessEventAsync += a => { if (a.Data != null) seen.Add(a.Data.EventBody.ToString()); return Task.CompletedTask; };
        p.ProcessErrorAsync += _ => Task.CompletedTask;
        await p.StartProcessingAsync();
        Assert.True(await Wait.Until(() => seen.Count >= 1, 40));
        await p.StopProcessingAsync();
        Assert.Equal("x", seen.Single());
    }

    [Fact]
    public async Task Processor_HandlerException_IsReportedToTheErrorHandler_AndProcessingContinues()
    {
        var store = await Container();
        await CreateHub(1);
        await SendTo("0", "bad", "good");
        var errors = new ConcurrentBag<Azure.Messaging.EventHubs.Processor.ProcessErrorEventArgs>();
        var seen = new ConcurrentBag<string>();
        var p = Processor(store);
        p.ProcessEventAsync += a =>
        {
            if (a.Data == null) return Task.CompletedTask;
            var t = a.Data.EventBody.ToString();
            seen.Add(t);
            if (t == "bad") throw new InvalidOperationException("handler failed");
            return Task.CompletedTask;
        };
        p.ProcessErrorAsync += a => { errors.Add(a); return Task.CompletedTask; };
        await p.StartProcessingAsync();
        Assert.True(await Wait.Until(() => seen.Count >= 2, 40));
        await p.StopProcessingAsync();
        // the SDK wraps an exception from the handler in an EventHubsException and keeps ours as the inner one
        Assert.Contains(errors, e => e.Exception is EventHubsException { InnerException: InvalidOperationException });
    }
}
