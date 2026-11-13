using System.Net.Http.Json;
using System.Text;
using System.Text.Json;
using Azure;
using Azure.Core.Serialization;
using Azure.Messaging;
using Azure.Messaging.EventGrid;
using Azure.Messaging.EventGrid.Namespaces;
using Azure.Storage;
using Azure.Storage.Queues;
using Azemu.DotnetTests.Storage;

namespace Azemu.DotnetTests.EventGridTests;

public sealed class EgFixture : EmuFixture { public EgFixture() : base("eventgrid") { } }

public class EventGridTests : IClassFixture<EgFixture>
{
    readonly Emu _emu;
    readonly string _topic = Names.Unique("tp");
    readonly string _account = Names.Unique("sa");
    public EventGridTests(EgFixture f) { _emu = f.Emu; }

    static readonly string Key = Convert.ToBase64String(new byte[32]);
    string Admin(string path) => $"http://127.0.0.1:{_emu.Port}/_azemu/eventgrid{path}";

    async Task Put(string url, object body)
    {
        using var h = new HttpClient();
        var r = await h.PutAsJsonAsync(url, body);
        Assert.True(r.IsSuccessStatusCode, await r.Content.ReadAsStringAsync());
    }

    /// <summary>Custom topic whose events go to a Storage queue, so that a test can read what was published.</summary>
    async Task<QueueClient> TopicWithQueue(string? inputSchema = null)
    {
        var qc = new QueueClient(new Uri($"https://{_account}.queue.core.localhost:{_emu.Port}/{Names.Unique("evq")}"), new StorageSharedKeyCredential(_account, Key), new QueueClientOptions { Transport = _emu.Transport(), MessageEncoding = QueueMessageEncoding.Base64 });
        await qc.CreateAsync();
        await Put(Admin($"/topics/{_topic}"), new { kind = "custom", inputSchema = inputSchema ?? "Auto" });
        await Put(Admin($"/topics/{_topic}/subscriptions/to-queue"), new { destination = new { endpointType = "StorageQueue", storageAccount = _account, queueName = qc.Name }, skipValidation = true });
        return qc;
    }

    EventGridPublisherClient Publisher() => new(new Uri($"https://{_topic}.eventgrid.localhost:{_emu.Port}/api/events"), new AzureKeyCredential("any-key"), new EventGridPublisherClientOptions { Transport = _emu.Transport() });

    static async Task<List<JsonElement>> Drain(QueueClient q, int expect, int seconds = 20)
    {
        var l = new List<JsonElement>();
        var end = DateTime.UtcNow.AddSeconds(seconds);
        while (l.Count < expect && DateTime.UtcNow < end)
        {
            foreach (var m in (await q.ReceiveMessagesAsync(32)).Value) { l.Add(JsonDocument.Parse(m.Body.ToString()).RootElement.Clone()); await q.DeleteMessageAsync(m.MessageId, m.PopReceipt); }
            if (l.Count < expect) await Task.Delay(200);
        }
        return l;
    }

    [Fact]
    public async Task EventGridSchema_Publish_ArrivesWithAllFields()
    {
        var q = await TopicWithQueue();
        var when = new DateTimeOffset(2024, 3, 4, 5, 6, 7, TimeSpan.Zero);
        await Publisher().SendEventAsync(new EventGridEvent("/subjects/a", "My.Type", "2", new { count = 3, name = "x" }) { EventTime = when, Id = "evt-1" });
        var e = Assert.Single(await Drain(q, 1));
        Assert.Equal("evt-1", e.GetProperty("id").GetString());
        Assert.Equal("/subjects/a", e.GetProperty("subject").GetString());
        Assert.Equal("My.Type", e.GetProperty("eventType").GetString());
        Assert.Equal("2", e.GetProperty("dataVersion").GetString());
        Assert.Equal(3, e.GetProperty("data").GetProperty("count").GetInt32());
        Assert.Equal(when, e.GetProperty("eventTime").GetDateTimeOffset());
    }

    [Fact]
    public async Task EventGridSchema_BatchOfEvents_AllArrive()
    {
        var q = await TopicWithQueue();
        await Publisher().SendEventsAsync(Enumerable.Range(0, 12).Select(i => new EventGridEvent("/s/" + i, "T", "1", new { i })));
        var got = await Drain(q, 12);
        Assert.Equal(12, got.Count);
        Assert.Equal(Enumerable.Range(0, 12).Select(i => "/s/" + i).OrderBy(x => x), got.Select(g => g.GetProperty("subject").GetString()!).OrderBy(x => x));
    }

    [Fact]
    public async Task CloudEventSchema_Publish_KeepsTypeSourceAndData()
    {
        var q = await TopicWithQueue();
        var ce = new CloudEvent("/my/source", "com.example.thing", new { v = 1 }) { Id = "ce-1", Subject = "subj" };
        await Publisher().SendEventAsync(ce);
        var e = Assert.Single(await Drain(q, 1));
        Assert.Equal("1.0", e.GetProperty("specversion").GetString());
        Assert.Equal("/my/source", e.GetProperty("source").GetString());
        Assert.Equal("com.example.thing", e.GetProperty("type").GetString());
        Assert.Equal("ce-1", e.GetProperty("id").GetString());
        Assert.Equal(1, e.GetProperty("data").GetProperty("v").GetInt32());
    }

    [Fact]
    public async Task CloudEvent_WithBinaryData_ArrivesAsDataBase64()
    {
        var q = await TopicWithQueue();
        await Publisher().SendEventAsync(new CloudEvent("/s", "t", new BinaryData(new byte[] { 1, 2, 3 }), "application/octet-stream"));
        var e = Assert.Single(await Drain(q, 1));
        Assert.Equal(Convert.ToBase64String(new byte[] { 1, 2, 3 }), e.GetProperty("data_base64").GetString());
    }

    [Fact]
    public async Task CustomSchema_Publish_KeepsTheJsonAsIs()
    {
        var q = await TopicWithQueue("CustomEventSchema");
        await Publisher().SendEventsAsync(new[] { new BinaryData(JsonSerializer.Serialize(new { myId = "m1", payload = new { a = 1 } })) });
        var e = Assert.Single(await Drain(q, 1));
        Assert.Equal("m1", e.GetProperty("myId").GetString());
    }

    [Fact]
    public async Task EventGridSchema_MissingRequiredField_Fails400()
    {
        await TopicWithQueue("EventGridSchema");
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await Publisher().SendEventsAsync(new[] { new BinaryData("{\"subject\":\"s\"}") }));
        Assert.Equal(400, ex.Status);
    }

    [Fact]
    public async Task Publish_WithoutACredential_Fails401()
    {
        await TopicWithQueue();
        using var h = _emu.NewHttpClient();
        var r = await h.PostAsync($"https://{_topic}.eventgrid.localhost:{_emu.Port}/api/events", new StringContent("[]", Encoding.UTF8, "application/json"));
        Assert.Equal(System.Net.HttpStatusCode.Unauthorized, r.StatusCode);
    }

    [Fact]
    public async Task SasCredential_AndSasTokenGeneration_AreAccepted()
    {
        var q = await TopicWithQueue();
        var uri = new Uri($"https://{_topic}.eventgrid.localhost:{_emu.Port}/api/events");
        var sas = new EventGridSasBuilder(uri, DateTimeOffset.UtcNow.AddHours(1)).GenerateSas(new AzureKeyCredential(Key));
        var client = new EventGridPublisherClient(uri, new AzureSasCredential(sas), new EventGridPublisherClientOptions { Transport = _emu.Transport() });
        await client.SendEventAsync(new EventGridEvent("/s", "T", "1", new { }));
        Assert.Single(await Drain(q, 1));
    }

    // ---- namespaces (pull delivery)
    string Ns => Names.Unique("egns");

    async Task<(EventGridSenderClient sender, EventGridReceiverClient receiver, string topic)> NamespaceTopic(string? sub = null)
    {
        var ns = Ns; var topic = Names.Unique("job"); sub ??= "workers";
        await Put(Admin($"/topics/{topic}?namespace={ns}"), new { kind = "namespace" });
        await Put(Admin($"/topics/{topic}/subscriptions/{sub}?namespace={ns}"), new { deliveryMode = "pull", lockDurationSeconds = 5, maxDeliveryCount = 3 });
        var endpoint = new Uri($"https://{ns}.eventgrid.localhost:{_emu.Port}");
        var opts = new EventGridSenderClientOptions { Transport = _emu.Transport() };
        var ropts = new EventGridReceiverClientOptions { Transport = _emu.Transport() };
        return (new EventGridSenderClient(endpoint, topic, new AzureKeyCredential("k"), opts), new EventGridReceiverClient(endpoint, topic, sub, new AzureKeyCredential("k"), ropts), topic);
    }

    [Fact]
    public async Task Namespace_SendReceiveAcknowledge_RoundTrips()
    {
        var (s, r, _) = await NamespaceTopic();
        await s.SendAsync(new CloudEvent("/src", "t.one", new { n = 1 }) { Id = "id-1" });
        var got = (await r.ReceiveAsync(maxEvents: 10, maxWaitTime: TimeSpan.FromSeconds(3))).Value;
        var d = Assert.Single(got.Details);
        Assert.Equal("id-1", d.Event.Id);
        Assert.Equal("t.one", d.Event.Type);
        Assert.Equal(1, d.BrokerProperties.DeliveryCount);
        var ack = (await r.AcknowledgeAsync(new[] { d.BrokerProperties.LockToken })).Value;
        Assert.Equal(new[] { d.BrokerProperties.LockToken }, ack.SucceededLockTokens.ToArray());
        Assert.Empty((await r.ReceiveAsync(maxWaitTime: TimeSpan.FromSeconds(1))).Value.Details);
    }

    [Fact]
    public async Task Namespace_Release_MakesTheEventAvailableAgainWithDeliveryCountTwo()
    {
        var (s, r, _) = await NamespaceTopic();
        await s.SendAsync(new CloudEvent("/src", "t", new { }));
        var d = Assert.Single((await r.ReceiveAsync(maxWaitTime: TimeSpan.FromSeconds(3))).Value.Details);
        await r.ReleaseAsync(new[] { d.BrokerProperties.LockToken });
        var again = Assert.Single((await r.ReceiveAsync(maxWaitTime: TimeSpan.FromSeconds(3))).Value.Details);
        Assert.Equal(2, again.BrokerProperties.DeliveryCount);
    }

    [Fact]
    public async Task Namespace_Reject_DeadLettersTheEvent()
    {
        var (s, r, topic) = await NamespaceTopic();
        await s.SendAsync(new CloudEvent("/src", "t", new { }));
        var d = Assert.Single((await r.ReceiveAsync(maxWaitTime: TimeSpan.FromSeconds(3))).Value.Details);
        var rej = (await r.RejectAsync(new[] { d.BrokerProperties.LockToken })).Value;
        Assert.Single(rej.SucceededLockTokens);
        Assert.Empty((await r.ReceiveAsync(maxWaitTime: TimeSpan.FromSeconds(1))).Value.Details);
    }

    [Fact]
    public async Task Namespace_AcknowledgeWithAnUnknownToken_ReportsAFailedLockToken()
    {
        var (_, r, _) = await NamespaceTopic();
        var res = (await r.AcknowledgeAsync(new[] { "not-a-token" })).Value;
        Assert.Empty(res.SucceededLockTokens);
        var f = Assert.Single(res.FailedLockTokens);
        Assert.Equal("not-a-token", f.LockToken);
    }

    [Fact]
    public async Task Namespace_RenewLock_Succeeds_AndAnExpiredLockRedelivers()
    {
        var (s, r, _) = await NamespaceTopic();
        await s.SendAsync(new CloudEvent("/src", "t", new { }));
        var d = Assert.Single((await r.ReceiveAsync(maxWaitTime: TimeSpan.FromSeconds(3))).Value.Details);
        Assert.Single((await r.RenewLocksAsync(new[] { d.BrokerProperties.LockToken })).Value.SucceededLockTokens);
        await Task.Delay(TimeSpan.FromSeconds(6));   // lock duration is 5 s
        var again = Assert.Single((await r.ReceiveAsync(maxWaitTime: TimeSpan.FromSeconds(3))).Value.Details);
        Assert.Equal(2, again.BrokerProperties.DeliveryCount);
    }

    [Fact]
    public async Task Namespace_ReceiveMaxEvents_ReturnsUpToThatMany_InOrder()
    {
        var (s, r, _) = await NamespaceTopic();
        await s.SendAsync(Enumerable.Range(0, 5).Select(i => new CloudEvent("/src", "t", new { i }) { Id = "e" + i }));
        var got = (await r.ReceiveAsync(maxEvents: 3, maxWaitTime: TimeSpan.FromSeconds(3))).Value.Details;
        Assert.Equal(new[] { "e0", "e1", "e2" }, got.Select(g => g.Event.Id).ToArray());
    }

    [Fact]
    public async Task Namespace_ReceiveFromAnEmptySubscription_ReturnsNothingAfterTheWait()
    {
        var (_, r, _) = await NamespaceTopic();
        var sw = System.Diagnostics.Stopwatch.StartNew();
        Assert.Empty((await r.ReceiveAsync(maxWaitTime: TimeSpan.FromSeconds(2))).Value.Details);
        Assert.InRange(sw.Elapsed.TotalSeconds, 1.5, 8);
    }
}
