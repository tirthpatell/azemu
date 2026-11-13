using Azure.Messaging.ServiceBus;
using Azure.Messaging.ServiceBus.Administration;

namespace Azemu.DotnetTests.ServiceBus;

public class SbMessagingTests : SbBase, IClassFixture<SbFixture>
{
    public SbMessagingTests(SbFixture f) : base(f) { }

    async Task<(ServiceBusClient c, string q)> Setup(Action<CreateQueueOptions>? cfg = null)
    {
        var q = await NewQueue(cfg);
        return (Client(), q);
    }

    [Fact]
    public async Task Send_AllSystemProperties_RoundTrip()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        var m = new ServiceBusMessage(BinaryData.FromBytes(new byte[] { 1, 2, 3, 0, 255 }))
        {
            MessageId = "id-1", Subject = "subj", ContentType = "application/x-bin", CorrelationId = "corr",
            To = "dest", ReplyTo = "reply", TimeToLive = TimeSpan.FromMinutes(30),
        };
        await c.CreateSender(q).SendMessageAsync(m);
        var r = await Recv(c.CreateReceiver(q));
        Assert.NotNull(r);
        Assert.Equal("id-1", r!.MessageId);
        Assert.Equal("subj", r.Subject);
        Assert.Equal("application/x-bin", r.ContentType);
        Assert.Equal("corr", r.CorrelationId);
        Assert.Equal("dest", r.To);
        Assert.Equal("reply", r.ReplyTo);
        Assert.Equal(TimeSpan.FromMinutes(30), r.TimeToLive);
        Assert.Equal(new byte[] { 1, 2, 3, 0, 255 }, r.Body.ToArray());
        Assert.True(r.SequenceNumber > 0);
        Assert.True(r.EnqueuedTime > DateTimeOffset.UtcNow.AddMinutes(-5));
        Assert.Equal(1, r.DeliveryCount);
    }

    [Fact]
    public async Task Send_ApplicationProperties_KeepTheirTypes()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        var guid = Guid.NewGuid();
        var when = new DateTimeOffset(2024, 5, 6, 7, 8, 9, TimeSpan.Zero);
        var m = Msg("p");
        m.ApplicationProperties["s"] = "text";
        m.ApplicationProperties["i"] = 42;
        m.ApplicationProperties["l"] = 1L << 40;
        m.ApplicationProperties["b"] = true;
        m.ApplicationProperties["d"] = 3.5d;
        m.ApplicationProperties["f"] = 1.25f;
        m.ApplicationProperties["g"] = guid;
        m.ApplicationProperties["by"] = (byte)200;
        m.ApplicationProperties["sh"] = (short)-3;
        m.ApplicationProperties["t"] = when;
        m.ApplicationProperties["ts"] = TimeSpan.FromSeconds(90);
        m.ApplicationProperties["u"] = new Uri("http://example.com/x");
        await c.CreateSender(q).SendMessageAsync(m);
        var r = await Recv(c.CreateReceiver(q));
        var p = r!.ApplicationProperties;
        Assert.Equal("text", p["s"]);
        Assert.Equal(42, p["i"]);
        Assert.Equal(1L << 40, p["l"]);
        Assert.Equal(true, p["b"]);
        Assert.Equal(3.5d, p["d"]);
        Assert.Equal(1.25f, p["f"]);
        Assert.Equal(guid, p["g"]);
        Assert.Equal((byte)200, p["by"]);
        Assert.Equal((short)-3, p["sh"]);
        Assert.Equal(when, p["t"]);
        Assert.Equal(TimeSpan.FromSeconds(90), p["ts"]);
        Assert.Equal(new Uri("http://example.com/x"), p["u"]);
    }

    [Fact]
    public async Task Send_LargeBody_RoundTrips()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        var body = new byte[200 * 1024];
        new Random(1).NextBytes(body);
        await c.CreateSender(q).SendMessageAsync(new ServiceBusMessage(BinaryData.FromBytes(body)));
        var r = await Recv(c.CreateReceiver(q));
        Assert.Equal(body, r!.Body.ToArray());
    }

    [Fact]
    public async Task Send_BodyOverTheEntityLimit_FailsWithMessageSizeExceeded()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        var body = new byte[300 * 1024];
        var ex = await Assert.ThrowsAsync<ServiceBusException>(async () =>
            await c.CreateSender(q).SendMessageAsync(new ServiceBusMessage(BinaryData.FromBytes(body))));
        Assert.Equal(ServiceBusFailureReason.MessageSizeExceeded, ex.Reason);
    }

    [Fact]
    public async Task Complete_RemovesTheMessage()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        await c.CreateSender(q).SendMessageAsync(Msg());
        var r = c.CreateReceiver(q);
        var m = await Recv(r);
        await r.CompleteMessageAsync(m!);
        Assert.Null(await r.ReceiveMessageAsync(TimeSpan.FromSeconds(1)));
        var rp = (await Admin().GetQueueRuntimePropertiesAsync(q)).Value;
        Assert.Equal(0, rp.TotalMessageCount);
    }

    [Fact]
    public async Task Abandon_MakesTheMessageAvailableWithDeliveryCountTwo()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        await c.CreateSender(q).SendMessageAsync(Msg());
        var r = c.CreateReceiver(q);
        var m = await Recv(r);
        Assert.Equal(1, m!.DeliveryCount);
        await r.AbandonMessageAsync(m);
        var m2 = await Recv(r);
        Assert.Equal(2, m2!.DeliveryCount);
        Assert.Equal(m.SequenceNumber, m2.SequenceNumber);
    }

    [Fact]
    public async Task Abandon_WithPropertiesToModify_ChangesApplicationProperties()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        var msg = Msg();
        msg.ApplicationProperties["keep"] = "k";
        await c.CreateSender(q).SendMessageAsync(msg);
        var r = c.CreateReceiver(q);
        var m = await Recv(r);
        await r.AbandonMessageAsync(m!, new Dictionary<string, object> { ["added"] = "yes", ["keep"] = "changed" });
        var m2 = await Recv(r);
        Assert.Equal("yes", m2!.ApplicationProperties["added"]);
        Assert.Equal("changed", m2.ApplicationProperties["keep"]);
    }

    [Fact]
    public async Task Defer_ThenReceiveDeferredBySequenceNumber()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        await c.CreateSender(q).SendMessageAsync(Msg("later"));
        var r = c.CreateReceiver(q);
        var m = await Recv(r);
        await r.DeferMessageAsync(m!, new Dictionary<string, object> { ["why"] = "busy" });
        Assert.Null(await r.ReceiveMessageAsync(TimeSpan.FromSeconds(1)));
        var d = await r.ReceiveDeferredMessageAsync(m!.SequenceNumber);
        Assert.Equal("later", d.Body.ToString());
        Assert.Equal(ServiceBusMessageState.Deferred, d.State);
        Assert.Equal("busy", d.ApplicationProperties["why"]);
        await r.CompleteMessageAsync(d);
        Assert.Equal(0, (await Admin().GetQueueRuntimePropertiesAsync(q)).Value.TotalMessageCount);
    }

    [Fact]
    public async Task ReceiveDeferredMessages_ForUnknownSequenceNumber_FailsWithNotFound()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        var ex = await Assert.ThrowsAsync<ServiceBusException>(async () => await c.CreateReceiver(q).ReceiveDeferredMessageAsync(99999));
        Assert.Equal(ServiceBusFailureReason.MessageNotFound, ex.Reason);
    }

    [Fact]
    public async Task DeadLetter_WithReasonAndProperties_AppearsInTheDeadLetterQueue()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        await c.CreateSender(q).SendMessageAsync(Msg("bad"));
        var r = c.CreateReceiver(q);
        var m = await Recv(r);
        await r.DeadLetterMessageAsync(m!, new Dictionary<string, object> { ["extra"] = 5 }, "my-reason", "my-description");
        Assert.Null(await r.ReceiveMessageAsync(TimeSpan.FromSeconds(1)));
        var dlq = c.CreateReceiver(q, new ServiceBusReceiverOptions { SubQueue = SubQueue.DeadLetter });
        var d = await Recv(dlq);
        Assert.Equal("bad", d!.Body.ToString());
        Assert.Equal("my-reason", d.DeadLetterReason);
        Assert.Equal("my-description", d.DeadLetterErrorDescription);
        Assert.Equal(5, d.ApplicationProperties["extra"]);
        // DeadLetterSource is documented as set only when a dead-lettered message was auto-forwarded: not asserted here
    }

    [Fact]
    public async Task DeadLetter_ShowsInRuntimePropertiesAsDeadLetterCount()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        await c.CreateSender(q).SendMessageAsync(Msg());
        var r = c.CreateReceiver(q);
        await r.DeadLetterMessageAsync((await Recv(r))!, "x");
        var rp = (await Admin().GetQueueRuntimePropertiesAsync(q)).Value;
        Assert.Equal(1, rp.DeadLetterMessageCount);
        Assert.Equal(0, rp.ActiveMessageCount);
    }

    [Fact]
    public async Task MaxDeliveryCount_ExceededByAbandons_DeadLettersWithMaxDeliveryCountExceeded()
    {
        var (c, q) = await Setup(o => o.MaxDeliveryCount = 3);
        await using var _ = c;
        await c.CreateSender(q).SendMessageAsync(Msg("poison"));
        var r = c.CreateReceiver(q);
        for (int i = 1; i <= 3; i++)
        {
            var m = await Recv(r);
            Assert.NotNull(m);
            Assert.Equal(i, m!.DeliveryCount);
            await r.AbandonMessageAsync(m);
        }
        Assert.Null(await r.ReceiveMessageAsync(TimeSpan.FromSeconds(1)));
        var d = await Recv(c.CreateReceiver(q, new ServiceBusReceiverOptions { SubQueue = SubQueue.DeadLetter }));
        Assert.Equal("poison", d!.Body.ToString());
        Assert.Equal("MaxDeliveryCountExceeded", d.DeadLetterReason);
    }

    [Fact]
    public async Task ReceiveAndDelete_RemovesTheMessageAtOnce()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        await c.CreateSender(q).SendMessageAsync(Msg("x"));
        var r = c.CreateReceiver(q, new ServiceBusReceiverOptions { ReceiveMode = ServiceBusReceiveMode.ReceiveAndDelete });
        var m = await Recv(r);
        Assert.Equal("x", m!.Body.ToString());
        Assert.Equal(0, (await Admin().GetQueueRuntimePropertiesAsync(q)).Value.TotalMessageCount);
    }

    [Fact]
    public async Task LockExpiry_RedeliversWithDeliveryCountTwo()
    {
        var (c, q) = await Setup(o => o.LockDuration = TimeSpan.FromSeconds(5));
        await using var _ = c;
        await c.CreateSender(q).SendMessageAsync(Msg());
        var r = c.CreateReceiver(q);
        var m = await Recv(r);
        Assert.Equal(1, m!.DeliveryCount);
        var m2 = await Recv(r, 15);
        Assert.NotNull(m2);
        Assert.Equal(2, m2!.DeliveryCount);
    }

    [Fact]
    public async Task CompleteAfterLockExpired_FailsWithMessageLockLost()
    {
        var (c, q) = await Setup(o => o.LockDuration = TimeSpan.FromSeconds(5));
        await using var _ = c;
        await c.CreateSender(q).SendMessageAsync(Msg());
        var r = c.CreateReceiver(q);
        var m = await Recv(r);
        await Task.Delay(TimeSpan.FromSeconds(7));
        var ex = await Assert.ThrowsAsync<ServiceBusException>(async () => await r.CompleteMessageAsync(m!));
        Assert.Equal(ServiceBusFailureReason.MessageLockLost, ex.Reason);
    }

    [Fact]
    public async Task RenewMessageLock_ExtendsLockedUntil()
    {
        var (c, q) = await Setup(o => o.LockDuration = TimeSpan.FromSeconds(10));
        await using var _ = c;
        await c.CreateSender(q).SendMessageAsync(Msg());
        var r = c.CreateReceiver(q);
        var m = await Recv(r);
        var before = m!.LockedUntil;
        await Task.Delay(1500);
        await r.RenewMessageLockAsync(m);
        Assert.True(m.LockedUntil > before, $"{m.LockedUntil} vs {before}");
        await r.CompleteMessageAsync(m);
    }

    [Fact]
    public async Task Peek_DoesNotLockAndReturnsMessagesInOrder()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        await c.CreateSender(q).SendMessagesAsync(new[] { Msg("1"), Msg("2"), Msg("3") });
        var r = c.CreateReceiver(q);
        var peeked = await r.PeekMessagesAsync(10);
        Assert.Equal(new[] { "1", "2", "3" }, peeked.Select(p => p.Body.ToString()).ToArray());
        Assert.All(peeked, p => Assert.Equal(0, p.DeliveryCount));
        // the SDK's peek cursor moves on: the next peek starts after the last one it returned
        Assert.Empty(await r.PeekMessagesAsync(10));
        var again = await r.PeekMessagesAsync(10, peeked[0].SequenceNumber);
        Assert.Equal(3, again.Count);
        var received = await r.ReceiveMessagesAsync(3, TimeSpan.FromSeconds(5));
        Assert.Equal(3, received.Count);
    }

    [Fact]
    public async Task Peek_FromSequenceNumber_SkipsEarlierMessages()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        await c.CreateSender(q).SendMessagesAsync(new[] { Msg("1"), Msg("2"), Msg("3") });
        var r = c.CreateReceiver(q);
        var all = await r.PeekMessagesAsync(10);
        var from = await r.PeekMessagesAsync(10, all[1].SequenceNumber);
        Assert.Equal(new[] { "2", "3" }, from.Select(p => p.Body.ToString()).ToArray());
    }

    [Fact]
    public async Task ScheduleMessage_IsNotDeliveredBeforeItsTime()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        var s = c.CreateSender(q);
        var seq = await s.ScheduleMessageAsync(Msg("later"), DateTimeOffset.UtcNow.AddSeconds(4));
        Assert.True(seq > 0);
        var r = c.CreateReceiver(q);
        Assert.Null(await r.ReceiveMessageAsync(TimeSpan.FromSeconds(1)));
        var rp = (await Admin().GetQueueRuntimePropertiesAsync(q)).Value;
        Assert.Equal(1, rp.ScheduledMessageCount);
        var m = await Recv(r, 15);
        Assert.Equal("later", m!.Body.ToString());
        Assert.True(m.EnqueuedTime >= DateTimeOffset.UtcNow.AddSeconds(-30));
    }

    [Fact]
    public async Task CancelScheduledMessage_PreventsDelivery()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        var s = c.CreateSender(q);
        var seq = await s.ScheduleMessageAsync(Msg("never"), DateTimeOffset.UtcNow.AddSeconds(3));
        await s.CancelScheduledMessageAsync(seq);
        Assert.Null(await c.CreateReceiver(q).ReceiveMessageAsync(TimeSpan.FromSeconds(6)));
    }

    [Fact]
    public async Task ScheduleMessages_Multiple_ReturnsOneSequenceNumberEach()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        var seqs = await c.CreateSender(q).ScheduleMessagesAsync(new[] { Msg("a"), Msg("b"), Msg("c") }, DateTimeOffset.UtcNow.AddMinutes(5));
        Assert.Equal(3, seqs.Count);
        Assert.Equal(3, seqs.Distinct().Count());
    }

    [Fact]
    public async Task Batch_AddsUntilFull_ThenAllAreReceived()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        var s = c.CreateSender(q);
        using var batch = await s.CreateMessageBatchAsync(new CreateMessageBatchOptions { MaxSizeInBytes = 20 * 1024 });
        int added = 0;
        while (batch.TryAddMessage(new ServiceBusMessage(new byte[1000]))) added++;
        Assert.InRange(added, 15, 25);
        Assert.Equal(added, batch.Count);
        await s.SendMessagesAsync(batch);
        var got = new List<ServiceBusReceivedMessage>();
        var r = c.CreateReceiver(q);
        while (got.Count < added)
        {
            var more = await r.ReceiveMessagesAsync(added, TimeSpan.FromSeconds(5));
            if (more.Count == 0) break;
            got.AddRange(more);
        }
        Assert.Equal(added, got.Count);
    }

    [Fact]
    public async Task ReceiveMessages_ReturnsUpToMax()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        await c.CreateSender(q).SendMessagesAsync(Enumerable.Range(0, 10).Select(i => Msg("m" + i)));
        var r = c.CreateReceiver(q);
        var got = new List<string>();
        while (got.Count < 10)
        {
            var more = await r.ReceiveMessagesAsync(10, TimeSpan.FromSeconds(5));
            Assert.NotEmpty(more);
            got.AddRange(more.Select(m => m.Body.ToString()));
        }
        Assert.Equal(Enumerable.Range(0, 10).Select(i => "m" + i), got);
    }

    [Fact]
    public async Task MessageTimeToLive_Expired_IsNotDelivered()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        var m = Msg(); m.TimeToLive = TimeSpan.FromSeconds(2);
        await c.CreateSender(q).SendMessageAsync(m);
        await Task.Delay(3000);
        Assert.Null(await c.CreateReceiver(q).ReceiveMessageAsync(TimeSpan.FromSeconds(1)));
    }

    [Fact]
    public async Task MessageTimeToLive_Expired_WithDeadLetteringOnExpiration_GoesToTheDeadLetterQueue()
    {
        var (c, q) = await Setup(o => o.DeadLetteringOnMessageExpiration = true);
        await using var _ = c;
        var m = Msg("old"); m.TimeToLive = TimeSpan.FromSeconds(2);
        await c.CreateSender(q).SendMessageAsync(m);
        await Task.Delay(3000);
        var d = await Recv(c.CreateReceiver(q, new ServiceBusReceiverOptions { SubQueue = SubQueue.DeadLetter }), 10);
        Assert.NotNull(d);
        Assert.Equal("TTLExpiredException", d!.DeadLetterReason);
    }

    [Fact]
    public async Task DuplicateDetection_SecondMessageWithSameIdIsDropped()
    {
        var (c, q) = await Setup(o => o.RequiresDuplicateDetection = true);
        await using var _ = c;
        var s = c.CreateSender(q);
        await s.SendMessageAsync(new ServiceBusMessage("one") { MessageId = "dup" });
        await s.SendMessageAsync(new ServiceBusMessage("two") { MessageId = "dup" });
        var r = c.CreateReceiver(q);
        var got = await r.ReceiveMessagesAsync(5, TimeSpan.FromSeconds(3));
        Assert.Single(got);
        Assert.Equal("one", got[0].Body.ToString());
    }

    [Fact]
    public async Task ForwardTo_DeliversToTheTargetQueue()
    {
        var target = await NewQueue();
        var src = await NewQueue(o => o.ForwardTo = target);
        await using var c = Client();
        await c.CreateSender(src).SendMessageAsync(Msg("fwd"));
        var m = await Recv(c.CreateReceiver(target));
        Assert.Equal("fwd", m!.Body.ToString());
    }

    [Fact]
    public async Task TwoReceivers_CompeteWithoutSeeingTheSameMessage()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        await c.CreateSender(q).SendMessagesAsync(Enumerable.Range(0, 6).Select(i => Msg("m" + i)));
        var r1 = c.CreateReceiver(q);
        var r2 = c.CreateReceiver(q);
        var a = await r1.ReceiveMessagesAsync(3, TimeSpan.FromSeconds(5));
        var b = await r2.ReceiveMessagesAsync(3, TimeSpan.FromSeconds(5));
        var ids = a.Concat(b).Select(m => m.MessageId).ToList();
        Assert.Equal(ids.Distinct().Count(), ids.Count);
    }

    [Fact]
    public async Task Receive_FromAnEmptyQueue_ReturnsNullAfterTheWait()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        var sw = System.Diagnostics.Stopwatch.StartNew();
        var m = await c.CreateReceiver(q).ReceiveMessageAsync(TimeSpan.FromSeconds(2));
        Assert.Null(m);
        Assert.InRange(sw.Elapsed.TotalSeconds, 1.5, 6);
    }

    [Fact]
    public async Task Send_ToDisabledQueue_Fails()
    {
        var (c, q) = await Setup();
        await using var _ = c;
        var a = Admin();
        var props = (await a.GetQueueAsync(q)).Value;
        props.Status = EntityStatus.Disabled;
        await a.UpdateQueueAsync(props);
        await Assert.ThrowsAsync<ServiceBusException>(async () => await c.CreateSender(q).SendMessageAsync(Msg()));
    }

    [Fact]
    public async Task WebSocketTransport_SendAndReceive()
    {
        var q = await NewQueue();
        await using var c = Client(ServiceBusTransportType.AmqpWebSockets);
        await c.CreateSender(q).SendMessageAsync(Msg("ws"));
        var m = await Recv(c.CreateReceiver(q));
        Assert.Equal("ws", m!.Body.ToString());
    }

    [Fact]
    public async Task AmqpTcpOverTls_WithACustomEndpointAndAnAnchoredCa_SendsAndReceives()
    {
        // no UseDevelopmentEmulator: the SDK uses TLS, and the custom endpoint carries azemu's port
        var q = await NewQueue();
        var cs = $"Endpoint=sb://{Ns}.servicebus.localhost/;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2VjcmV0";
        var o = new ServiceBusClientOptions
        {
            TransportType = ServiceBusTransportType.AmqpTcp,
            CustomEndpointAddress = new Uri($"sb://{Ns}.servicebus.localhost:{Emu.Port}"),
            CertificateValidationCallback = (s, c, ch, e) => Emu.ValidateCert(s, c, ch, e),
        };
        await using var c = new ServiceBusClient(cs, o);
        await c.CreateSender(q).SendMessageAsync(Msg("tls"));
        Assert.Equal("tls", (await Recv(c.CreateReceiver(q)))!.Body.ToString());
    }

    [Fact]
    public async Task AmqpTcpOverTls_WithoutTrustingTheCa_FailsTheHandshake()
    {
        var q = await NewQueue();
        var cs = $"Endpoint=sb://{Ns}.servicebus.localhost/;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2VjcmV0";
        var o = new ServiceBusClientOptions
        {
            TransportType = ServiceBusTransportType.AmqpTcp,
            CustomEndpointAddress = new Uri($"sb://{Ns}.servicebus.localhost:{Emu.Port}"),
            RetryOptions = new ServiceBusRetryOptions { MaxRetries = 0, TryTimeout = TimeSpan.FromSeconds(10) },
        };
        await using var c = new ServiceBusClient(cs, o);
        await Assert.ThrowsAnyAsync<Exception>(async () => await c.CreateSender(q).SendMessageAsync(Msg("no trust")));
    }
}
