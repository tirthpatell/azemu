using Azure;
using Azure.Messaging.ServiceBus;
using Azure.Messaging.ServiceBus.Administration;

namespace Azemu.DotnetTests.ServiceBus;

public class SbAdminTests : SbBase, IClassFixture<SbFixture>
{
    public SbAdminTests(SbFixture f) : base(f) { }

    [Fact]
    public async Task CreateQueue_ReturnsTheOptionsThatWereSet()
    {
        var name = Names.Unique("q");
        var r = await Admin().CreateQueueAsync(new CreateQueueOptions(name)
        {
            MaxDeliveryCount = 7,
            LockDuration = TimeSpan.FromSeconds(45),
            DefaultMessageTimeToLive = TimeSpan.FromHours(2),
            DeadLetteringOnMessageExpiration = true,
            RequiresSession = true,
            UserMetadata = "meta",
        });
        var q = r.Value;
        Assert.Equal(name, q.Name);
        Assert.Equal(7, q.MaxDeliveryCount);
        Assert.Equal(TimeSpan.FromSeconds(45), q.LockDuration);
        Assert.Equal(TimeSpan.FromHours(2), q.DefaultMessageTimeToLive);
        Assert.True(q.DeadLetteringOnMessageExpiration);
        Assert.True(q.RequiresSession);
        Assert.Equal("meta", q.UserMetadata);
        Assert.Equal(EntityStatus.Active, q.Status);
    }

    [Fact]
    public async Task CreateQueue_Twice_FailsWithConflict()
    {
        var name = await NewQueue();
        var ex = await Assert.ThrowsAsync<ServiceBusException>(async () => await Admin().CreateQueueAsync(name));
        Assert.Equal(ServiceBusFailureReason.MessagingEntityAlreadyExists, ex.Reason);
    }

    [Fact]
    public async Task GetQueue_Missing_FailsWithNotFound()
    {
        var ex = await Assert.ThrowsAsync<ServiceBusException>(async () => await Admin().GetQueueAsync(Names.Unique("nope")));
        Assert.Equal(ServiceBusFailureReason.MessagingEntityNotFound, ex.Reason);
    }

    [Fact]
    public async Task QueueExists_IsFalseThenTrue()
    {
        var a = Admin();
        var name = Names.Unique("q");
        Assert.False((await a.QueueExistsAsync(name)).Value);
        await a.CreateQueueAsync(name);
        Assert.True((await a.QueueExistsAsync(name)).Value);
    }

    [Fact]
    public async Task UpdateQueue_ChangesMaxDeliveryCount()
    {
        var a = Admin();
        var name = await NewQueue(o => o.MaxDeliveryCount = 3);
        var q = (await a.GetQueueAsync(name)).Value;
        q.MaxDeliveryCount = 9;
        var updated = (await a.UpdateQueueAsync(q)).Value;
        Assert.Equal(9, updated.MaxDeliveryCount);
        Assert.Equal(9, (await a.GetQueueAsync(name)).Value.MaxDeliveryCount);
    }

    [Fact]
    public async Task DeleteQueue_RemovesIt()
    {
        var a = Admin();
        var name = await NewQueue();
        await a.DeleteQueueAsync(name);
        Assert.False((await a.QueueExistsAsync(name)).Value);
    }

    [Fact]
    public async Task DeleteQueue_Missing_FailsWithNotFound()
    {
        var ex = await Assert.ThrowsAsync<ServiceBusException>(async () => await Admin().DeleteQueueAsync(Names.Unique("nope")));
        Assert.Equal(ServiceBusFailureReason.MessagingEntityNotFound, ex.Reason);
    }

    [Fact]
    public async Task GetQueues_ListsCreatedQueues()
    {
        var a = Admin();
        var names = new List<string>();
        for (int i = 0; i < 3; i++) names.Add(await NewQueue());
        var found = new List<string>();
        await foreach (var q in a.GetQueuesAsync()) found.Add(q.Name);
        foreach (var n in names) Assert.Contains(n, found);
    }

    [Fact]
    public async Task QueueRuntimeProperties_CountMessages()
    {
        var q = await NewQueue();
        await using var c = Client();
        var s = c.CreateSender(q);
        await s.SendMessagesAsync(new[] { Msg("a"), Msg("b"), Msg("c") });
        var rp = (await Admin().GetQueueRuntimePropertiesAsync(q)).Value;
        Assert.Equal(3, rp.ActiveMessageCount);
        Assert.Equal(0, rp.DeadLetterMessageCount);
        Assert.Equal(3, rp.TotalMessageCount);
        Assert.True(rp.SizeInBytes > 0);
    }

    [Fact]
    public async Task CreateQueue_LockDurationOverFiveMinutes_IsRejectedWith400()
    {
        var ex = await Assert.ThrowsAnyAsync<Exception>(async () =>
            await Admin().CreateQueueAsync(new CreateQueueOptions(Names.Unique("q")) { LockDuration = TimeSpan.FromMinutes(6) }));
        // the SDK turns Azure's 400 (SubCode=40000) into an ArgumentException that wraps the RequestFailedException
        var rfe = Assert.IsType<RequestFailedException>(ex.InnerException);
        Assert.Equal(400, rfe.Status);
        Assert.Contains("lock time exceeds", rfe.Message);
    }

    [Fact]
    public async Task Topic_CreateGetUpdateDelete()
    {
        var a = Admin();
        var t = Names.Unique("t");
        var created = (await a.CreateTopicAsync(new CreateTopicOptions(t) { DefaultMessageTimeToLive = TimeSpan.FromDays(1) })).Value;
        Assert.Equal(TimeSpan.FromDays(1), created.DefaultMessageTimeToLive);
        created.UserMetadata = "x";
        var upd = (await a.UpdateTopicAsync(created)).Value;
        Assert.Equal("x", upd.UserMetadata);
        await a.DeleteTopicAsync(t);
        Assert.False((await a.TopicExistsAsync(t)).Value);
    }

    [Fact]
    public async Task Subscription_CreateAndList_HasDefaultRule()
    {
        var a = Admin();
        var t = Names.Unique("t");
        await a.CreateTopicAsync(t);
        var sub = (await a.CreateSubscriptionAsync(new CreateSubscriptionOptions(t, "s1") { MaxDeliveryCount = 4 })).Value;
        Assert.Equal(4, sub.MaxDeliveryCount);
        var subs = new List<string>();
        await foreach (var s in a.GetSubscriptionsAsync(t)) subs.Add(s.SubscriptionName);
        Assert.Equal(new[] { "s1" }, subs);
        var rules = new List<RuleProperties>();
        await foreach (var r in a.GetRulesAsync(t, "s1")) rules.Add(r);
        Assert.Single(rules);
        Assert.Equal("$Default", rules[0].Name);
        Assert.IsType<TrueRuleFilter>(rules[0].Filter);
    }

    [Fact]
    public async Task Rules_CreateSqlAndCorrelationFilters_ComeBackAsTheSameTypes()
    {
        var a = Admin();
        var t = Names.Unique("t");
        await a.CreateTopicAsync(t);
        await a.CreateSubscriptionAsync(new CreateSubscriptionOptions(t, "s"), new CreateRuleOptions("sql", new SqlRuleFilter("color = 'red' AND qty > 5")) { Action = new SqlRuleAction("SET flag = 'x'") });
        await a.CreateRuleAsync(t, "s", new CreateRuleOptions("corr", new CorrelationRuleFilter { Subject = "sub1", CorrelationId = "c1", ApplicationProperties = { ["p"] = "v" } }));
        var sql = (await a.GetRuleAsync(t, "s", "sql")).Value;
        var f = Assert.IsType<SqlRuleFilter>(sql.Filter);
        Assert.Equal("color = 'red' AND qty > 5", f.SqlExpression);
        Assert.Equal("SET flag = 'x'", Assert.IsType<SqlRuleAction>(sql.Action).SqlExpression);
        var corr = Assert.IsType<CorrelationRuleFilter>((await a.GetRuleAsync(t, "s", "corr")).Value.Filter);
        Assert.Equal("sub1", corr.Subject);
        Assert.Equal("c1", corr.CorrelationId);
        Assert.Equal("v", corr.ApplicationProperties["p"]);
    }

    [Fact]
    public async Task Rule_DeleteAndUpdate()
    {
        var a = Admin();
        var t = Names.Unique("t");
        await a.CreateTopicAsync(t);
        await a.CreateSubscriptionAsync(new CreateSubscriptionOptions(t, "s"), new CreateRuleOptions("r", new FalseRuleFilter()));
        var r = (await a.GetRuleAsync(t, "s", "r")).Value;
        r.Filter = new SqlRuleFilter("1=1");
        await a.UpdateRuleAsync(t, "s", r);
        Assert.IsType<SqlRuleFilter>((await a.GetRuleAsync(t, "s", "r")).Value.Filter);
        await a.DeleteRuleAsync(t, "s", "r");
        var ex = await Assert.ThrowsAsync<ServiceBusException>(async () => await a.GetRuleAsync(t, "s", "r"));
        Assert.Equal(ServiceBusFailureReason.MessagingEntityNotFound, ex.Reason);
    }

    [Fact]
    public async Task SubscriptionRuntimeProperties_CountMessages()
    {
        var a = Admin();
        var t = Names.Unique("t");
        await a.CreateTopicAsync(t);
        await a.CreateSubscriptionAsync(t, "s");
        await using var c = Client();
        await c.CreateSender(t).SendMessagesAsync(new[] { Msg("1"), Msg("2") });
        var rp = (await a.GetSubscriptionRuntimePropertiesAsync(t, "s")).Value;
        Assert.Equal(2, rp.ActiveMessageCount);
        var tp = (await a.GetTopicRuntimePropertiesAsync(t)).Value;
        Assert.Equal(1, tp.SubscriptionCount);
    }

    [Fact]
    public async Task GetNamespaceProperties_Works()
    {
        var ns = (await Admin().GetNamespacePropertiesAsync()).Value;
        Assert.False(string.IsNullOrEmpty(ns.Name));
    }
}
