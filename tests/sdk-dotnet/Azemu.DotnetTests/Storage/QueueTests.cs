using Azure;
using Azure.Storage;
using Azure.Storage.Queues;
using Azure.Storage.Queues.Models;
using Azure.Storage.Sas;

namespace Azemu.DotnetTests.Storage;

public class QueueTests : StorageBase, IClassFixture<StorageFixture>
{
    public QueueTests(StorageFixture f) : base(f) { }

    QueueServiceClient Svc(QueueClientOptions? o = null) => new(QueueUri, Cred, Opts(o ?? new QueueClientOptions()));
    async Task<QueueClient> NewQueue(QueueClientOptions? o = null)
    {
        var q = Svc(o).GetQueueClient(Names.Unique("q"));
        await q.CreateAsync();
        return q;
    }

    [Fact]
    public async Task CreateQueue_ThenAgainWithTheSameMetadata_IsAccepted_WithDifferentMetadata_Fails409()
    {
        var q = Svc().GetQueueClient(Names.Unique("q"));
        await q.CreateAsync(new Dictionary<string, string> { ["a"] = "1" });
        await q.CreateIfNotExistsAsync(new Dictionary<string, string> { ["a"] = "1" });
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await q.CreateAsync(new Dictionary<string, string> { ["a"] = "2" }));
        Assert.Equal(409, ex.Status);
        Assert.Equal("QueueAlreadyExists", ex.ErrorCode);
    }

    [Fact]
    public async Task QueueProperties_ShowMetadataAndApproximateCount()
    {
        var q = await NewQueue();
        await q.SetMetadataAsync(new Dictionary<string, string> { ["owner"] = "tests" });
        await q.SendMessageAsync("a"); await q.SendMessageAsync("b");
        var p = (await q.GetPropertiesAsync()).Value;
        Assert.Equal("tests", p.Metadata["owner"]);
        Assert.Equal(2, p.ApproximateMessagesCount);
    }

    [Fact]
    public async Task SendReceiveDelete_RoundTripsTheMessage()
    {
        var q = await NewQueue();
        var sent = (await q.SendMessageAsync("hello")).Value;
        Assert.False(string.IsNullOrEmpty(sent.MessageId));
        Assert.False(string.IsNullOrEmpty(sent.PopReceipt));
        Assert.True(sent.ExpirationTime > DateTimeOffset.UtcNow.AddDays(6));
        var m = (await q.ReceiveMessageAsync()).Value;
        Assert.Equal("hello", m.Body.ToString());
        Assert.Equal(sent.MessageId, m.MessageId);
        Assert.Equal(1, m.DequeueCount);
        await q.DeleteMessageAsync(m.MessageId, m.PopReceipt);
        Assert.Empty((await q.ReceiveMessagesAsync(5)).Value);
    }

    [Fact]
    public async Task ReceivedMessage_IsInvisibleUntilTheVisibilityTimeout_ThenReturnsWithDequeueCountTwo()
    {
        var q = await NewQueue();
        await q.SendMessageAsync("x");
        var first = (await q.ReceiveMessageAsync(TimeSpan.FromSeconds(2))).Value;
        Assert.Null((await q.ReceiveMessageAsync()).Value);
        await Task.Delay(2500);
        var again = (await q.ReceiveMessageAsync()).Value;
        Assert.Equal(first.MessageId, again.MessageId);
        Assert.Equal(2, again.DequeueCount);
        Assert.NotEqual(first.PopReceipt, again.PopReceipt);
    }

    [Fact]
    public async Task PeekMessages_DoesNotChangeVisibilityOrDequeueCount()
    {
        var q = await NewQueue();
        await q.SendMessagesAsyncForTest(new[] { "1", "2", "3" });
        var peeked = (await q.PeekMessagesAsync(10)).Value;
        Assert.Equal(new[] { "1", "2", "3" }, peeked.Select(m => m.Body.ToString()).ToArray());
        Assert.All(peeked, m => Assert.Equal(0, m.DequeueCount));
        Assert.Equal(3, (await q.ReceiveMessagesAsync(10)).Value.Length);
    }

    [Fact]
    public async Task ReceiveMessages_MaxIsThirtyTwo_AndMoreIsRefused()
    {
        var q = await NewQueue();
        await q.SendMessagesAsyncForTest(Enumerable.Range(0, 40).Select(i => "m" + i));
        Assert.Equal(32, (await q.ReceiveMessagesAsync(32)).Value.Length);
        var ex = await Assert.ThrowsAnyAsync<Exception>(async () => await q.ReceiveMessagesAsync(33));
        Assert.True(ex is RequestFailedException or ArgumentOutOfRangeException);
    }

    [Fact]
    public async Task UpdateMessage_ChangesTheBodyAndReturnsANewPopReceipt()
    {
        var q = await NewQueue();
        await q.SendMessageAsync("old");
        var m = (await q.ReceiveMessageAsync()).Value;
        var upd = (await q.UpdateMessageAsync(m.MessageId, m.PopReceipt, "new", TimeSpan.Zero)).Value;
        Assert.NotEqual(m.PopReceipt, upd.PopReceipt);
        var again = (await q.ReceiveMessageAsync()).Value;
        Assert.Equal("new", again.Body.ToString());
    }

    [Fact]
    public async Task DeleteMessage_WithAStalePopReceipt_Fails400PopReceiptMismatch()
    {
        var q = await NewQueue();
        await q.SendMessageAsync("x");
        var m = (await q.ReceiveMessageAsync(TimeSpan.FromSeconds(1))).Value;
        await Task.Delay(1500);
        var m2 = (await q.ReceiveMessageAsync()).Value;
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await q.DeleteMessageAsync(m.MessageId, m.PopReceipt));
        Assert.Equal(400, ex.Status);
        Assert.Equal("PopReceiptMismatch", ex.ErrorCode);
        await q.DeleteMessageAsync(m2.MessageId, m2.PopReceipt);
        var gone = await Assert.ThrowsAsync<RequestFailedException>(async () => await q.DeleteMessageAsync(m2.MessageId, m2.PopReceipt));
        Assert.Equal(404, gone.Status);
        Assert.Equal("MessageNotFound", gone.ErrorCode);
    }

    [Fact]
    public async Task SendMessage_WithVisibilityTimeout_IsHiddenAtFirst()
    {
        var q = await NewQueue();
        await q.SendMessageAsync("later", visibilityTimeout: TimeSpan.FromSeconds(2));
        Assert.Null((await q.ReceiveMessageAsync()).Value);
        await Task.Delay(2500);
        Assert.Equal("later", (await q.ReceiveMessageAsync()).Value.Body.ToString());
    }

    [Fact]
    public async Task SendMessage_WithShortTimeToLive_Expires()
    {
        var q = await NewQueue();
        await q.SendMessageAsync("short", timeToLive: TimeSpan.FromSeconds(1));
        await Task.Delay(2500);
        Assert.Null((await q.ReceiveMessageAsync()).Value);
    }

    [Fact]
    public async Task ClearMessages_EmptiesTheQueue()
    {
        var q = await NewQueue();
        await q.SendMessagesAsyncForTest(new[] { "a", "b" });
        await q.ClearMessagesAsync();
        Assert.Empty((await q.PeekMessagesAsync(10)).Value);
    }

    [Fact]
    public async Task Message_Over64Kb_IsRefusedWith413RequestBodyTooLarge()
    {
        var q = await NewQueue();
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await q.SendMessageAsync(new string('x', 70 * 1024)));
        Assert.Equal(413, ex.Status);   // real Azure: 413 RequestBodyTooLarge (recorded in refs/recordings azure-storage-queue)
        Assert.Equal("RequestBodyTooLarge", ex.ErrorCode);
    }

    [Fact]
    public async Task Base64Encoding_RoundTripsBinaryLookingText_AndPlainTextIsXmlEscaped()
    {
        var q = await NewQueue(new QueueClientOptions { MessageEncoding = QueueMessageEncoding.Base64 });
        await q.SendMessageAsync(BinaryData.FromBytes(new byte[] { 0, 1, 2, 254, 255 }));
        Assert.Equal(new byte[] { 0, 1, 2, 254, 255 }, (await q.ReceiveMessageAsync()).Value.Body.ToArray());
        var p = await NewQueue();
        await p.SendMessageAsync("<tag attr=\"1\">&amp; \"quoted\" ünï</tag>");
        Assert.Equal("<tag attr=\"1\">&amp; \"quoted\" ünï</tag>", (await p.ReceiveMessageAsync()).Value.Body.ToString());
    }

    [Fact]
    public async Task MissingQueue_Fails404QueueNotFound()
    {
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await Svc().GetQueueClient(Names.Unique("nope")).SendMessageAsync("x"));
        Assert.Equal(404, ex.Status);
        Assert.Equal("QueueNotFound", ex.ErrorCode);
    }

    [Fact]
    public async Task ListQueues_WithPrefixAndMetadata()
    {
        var svc = Svc();
        var prefix = Names.Unique("lq").Substring(0, 9);
        for (int i = 0; i < 3; i++) await svc.GetQueueClient(prefix + i).CreateAsync(new Dictionary<string, string> { ["k"] = "v" });
        var found = new List<QueueItem>();
        await foreach (var q in svc.GetQueuesAsync(QueueTraits.Metadata, prefix)) found.Add(q);
        Assert.Equal(3, found.Count);
        Assert.All(found, q => Assert.Equal("v", q.Metadata["k"]));
    }

    [Fact]
    public async Task DeleteQueue_ThenGetPropertiesFails404()
    {
        var q = await NewQueue();
        await q.DeleteAsync();
        Assert.False((await q.ExistsAsync()).Value);
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await q.GetPropertiesAsync());
        Assert.Equal(404, ex.Status);
    }

    [Fact]
    public async Task AccessPolicy_SetAndGet()
    {
        var q = await NewQueue();
        await q.SetAccessPolicyAsync(new[] { new QueueSignedIdentifier { Id = "p1", AccessPolicy = new QueueAccessPolicy { Permissions = "raup", ExpiresOn = DateTimeOffset.UtcNow.AddHours(1) } } });
        var p = (await q.GetAccessPolicyAsync()).Value.ToList();
        Assert.Equal("p1", Assert.Single(p).Id);
        Assert.Equal("raup", p[0].AccessPolicy.Permissions);
    }

    [Fact]
    public async Task ServiceSas_Add_AllowsSendingByASecondClient_ButNotReading()
    {
        var q = await NewQueue();
        var sas = q.GenerateSasUri(QueueSasPermissions.Add, DateTimeOffset.UtcNow.AddMinutes(10));
        var second = new QueueClient(sas, Opts(new QueueClientOptions()));
        await second.SendMessageAsync("via sas");
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await second.ReceiveMessageAsync());
        Assert.Equal(403, ex.Status);
        Assert.Equal("via sas", (await q.ReceiveMessageAsync()).Value.Body.ToString());
    }

    [Fact]
    public async Task AccountSas_ForQueues_Works()
    {
        var svc = Svc();
        var sas = svc.GenerateAccountSasUri(AccountSasPermissions.All, DateTimeOffset.UtcNow.AddMinutes(10), AccountSasResourceTypes.All);
        var second = new QueueServiceClient(sas, Opts(new QueueClientOptions()));
        var q = second.GetQueueClient(Names.Unique("q"));
        await q.CreateAsync();
        await q.SendMessageAsync("ok");
        Assert.Equal("ok", (await q.PeekMessageAsync()).Value.Body.ToString());
    }

    [Fact]
    public async Task ServiceProperties_Logging_AndCors_RoundTrip()
    {
        var svc = Svc();
        var p = (await svc.GetPropertiesAsync()).Value;
        p.Cors.Clear();
        p.Cors.Add(new QueueCorsRule { AllowedOrigins = "https://example.org", AllowedMethods = "GET", AllowedHeaders = "*", ExposedHeaders = "*", MaxAgeInSeconds = 30 });
        await svc.SetPropertiesAsync(p);
        Assert.Equal("https://example.org", Assert.Single((await svc.GetPropertiesAsync()).Value.Cors).AllowedOrigins);
    }

    [Fact]
    public async Task DevAccount_WithAWrongKey_IsRefused403()
    {
        var uri = new Uri($"https://devstoreaccount1.queue.core.localhost:{Emu.Port}");
        var good = new QueueServiceClient(uri, new StorageSharedKeyCredential("devstoreaccount1", DevKey), Opts(new QueueClientOptions()));
        var q = good.GetQueueClient(Names.Unique("q"));
        await q.CreateAsync();
        var bad = new QueueServiceClient(uri, new StorageSharedKeyCredential("devstoreaccount1", AnyKey), Opts(new QueueClientOptions()));
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await bad.GetQueueClient(q.Name).GetPropertiesAsync());
        Assert.Equal(403, ex.Status);
        Assert.Equal("AuthenticationFailed", ex.ErrorCode);
    }
}

static class QueueTestExtensions
{
    public static async Task SendMessagesAsyncForTest(this QueueClient q, IEnumerable<string> bodies) { foreach (var b in bodies) await q.SendMessageAsync(b); }
}
