using System.Transactions;
using Azure.Messaging.ServiceBus;
using Azure.Messaging.ServiceBus.Administration;

namespace Azemu.DotnetTests.ServiceBus;

/// <summary>
/// Transactions with System.Transactions.TransactionScope. The SDK declares a transaction on a coordinator link
/// ("amqp:coordinator"); with EnableCrossEntityTransactions it also opens the sending link "via" a receiving entity.
/// </summary>
public class SbTransactionTests : SbBase, IClassFixture<SbFixture>
{
    public SbTransactionTests(SbFixture f) : base(f) { }

    static TransactionScope Scope() => new(TransactionScopeAsyncFlowOption.Enabled);

    async Task<long> Count(string q) => (await Admin().GetQueueRuntimePropertiesAsync(q)).Value.TotalMessageCount;

    [Fact]
    public async Task Send_InsideACompletedScope_IsVisibleAfterTheScopeCompletes()
    {
        var q = await NewQueue();
        await using var c = Client();
        var s = c.CreateSender(q);
        using (var ts = Scope())
        {
            await s.SendMessageAsync(Msg("in-txn"));
            Assert.Equal(0, await Count(q));   // not visible before the commit
            ts.Complete();
        }
        Assert.Equal("in-txn", (await Recv(c.CreateReceiver(q)))!.Body.ToString());
    }

    [Fact]
    public async Task Send_InsideAScopeThatIsNotCompleted_IsRolledBack()
    {
        var q = await NewQueue();
        await using var c = Client();
        var s = c.CreateSender(q);
        using (var ts = Scope())
        {
            await s.SendMessageAsync(Msg("never"));
        }
        Assert.Null(await c.CreateReceiver(q).ReceiveMessageAsync(TimeSpan.FromSeconds(2)));
        Assert.Equal(0, await Count(q));
    }

    [Fact]
    public async Task SendBatch_InsideACompletedScope_AllMessagesAppearTogether()
    {
        var q = await NewQueue();
        await using var c = Client();
        var s = c.CreateSender(q);
        using (var ts = Scope())
        {
            await s.SendMessagesAsync(Enumerable.Range(0, 5).Select(i => Msg("b" + i)));
            await s.SendMessageAsync(Msg("single"));
            ts.Complete();
        }
        Assert.Equal(6, await Count(q));
    }

    [Fact]
    public async Task Complete_InsideACompletedScope_RemovesTheMessage()
    {
        var q = await NewQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(Msg("x"));
        var r = c.CreateReceiver(q);
        var m = await Recv(r);
        using (var ts = Scope()) { await r.CompleteMessageAsync(m!); ts.Complete(); }
        Assert.Equal(0, await Count(q));
    }

    [Fact]
    public async Task Complete_InsideAScopeThatIsNotCompleted_LeavesTheMessageInTheQueue()
    {
        var q = await NewQueue(o => o.LockDuration = TimeSpan.FromSeconds(5));
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(Msg("x"));
        var r = c.CreateReceiver(q);
        var m = await Recv(r);
        using (var ts = Scope()) { await r.CompleteMessageAsync(m!); }
        Assert.Equal(1, await Count(q));
        // the message stays locked by its receiver until the lock runs out, then comes back
        var again = await Recv(r, 20);
        Assert.NotNull(again);
        Assert.Equal(2, again!.DeliveryCount);
    }

    [Fact]
    public async Task ReceiveAndSend_SameEntity_InOneScope_Commits()
    {
        var q = await NewQueue();
        await using var c = Client();
        var s = c.CreateSender(q);
        await s.SendMessageAsync(Msg("in"));
        var r = c.CreateReceiver(q);
        var m = await Recv(r);
        using (var ts = Scope())
        {
            await r.CompleteMessageAsync(m!);
            await s.SendMessageAsync(Msg("out"));
            ts.Complete();
        }
        var got = await Recv(c.CreateReceiver(q));
        Assert.Equal("out", got!.Body.ToString());
        Assert.Equal(1, await Count(q));
    }

    [Fact]
    public async Task AbandonAndDeadLetter_InsideACompletedScope_Apply()
    {
        var q = await NewQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessagesAsync(new[] { Msg("a"), Msg("b") });
        var r = c.CreateReceiver(q);
        var ms = await r.ReceiveMessagesAsync(2, TimeSpan.FromSeconds(5));
        Assert.Equal(2, ms.Count);
        using (var ts = Scope())
        {
            await r.DeadLetterMessageAsync(ms[0], "txn-dl");
            await r.CompleteMessageAsync(ms[1]);
            ts.Complete();
        }
        var rp = (await Admin().GetQueueRuntimePropertiesAsync(q)).Value;
        Assert.Equal(1, rp.DeadLetterMessageCount);
        Assert.Equal(0, rp.ActiveMessageCount);
    }

    [Fact]
    public async Task CrossEntity_ReceiveFromOneQueueAndSendToAnother_CommitsBoth()
    {
        var a = await NewQueue();
        var b = await NewQueue();
        await using var c = Client(crossEntity: true);
        await c.CreateSender(a).SendMessageAsync(Msg("move-me"));
        var ra = c.CreateReceiver(a);
        var sb = c.CreateSender(b);
        var m = await Recv(ra);
        using (var ts = Scope())
        {
            await ra.CompleteMessageAsync(m!);
            await sb.SendMessageAsync(Msg("moved"));
            ts.Complete();
        }
        Assert.Equal(0, await Count(a));
        Assert.Equal("moved", (await Recv(c.CreateReceiver(b)))!.Body.ToString());
    }

    [Fact]
    public async Task CrossEntity_ScopeNotCompleted_NeitherQueueChanges()
    {
        var a = await NewQueue(o => o.LockDuration = TimeSpan.FromSeconds(30));
        var b = await NewQueue();
        await using var c = Client(crossEntity: true);
        await c.CreateSender(a).SendMessageAsync(Msg("stay"));
        var ra = c.CreateReceiver(a);
        var sb = c.CreateSender(b);
        var m = await Recv(ra);
        using (var ts = Scope())
        {
            await ra.CompleteMessageAsync(m!);
            await sb.SendMessageAsync(Msg("ghost"));
        }
        Assert.Equal(1, await Count(a));
        Assert.Equal(0, await Count(b));
    }

    [Fact]
    public async Task CrossEntity_SendToTwoQueuesInOneScope_BothAppear()
    {
        var a = await NewQueue();
        var b = await NewQueue();
        await using var c = Client(crossEntity: true);
        var sa = c.CreateSender(a);
        var sb = c.CreateSender(b);
        using (var ts = Scope())
        {
            await sa.SendMessageAsync(Msg("to-a"));
            await sb.SendMessageAsync(Msg("to-b"));
            ts.Complete();
        }
        Assert.Equal(1, await Count(a));
        Assert.Equal(1, await Count(b));
    }

    [Fact]
    public async Task CrossEntity_ReceiveFromQueueAndPublishToTopic_Commits()
    {
        var q = await NewQueue();
        var adm = Admin();
        var t = Names.Unique("t");
        await adm.CreateTopicAsync(t);
        await adm.CreateSubscriptionAsync(t, "s");
        await using var c = Client(crossEntity: true);
        await c.CreateSender(q).SendMessageAsync(Msg("in"));
        var r = c.CreateReceiver(q);
        var st = c.CreateSender(t);
        var m = await Recv(r);
        using (var ts = Scope())
        {
            await r.CompleteMessageAsync(m!);
            await st.SendMessageAsync(Msg("published"));
            ts.Complete();
        }
        Assert.Equal("published", (await Recv(c.CreateReceiver(t, "s")))!.Body.ToString());
        Assert.Equal(0, await Count(q));
    }

    [Fact]
    public async Task CrossEntity_OverWebSockets_Commits()
    {
        var a = await NewQueue();
        var b = await NewQueue();
        await using var c = Client(ServiceBusTransportType.AmqpWebSockets, crossEntity: true);
        var sa = c.CreateSender(a);
        var sb = c.CreateSender(b);
        using (var ts = Scope())
        {
            await sa.SendMessageAsync(Msg("to-a"));
            await sb.SendMessageAsync(Msg("to-b"));
            ts.Complete();
        }
        Assert.Equal(1, await Count(a));
        Assert.Equal(1, await Count(b));
    }

    [Fact]
    public async Task TwoSequentialScopes_OneCommitsOneRollsBack()
    {
        var q = await NewQueue();
        await using var c = Client();
        var s = c.CreateSender(q);
        using (var ts = Scope()) { await s.SendMessageAsync(Msg("kept")); ts.Complete(); }
        using (var ts = Scope()) { await s.SendMessageAsync(Msg("dropped")); }
        var got = await c.CreateReceiver(q).ReceiveMessagesAsync(5, TimeSpan.FromSeconds(3));
        Assert.Equal(new[] { "kept" }, got.Select(m => m.Body.ToString()).ToArray());
    }

    [Fact]
    public async Task SuppressedScope_SendsOutsideTheTransaction()
    {
        var q = await NewQueue();
        await using var c = Client();
        var s = c.CreateSender(q);
        using (var outer = Scope())
        {
            using (var sup = new TransactionScope(TransactionScopeOption.Suppress, TransactionScopeAsyncFlowOption.Enabled))
            {
                await s.SendMessageAsync(Msg("immediate"));
                sup.Complete();
            }
            Assert.Equal(1, await Count(q));
        }
    }

    [Fact]
    public async Task Schedule_InsideACompletedScope_ReturnsSequenceNumberAndDeliversAtTheTime()
    {
        var q = await NewQueue();
        await using var c = Client();
        var s = c.CreateSender(q);
        long seq;
        using (var ts = Scope())
        {
            seq = await s.ScheduleMessageAsync(Msg("sched"), DateTimeOffset.UtcNow.AddSeconds(3));
            ts.Complete();
        }
        Assert.True(seq > 0);
        Assert.Equal("sched", (await Recv(c.CreateReceiver(q), 15))!.Body.ToString());
    }

    [Fact]
    public async Task CrossEntity_OnTheWire_TheSdkDoesNotSendAViaLinkProperty_SoTheViaRuleCannotBeTestedFromDotNet()
    {
        // WireTap records what Azure.Messaging.ServiceBus 7.21.0 sends. With EnableCrossEntityTransactions the sender link to the second queue is attached
        // straight to that queue (address only, no com.microsoft:transfer-destination-address), the same as the Java SDK does. The emulator accepts it.
        var a = await NewQueue();
        var b = await NewQueue();
        var wireFile = Path.Combine(Emu.TestRoot, "logs", "sb-via.wire");
        using (var tap = new WireTap(Emu.Port, "sb-via.wire"))
        {
            var cs = $"Endpoint=sb://{Ns}.servicebus.localhost:{tap.Port};SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true";
            await using var c = new ServiceBusClient(cs, new ServiceBusClientOptions { EnableCrossEntityTransactions = true });
            await c.CreateSender(a).SendMessageAsync(Msg("in"));
            var ra = c.CreateReceiver(a);
            var sb = c.CreateSender(b);
            var m = await Recv(ra);
            using (var ts = Scope())
            {
                await ra.CompleteMessageAsync(m!);
                await sb.SendMessageAsync(Msg("out"));
                ts.Complete();
            }
            Assert.Equal("out", (await Recv(c.CreateReceiver(b)))!.Body.ToString());
            await Task.Delay(300);
        }
        var wire = File.ReadAllText(wireFile);
        Assert.DoesNotContain("transfer-destination-address", wire);
        Assert.Contains(b, wire);
    }

    [Fact]
    public async Task CrossEntity_WithoutEnableCrossEntityTransactions_IsRefused()
    {
        var a = await NewQueue();
        var b = await NewQueue();
        await using var c = Client(crossEntity: false);
        await c.CreateSender(a).SendMessageAsync(Msg("in"));
        var ra = c.CreateReceiver(a);
        var sb = c.CreateSender(b);
        var m = await Recv(ra);
        await Assert.ThrowsAnyAsync<Exception>(async () =>
        {
            using var ts = Scope();
            await ra.CompleteMessageAsync(m!);
            await sb.SendMessageAsync(Msg("out"));
            ts.Complete();
        });
    }
}
