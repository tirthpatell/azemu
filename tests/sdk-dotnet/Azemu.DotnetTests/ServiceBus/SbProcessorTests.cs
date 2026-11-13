using Azure.Messaging.ServiceBus;
using Azure.Messaging.ServiceBus.Administration;

namespace Azemu.DotnetTests.ServiceBus;

public class SbProcessorTests : SbBase, IClassFixture<SbFixture>
{
    public SbProcessorTests(SbFixture f) : base(f) { }

    [Fact]
    public async Task Processor_AutoCompletes_EveryMessage()
    {
        var q = await NewQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessagesAsync(Enumerable.Range(0, 20).Select(i => Msg("m" + i)));
        var seen = new System.Collections.Concurrent.ConcurrentBag<string>();
        await using var p = c.CreateProcessor(q, new ServiceBusProcessorOptions { MaxConcurrentCalls = 4 });
        p.ProcessMessageAsync += a => { seen.Add(a.Message.Body.ToString()); return Task.CompletedTask; };
        p.ProcessErrorAsync += _ => Task.CompletedTask;
        await p.StartProcessingAsync();
        Assert.True(await Wait.Until(() => seen.Count == 20, 30));
        await p.StopProcessingAsync();
        Assert.Equal(20, seen.Distinct().Count());
        Assert.True(await Wait.UntilAsync(async () => (await Admin().GetQueueRuntimePropertiesAsync(q)).Value.TotalMessageCount == 0, 10));
    }

    [Fact]
    public async Task Processor_HandlerThrows_MessageIsAbandonedAndErrorHandlerSeesTheException()
    {
        var q = await NewQueue(o => o.MaxDeliveryCount = 10);
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(Msg("boom"));
        var attempts = 0;
        var errors = new System.Collections.Concurrent.ConcurrentBag<ProcessErrorEventArgs>();
        await using var p = c.CreateProcessor(q, new ServiceBusProcessorOptions { MaxConcurrentCalls = 1 });
        p.ProcessMessageAsync += a => { if (Interlocked.Increment(ref attempts) < 3) throw new InvalidOperationException("nope"); return Task.CompletedTask; };
        p.ProcessErrorAsync += a => { errors.Add(a); return Task.CompletedTask; };
        await p.StartProcessingAsync();
        Assert.True(await Wait.Until(() => Volatile.Read(ref attempts) >= 3, 30));
        await p.StopProcessingAsync();
        Assert.Equal(3, attempts);
        Assert.Equal(2, errors.Count(e => e.Exception is InvalidOperationException && e.ErrorSource == ServiceBusErrorSource.ProcessMessageCallback));
    }

    [Fact]
    public async Task Processor_HandlerAlwaysThrows_MessageEndsInTheDeadLetterQueue()
    {
        var q = await NewQueue(o => o.MaxDeliveryCount = 2);
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(Msg("poison"));
        await using var p = c.CreateProcessor(q);
        p.ProcessMessageAsync += _ => throw new Exception("always");
        p.ProcessErrorAsync += _ => Task.CompletedTask;
        await p.StartProcessingAsync();
        Assert.True(await Wait.UntilAsync(async () => (await Admin().GetQueueRuntimePropertiesAsync(q)).Value.DeadLetterMessageCount == 1, 30));
        await p.StopProcessingAsync();
        var d = await Recv(c.CreateReceiver(q, new ServiceBusReceiverOptions { SubQueue = SubQueue.DeadLetter }));
        Assert.Equal("MaxDeliveryCountExceeded", d!.DeadLetterReason);
    }

    [Fact]
    public async Task Processor_ManualSettlement_DeadLetterFromHandler()
    {
        var q = await NewQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(Msg("dl"));
        await using var p = c.CreateProcessor(q, new ServiceBusProcessorOptions { AutoCompleteMessages = false });
        p.ProcessMessageAsync += async a => await a.DeadLetterMessageAsync(a.Message, "handler", "said so");
        p.ProcessErrorAsync += _ => Task.CompletedTask;
        await p.StartProcessingAsync();
        Assert.True(await Wait.UntilAsync(async () => (await Admin().GetQueueRuntimePropertiesAsync(q)).Value.DeadLetterMessageCount == 1, 20));
        await p.StopProcessingAsync();
        var d = await Recv(c.CreateReceiver(q, new ServiceBusReceiverOptions { SubQueue = SubQueue.DeadLetter }));
        Assert.Equal("said so", d!.DeadLetterErrorDescription);
    }

    [Fact]
    public async Task Processor_OnASubscription_ReceivesTopicMessages()
    {
        var a = Admin();
        var t = Names.Unique("t");
        await a.CreateTopicAsync(t);
        await a.CreateSubscriptionAsync(t, "s");
        await using var c = Client();
        await c.CreateSender(t).SendMessageAsync(Msg("topic"));
        var got = new TaskCompletionSource<string>();
        await using var p = c.CreateProcessor(t, "s");
        p.ProcessMessageAsync += x => { got.TrySetResult(x.Message.Body.ToString()); return Task.CompletedTask; };
        p.ProcessErrorAsync += _ => Task.CompletedTask;
        await p.StartProcessingAsync();
        Assert.Equal("topic", await got.Task.WaitAsync(TimeSpan.FromSeconds(20)));
        await p.StopProcessingAsync();
    }

    [Fact]
    public async Task Processor_OverWebSockets_ReceivesMessages()
    {
        var q = await NewQueue();
        await using var c = Client(ServiceBusTransportType.AmqpWebSockets);
        await c.CreateSender(q).SendMessageAsync(Msg("ws"));
        var got = new TaskCompletionSource<string>();
        await using var p = c.CreateProcessor(q);
        p.ProcessMessageAsync += x => { got.TrySetResult(x.Message.Body.ToString()); return Task.CompletedTask; };
        p.ProcessErrorAsync += _ => Task.CompletedTask;
        await p.StartProcessingAsync();
        Assert.Equal("ws", await got.Task.WaitAsync(TimeSpan.FromSeconds(20)));
        await p.StopProcessingAsync();
    }

    [Fact]
    public async Task Processor_RenewsLocksWhileTheHandlerRuns()
    {
        var q = await NewQueue(o => o.LockDuration = TimeSpan.FromSeconds(5));
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(Msg("slow"));
        var done = new TaskCompletionSource<int>();
        var calls = 0;
        await using var p = c.CreateProcessor(q, new ServiceBusProcessorOptions { MaxAutoLockRenewalDuration = TimeSpan.FromMinutes(1) });
        p.ProcessMessageAsync += async a => { Interlocked.Increment(ref calls); await Task.Delay(9000); done.TrySetResult(a.Message.DeliveryCount); };
        p.ProcessErrorAsync += _ => Task.CompletedTask;
        await p.StartProcessingAsync();
        Assert.Equal(1, await done.Task.WaitAsync(TimeSpan.FromSeconds(40)));
        await p.StopProcessingAsync();
        Assert.Equal(1, calls);
    }

    [Fact]
    public async Task Receiver_Prefetch_ReceivesAllMessages()
    {
        var q = await NewQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessagesAsync(Enumerable.Range(0, 30).Select(i => Msg("p" + i)));
        var r = c.CreateReceiver(q, new ServiceBusReceiverOptions { PrefetchCount = 10 });
        var seen = new List<string>();
        while (seen.Count < 30)
        {
            var more = await r.ReceiveMessagesAsync(10, TimeSpan.FromSeconds(5));
            Assert.NotEmpty(more);
            foreach (var m in more) { seen.Add(m.Body.ToString()); await r.CompleteMessageAsync(m); }
        }
        Assert.Equal(30, seen.Distinct().Count());
    }

    [Fact]
    public async Task ReceiveAsyncEnumerable_YieldsMessagesUntilStopped()
    {
        var q = await NewQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessagesAsync(new[] { Msg("1"), Msg("2"), Msg("3") });
        var r = c.CreateReceiver(q);
        var got = new List<string>();
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(20));
        await foreach (var m in r.ReceiveMessagesAsync(cts.Token))
        {
            got.Add(m.Body.ToString());
            await r.CompleteMessageAsync(m);
            if (got.Count == 3) break;
        }
        Assert.Equal(new[] { "1", "2", "3" }, got);
    }

    [Fact]
    public async Task ClosedClient_RefusesFurtherOperations()
    {
        var q = await NewQueue();
        var c = Client();
        var s = c.CreateSender(q);
        await s.SendMessageAsync(Msg());
        await c.DisposeAsync();
        await Assert.ThrowsAsync<ObjectDisposedException>(async () => await s.SendMessageAsync(Msg()));
    }
}
