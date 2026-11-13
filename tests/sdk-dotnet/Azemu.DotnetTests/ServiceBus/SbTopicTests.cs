using Azure.Messaging.ServiceBus;
using Azure.Messaging.ServiceBus.Administration;

namespace Azemu.DotnetTests.ServiceBus;

public class SbTopicTests : SbBase, IClassFixture<SbFixture>
{
    public SbTopicTests(SbFixture f) : base(f) { }

    async Task<string> Topic(params (string sub, RuleFilter? filter, RuleAction? action)[] subs)
    {
        var a = Admin();
        var t = Names.Unique("t");
        await a.CreateTopicAsync(t);
        foreach (var (sub, filter, action) in subs)
        {
            if (filter == null) await a.CreateSubscriptionAsync(t, sub);
            else await a.CreateSubscriptionAsync(new CreateSubscriptionOptions(t, sub), new CreateRuleOptions("r", filter) { Action = action });
        }
        return t;
    }

    static async Task<List<T>> ToList<T>(IAsyncEnumerable<T> e) { var l = new List<T>(); await foreach (var x in e) l.Add(x); return l; }

    static ServiceBusMessage M(string body, string? subject = null, params (string k, object v)[] props)
    {
        var m = new ServiceBusMessage(body) { Subject = subject };
        foreach (var (k, v) in props) m.ApplicationProperties[k] = v;
        return m;
    }

    async Task<List<ServiceBusReceivedMessage>> Drain(ServiceBusClient c, string t, string sub, int secs = 2)
    {
        var r = c.CreateReceiver(t, sub);
        var list = new List<ServiceBusReceivedMessage>();
        while (true)
        {
            var more = await r.ReceiveMessagesAsync(50, TimeSpan.FromSeconds(secs));
            if (more.Count == 0) break;
            foreach (var m in more) await r.CompleteMessageAsync(m);
            list.AddRange(more);
        }
        return list;
    }

    [Fact]
    public async Task Topic_DefaultRule_DeliversEveryMessageToEverySubscription()
    {
        var t = await Topic(("a", null, null), ("b", null, null));
        await using var c = Client();
        await c.CreateSender(t).SendMessagesAsync(new[] { M("1"), M("2") });
        Assert.Equal(new[] { "1", "2" }, (await Drain(c, t, "a")).Select(m => m.Body.ToString()).ToArray());
        Assert.Equal(new[] { "1", "2" }, (await Drain(c, t, "b")).Select(m => m.Body.ToString()).ToArray());
    }

    [Fact]
    public async Task SqlFilter_OnApplicationProperties_RoutesMatchingMessagesOnly()
    {
        var t = await Topic(("red", new SqlRuleFilter("color = 'red' AND qty > 5"), null), ("all", null, null));
        await using var c = Client();
        await c.CreateSender(t).SendMessagesAsync(new[]
        {
            M("hit", null, ("color", "red"), ("qty", 10)),
            M("wrong-color", null, ("color", "blue"), ("qty", 10)),
            M("too-few", null, ("color", "red"), ("qty", 1)),
        });
        var got = await Drain(c, t, "red");
        Assert.Equal(new[] { "hit" }, got.Select(m => m.Body.ToString()).ToArray());
        Assert.Equal(3, (await Drain(c, t, "all")).Count);
    }

    [Fact]
    public async Task SqlFilter_OnSystemProperties_UsesTheSysPrefix()
    {
        var t = await Topic(("s", new SqlRuleFilter("sys.Label = 'orders' AND sys.To LIKE 'x%'"), null));
        await using var c = Client();
        var yes = M("yes", "orders"); yes.To = "xyz";
        var no = M("no", "orders"); no.To = "abc";
        await c.CreateSender(t).SendMessagesAsync(new[] { yes, no });
        Assert.Equal(new[] { "yes" }, (await Drain(c, t, "s")).Select(m => m.Body.ToString()).ToArray());
    }

    [Fact]
    public async Task SqlFilter_WithParameters_Matches()
    {
        var f = new SqlRuleFilter("n >= @min AND tag IN ('a','b')");
        f.Parameters["@min"] = 3;
        var t = await Topic(("p", f, null));
        await using var c = Client();
        await c.CreateSender(t).SendMessagesAsync(new[] { M("ok", null, ("n", 3), ("tag", "a")), M("low", null, ("n", 2), ("tag", "a")), M("badtag", null, ("n", 9), ("tag", "z")) });
        Assert.Equal(new[] { "ok" }, (await Drain(c, t, "p")).Select(m => m.Body.ToString()).ToArray());
    }

    [Fact]
    public async Task CorrelationFilter_MatchesSubjectCorrelationIdAndProperties()
    {
        var cf = new CorrelationRuleFilter { Subject = "s1", CorrelationId = "c1" };
        cf.ApplicationProperties["k"] = "v";
        var t = await Topic(("c", cf, null));
        await using var c = Client();
        var hit = M("hit", "s1", ("k", "v")); hit.CorrelationId = "c1";
        var miss1 = M("miss-subject", "other", ("k", "v")); miss1.CorrelationId = "c1";
        var miss2 = M("miss-prop", "s1", ("k", "x")); miss2.CorrelationId = "c1";
        await c.CreateSender(t).SendMessagesAsync(new[] { hit, miss1, miss2 });
        Assert.Equal(new[] { "hit" }, (await Drain(c, t, "c")).Select(m => m.Body.ToString()).ToArray());
    }

    [Fact]
    public async Task FalseFilter_DeliversNothing()
    {
        var t = await Topic(("f", new FalseRuleFilter(), null));
        await using var c = Client();
        await c.CreateSender(t).SendMessageAsync(M("x"));
        Assert.Empty(await Drain(c, t, "f"));
    }

    [Fact]
    public async Task SqlRuleAction_SetsApplicationProperties()
    {
        var t = await Topic(("a", new TrueRuleFilter(), new SqlRuleAction("SET processed = 'yes'; SET n = 7")));
        await using var c = Client();
        await c.CreateSender(t).SendMessageAsync(M("x"));
        var m = (await Drain(c, t, "a")).Single();
        Assert.Equal("yes", m.ApplicationProperties["processed"]);
        Assert.Equal(7L, m.ApplicationProperties["n"]);   // a SQL integer literal is an Int64 on Service Bus
    }

    [Fact]
    public async Task RuleManager_AddGetRemove_ChangesRoutingOverAmqp()
    {
        var t = await Topic(("s", new FalseRuleFilter(), null));
        await using var c = Client();
        await using var rm = c.CreateRuleManager(t, "s");
        var before = await ToList(rm.GetRulesAsync());
        Assert.Equal("r", Assert.Single(before).Name);
        Assert.IsType<FalseRuleFilter>(before[0].Filter);

        await rm.CreateRuleAsync(new CreateRuleOptions("only-blue", new SqlRuleFilter("color = 'blue'")) { Action = new SqlRuleAction("SET seen = 1") });
        await rm.CreateRuleAsync("corr", new CorrelationRuleFilter { Subject = "special" });
        var rules = await ToList(rm.GetRulesAsync());
        Assert.Equal(new[] { "corr", "only-blue", "r" }, rules.Select(x => x.Name).OrderBy(x => x).ToArray());
        var sql = Assert.IsType<SqlRuleFilter>(rules.Single(x => x.Name == "only-blue").Filter);
        Assert.Equal("color = 'blue'", sql.SqlExpression);
        Assert.Equal("SET seen = 1", Assert.IsType<SqlRuleAction>(rules.Single(x => x.Name == "only-blue").Action).SqlExpression);
        Assert.Equal("special", Assert.IsType<CorrelationRuleFilter>(rules.Single(x => x.Name == "corr").Filter).Subject);

        await c.CreateSender(t).SendMessagesAsync(new[] { M("blue", null, ("color", "blue")), M("red", null, ("color", "red")), M("sp", "special") });
        var got = (await Drain(c, t, "s")).Select(m => m.Body.ToString()).OrderBy(x => x).ToArray();
        Assert.Equal(new[] { "blue", "sp" }, got);

        await rm.DeleteRuleAsync("only-blue");
        await rm.DeleteRuleAsync("corr");
        await c.CreateSender(t).SendMessageAsync(M("blue2", null, ("color", "blue")));
        Assert.Empty(await Drain(c, t, "s"));
    }

    [Fact]
    public async Task RuleManager_AddingARuleThatExists_FailsWithEntityAlreadyExists()
    {
        var t = await Topic(("s", new FalseRuleFilter(), null));
        await using var c = Client();
        await using var rm = c.CreateRuleManager(t, "s");
        var ex = await Assert.ThrowsAsync<ServiceBusException>(async () => await rm.CreateRuleAsync("r", new TrueRuleFilter()));
        Assert.Equal(ServiceBusFailureReason.MessagingEntityAlreadyExists, ex.Reason);
    }

    [Fact]
    public async Task RuleManager_RemovingAMissingRule_FailsWithNotFound()
    {
        var t = await Topic(("s", null, null));
        await using var c = Client();
        await using var rm = c.CreateRuleManager(t, "s");
        var ex = await Assert.ThrowsAsync<ServiceBusException>(async () => await rm.DeleteRuleAsync("nope"));
        Assert.Equal(ServiceBusFailureReason.MessagingEntityNotFound, ex.Reason);
    }

    [Fact]
    public async Task Subscription_DeadLetterQueue_HoldsMessagesDeadLetteredFromTheSubscription()
    {
        var t = await Topic(("s", null, null));
        await using var c = Client();
        await c.CreateSender(t).SendMessageAsync(M("bad"));
        var r = c.CreateReceiver(t, "s");
        await r.DeadLetterMessageAsync((await Recv(r))!, "why", "because");
        var d = await Recv(c.CreateReceiver(t, "s", new ServiceBusReceiverOptions { SubQueue = SubQueue.DeadLetter }));
        Assert.Equal("bad", d!.Body.ToString());
        Assert.Equal("why", d.DeadLetterReason);
        Assert.Equal("because", d.DeadLetterErrorDescription);
    }

    [Fact]
    public async Task Subscription_MaxDeliveryCount_DeadLettersAfterThatManyDeliveries()
    {
        var a = Admin();
        var t = Names.Unique("t");
        await a.CreateTopicAsync(t);
        await a.CreateSubscriptionAsync(new CreateSubscriptionOptions(t, "s") { MaxDeliveryCount = 2 });
        await using var c = Client();
        await c.CreateSender(t).SendMessageAsync(M("p"));
        var r = c.CreateReceiver(t, "s");
        for (int i = 0; i < 2; i++) await r.AbandonMessageAsync((await Recv(r))!);
        Assert.Null(await r.ReceiveMessageAsync(TimeSpan.FromSeconds(1)));
        var d = await Recv(c.CreateReceiver(t, "s", new ServiceBusReceiverOptions { SubQueue = SubQueue.DeadLetter }));
        Assert.Equal("MaxDeliveryCountExceeded", d!.DeadLetterReason);
    }

    [Fact]
    public async Task ForwardTo_FromASubscription_DeliversToAQueue()
    {
        var a = Admin();
        var target = await NewQueue();
        var t = Names.Unique("t");
        await a.CreateTopicAsync(t);
        await a.CreateSubscriptionAsync(new CreateSubscriptionOptions(t, "s") { ForwardTo = target });
        await using var c = Client();
        await c.CreateSender(t).SendMessageAsync(M("fwd"));
        Assert.Equal("fwd", (await Recv(c.CreateReceiver(target)))!.Body.ToString());
    }

    [Fact]
    public async Task DeadLetterForwarding_MovesDeadLetteredMessagesAndSetsDeadLetterSource()
    {
        var target = await NewQueue();
        var src = await NewQueue(o => o.ForwardDeadLetteredMessagesTo = target);
        await using var c = Client();
        await c.CreateSender(src).SendMessageAsync(M("dl"));
        var r = c.CreateReceiver(src);
        await r.DeadLetterMessageAsync((await Recv(r))!, "r1");
        var m = await Recv(c.CreateReceiver(target));
        Assert.Equal("dl", m!.Body.ToString());
        Assert.Equal("r1", m.DeadLetterReason);
        Assert.Equal(src, m.DeadLetterSource, ignoreCase: true);
    }

    [Fact]
    public async Task ScheduledMessage_ToATopic_ReachesSubscriptionsWhenDue()
    {
        var t = await Topic(("s", null, null));
        await using var c = Client();
        await c.CreateSender(t).ScheduleMessageAsync(M("later"), DateTimeOffset.UtcNow.AddSeconds(3));
        var r = c.CreateReceiver(t, "s");
        Assert.Null(await r.ReceiveMessageAsync(TimeSpan.FromSeconds(1)));
        Assert.Equal("later", (await Recv(r, 15))!.Body.ToString());
    }

    [Fact]
    public async Task WebSocketTransport_Topic_AndRuleManager()
    {
        var t = await Topic(("s", new FalseRuleFilter(), null));
        await using var c = Client(ServiceBusTransportType.AmqpWebSockets);
        await using var rm = c.CreateRuleManager(t, "s");
        await rm.CreateRuleAsync("blue", new SqlRuleFilter("color = 'blue'"));
        await c.CreateSender(t).SendMessageAsync(M("b", null, ("color", "blue")));
        Assert.Equal("b", (await Recv(c.CreateReceiver(t, "s")))!.Body.ToString());
    }
}
