using Azure.Messaging.ServiceBus;
using Azure.Messaging.ServiceBus.Administration;

namespace Azemu.DotnetTests.ServiceBus;

public class SbSessionTests : SbBase, IClassFixture<SbFixture>
{
    public SbSessionTests(SbFixture f) : base(f) { }

    static ServiceBusMessage S(string body, string session) => new(body) { SessionId = session };
    Task<string> SessionQueue(Action<CreateQueueOptions>? cfg = null) => NewQueue(o => { o.RequiresSession = true; cfg?.Invoke(o); });

    [Fact]
    public async Task AcceptSession_ByIdReceivesOnlyThatSessionInOrder()
    {
        var q = await SessionQueue();
        await using var c = Client();
        var s = c.CreateSender(q);
        await s.SendMessagesAsync(new[] { S("a1", "A"), S("b1", "B"), S("a2", "A"), S("b2", "B"), S("a3", "A") });
        await using var r = await c.AcceptSessionAsync(q, "A");
        Assert.Equal("A", r.SessionId);
        var got = new List<string>();
        for (int i = 0; i < 3; i++) { var m = await Recv(r); got.Add(m!.Body.ToString()); Assert.Equal("A", m.SessionId); await r.CompleteMessageAsync(m); }
        Assert.Equal(new[] { "a1", "a2", "a3" }, got);
        Assert.Null(await r.ReceiveMessageAsync(TimeSpan.FromSeconds(1)));
    }

    [Fact]
    public async Task AcceptNextSession_PicksASessionThatHasMessages()
    {
        var q = await SessionQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessagesAsync(new[] { S("x", "only") });
        await using var r = await c.AcceptNextSessionAsync(q);
        Assert.Equal("only", r.SessionId);
        Assert.Equal("x", (await Recv(r))!.Body.ToString());
    }

    [Fact]
    public async Task AcceptNextSession_TwoReceivers_GetDifferentSessions()
    {
        var q = await SessionQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessagesAsync(new[] { S("1", "s1"), S("2", "s2") });
        await using var r1 = await c.AcceptNextSessionAsync(q);
        await using var r2 = await c.AcceptNextSessionAsync(q);
        Assert.NotEqual(r1.SessionId, r2.SessionId);
    }

    [Fact]
    public async Task AcceptNextSession_WithNoSessions_FailsWithServiceTimeout()
    {
        var q = await SessionQueue();
        await using var c = Client(tryTimeoutSeconds: 5);
        var ex = await Assert.ThrowsAsync<ServiceBusException>(async () => await c.AcceptNextSessionAsync(q));
        Assert.Equal(ServiceBusFailureReason.ServiceTimeout, ex.Reason);
    }

    [Fact]
    public async Task AcceptSession_AlreadyLocked_FailsWithSessionCannotBeLocked()
    {
        var q = await SessionQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(S("x", "A"));
        await using var first = await c.AcceptSessionAsync(q, "A");
        var ex = await Assert.ThrowsAsync<ServiceBusException>(async () => await c.AcceptSessionAsync(q, "A"));
        Assert.Equal(ServiceBusFailureReason.SessionCannotBeLocked, ex.Reason);
    }

    [Fact]
    public async Task SessionState_SetThenGet_RoundTrips_AndClears()
    {
        var q = await SessionQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(S("x", "A"));
        await using var r = await c.AcceptSessionAsync(q, "A");
        var initial = await r.GetSessionStateAsync();
        Assert.True(initial == null || initial.ToMemory().Length == 0);
        await r.SetSessionStateAsync(BinaryData.FromBytes(new byte[] { 9, 8, 7 }));
        Assert.Equal(new byte[] { 9, 8, 7 }, (await r.GetSessionStateAsync()).ToArray());
        await r.SetSessionStateAsync(null);
        var cleared = await r.GetSessionStateAsync();
        Assert.True(cleared == null || cleared.ToMemory().Length == 0);
    }

    [Fact]
    public async Task SessionState_SurvivesReopeningTheSession()
    {
        var q = await SessionQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(S("x", "A"));
        var r = await c.AcceptSessionAsync(q, "A");
        await r.SetSessionStateAsync(new BinaryData("saved"));
        await r.CloseAsync();
        await using var again = await c.AcceptSessionAsync(q, "A");
        Assert.Equal("saved", (await again.GetSessionStateAsync()).ToString());
    }

    [Fact]
    public async Task RenewSessionLock_MovesSessionLockedUntilForward()
    {
        var q = await SessionQueue(o => o.LockDuration = TimeSpan.FromSeconds(10));
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(S("x", "A"));
        await using var r = await c.AcceptSessionAsync(q, "A");
        var before = r.SessionLockedUntil;
        await Task.Delay(1500);
        await r.RenewSessionLockAsync();
        Assert.True(r.SessionLockedUntil > before);
    }

    [Fact]
    public async Task SessionLockExpiry_ThenOperationFailsWithSessionLockLost()
    {
        var q = await SessionQueue(o => o.LockDuration = TimeSpan.FromSeconds(5));
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(S("x", "A"));
        await using var r = await c.AcceptSessionAsync(q, "A");
        var m = await Recv(r);
        await Task.Delay(TimeSpan.FromSeconds(7));
        var ex = await Assert.ThrowsAsync<ServiceBusException>(async () => await r.CompleteMessageAsync(m!));
        Assert.Equal(ServiceBusFailureReason.SessionLockLost, ex.Reason);
    }

    [Fact]
    public async Task SendWithoutSessionId_ToSessionQueue_IsRefused()
    {
        var q = await SessionQueue();
        await using var c = Client();
        // Azure refuses it; the SDK turns that answer into an ArgumentException about the SessionId
        var ex = await Assert.ThrowsAsync<ArgumentException>(async () => await c.CreateSender(q).SendMessageAsync(new ServiceBusMessage("no-session")));
        Assert.Contains("SessionId", ex.Message);
    }

    [Fact]
    public async Task Session_AbandonedMessage_IsReceivedAgainInTheSameSession()
    {
        var q = await SessionQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(S("x", "A"));
        await using var r = await c.AcceptSessionAsync(q, "A");
        var m = await Recv(r);
        await r.AbandonMessageAsync(m!);
        var m2 = await Recv(r);
        Assert.Equal(2, m2!.DeliveryCount);
        Assert.Equal("A", m2.SessionId);
    }

    [Fact]
    public async Task Session_DeferThenReceiveDeferred_ThroughTheSessionReceiver()
    {
        var q = await SessionQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(S("x", "A"));
        await using var r = await c.AcceptSessionAsync(q, "A");
        var m = await Recv(r);
        await r.DeferMessageAsync(m!);
        var d = await r.ReceiveDeferredMessageAsync(m!.SequenceNumber);
        Assert.Equal("x", d.Body.ToString());
        await r.CompleteMessageAsync(d);
    }

    [Fact]
    public async Task Session_DeadLetter_GoesToTheDeadLetterQueueWithSessionId()
    {
        var q = await SessionQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(S("x", "A"));
        await using var r = await c.AcceptSessionAsync(q, "A");
        await r.DeadLetterMessageAsync((await Recv(r))!, "r");
        var d = await Recv(c.CreateReceiver(q, new ServiceBusReceiverOptions { SubQueue = SubQueue.DeadLetter }));
        Assert.Equal("A", d!.SessionId);
        Assert.Equal("r", d.DeadLetterReason);
    }

    [Fact]
    public async Task SessionProcessor_HandlesEverySession_AndCompletesMessages()
    {
        var q = await SessionQueue();
        await using var c = Client();
        var all = new List<ServiceBusMessage>();
        for (int s = 0; s < 3; s++) for (int i = 0; i < 4; i++) all.Add(S($"{s}-{i}", "sess" + s));
        await c.CreateSender(q).SendMessagesAsync(all);
        var seen = new System.Collections.Concurrent.ConcurrentDictionary<string, List<string>>();
        var errors = new List<Exception>();
        await using var p = c.CreateSessionProcessor(q, new ServiceBusSessionProcessorOptions { MaxConcurrentSessions = 2, SessionIdleTimeout = TimeSpan.FromSeconds(2) });
        p.ProcessMessageAsync += a => { seen.GetOrAdd(a.SessionId, _ => new()).Add(a.Message.Body.ToString()); return Task.CompletedTask; };
        p.ProcessErrorAsync += a => { lock (errors) errors.Add(a.Exception); return Task.CompletedTask; };
        await p.StartProcessingAsync();
        Assert.True(await Wait.Until(() => seen.Values.Sum(v => v.Count) == 12, 40));
        await p.StopProcessingAsync();
        for (int s = 0; s < 3; s++) Assert.Equal(Enumerable.Range(0, 4).Select(i => $"{s}-{i}"), seen["sess" + s]);
        Assert.Equal(0, (await Admin().GetQueueRuntimePropertiesAsync(q)).Value.TotalMessageCount);
        Assert.Empty(errors.Where(e => e is not ServiceBusException { Reason: ServiceBusFailureReason.ServiceTimeout }));
    }

    [Fact]
    public async Task SessionProcessor_CanStoreSessionStateFromTheHandler()
    {
        var q = await SessionQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(S("x", "A"));
        var done = new TaskCompletionSource();
        await using var p = c.CreateSessionProcessor(q, new ServiceBusSessionProcessorOptions { MaxConcurrentSessions = 1, SessionIdleTimeout = TimeSpan.FromSeconds(2) });
        p.ProcessMessageAsync += async a => { await a.SetSessionStateAsync(new BinaryData("count=1")); done.TrySetResult(); };
        p.ProcessErrorAsync += _ => Task.CompletedTask;
        await p.StartProcessingAsync();
        await done.Task.WaitAsync(TimeSpan.FromSeconds(30));
        await p.StopProcessingAsync();
        // the SDK keeps the processor's session link open after StopProcessingAsync until the client is closed
        await c.DisposeAsync();
        await using var c2 = Client();
        await using var r = await c2.AcceptSessionAsync(q, "A");
        Assert.Equal("count=1", (await r.GetSessionStateAsync()).ToString());
    }

    [Fact]
    public async Task Session_OnASubscription_Works()
    {
        var a = Admin();
        var t = Names.Unique("t");
        await a.CreateTopicAsync(t);
        await a.CreateSubscriptionAsync(new CreateSubscriptionOptions(t, "s") { RequiresSession = true });
        await using var c = Client();
        await c.CreateSender(t).SendMessagesAsync(new[] { S("1", "A"), S("2", "A") });
        await using var r = await c.AcceptSessionAsync(t, "s", "A");
        Assert.Equal("1", (await Recv(r))!.Body.ToString());
    }

    [Fact]
    public async Task WebSocketTransport_Session()
    {
        var q = await SessionQueue();
        await using var c = Client(ServiceBusTransportType.AmqpWebSockets);
        await c.CreateSender(q).SendMessageAsync(S("ws", "A"));
        await using var r = await c.AcceptNextSessionAsync(q);
        Assert.Equal("ws", (await Recv(r))!.Body.ToString());
    }
}
