using System.Net;
using System.Text.Json.Serialization;
using Azure.Data.Tables;
using Azure.Messaging.EventGrid;
using Microsoft.Azure.Functions.Worker;
using Microsoft.Azure.Functions.Worker.Http;
using Microsoft.Extensions.Logging;

namespace FuncApp;

// One function per trigger or binding. Every trigger leaves a marker blob in the container "markers"
// (written through a Blob output binding), so a test can see that it ran. Names of containers, queues,
// hubs are fixed here; run.py creates them first.

public class Item
{
    [JsonPropertyName("id")] public string Id { get; set; } = "";
    [JsonPropertyName("value")] public string Value { get; set; } = "";
}

// ---- HTTP: the host is up -------------------------------------------------------------------
public class Http
{
    [Function("ping")]
    public HttpResponseData Ping([HttpTrigger(AuthorizationLevel.Anonymous, "get")] HttpRequestData req)
    {
        var r = req.CreateResponse(HttpStatusCode.OK);
        r.WriteString("pong");
        return r;
    }
}

// ---- Timer: only needs AzureWebJobsStorage (singleton lock in a blob lease) ------------------
public class TimerFns
{
    [Function("timer")]
    [BlobOutput("markers/timer.txt", Connection = "AzureWebJobsStorage")]
    public string Run([TimerTrigger("*/5 * * * * *")] TimerInfo timer, FunctionContext ctx)
        => "timer fired " + DateTime.UtcNow.ToString("O");
}

// ---- Queue Storage ------------------------------------------------------------------------
public class QueueFns
{
    [Function("queue_to_blob")]
    [BlobOutput("markers/queue-{QueueTrigger}.txt", Connection = "AzureWebJobsStorage")]
    public string ToBlob([QueueTrigger("q-in", Connection = "AzureWebJobsStorage")] string message, int dequeueCount)
        => $"queue message '{message}' dequeueCount={dequeueCount}";

    [Function("queue_poison")]
    public void Poison([QueueTrigger("q-fail", Connection = "AzureWebJobsStorage")] string message, int dequeueCount, ILogger<QueueFns> log)
    {
        log.LogWarning("queue_poison attempt {n} for {m}", dequeueCount, message);
        throw new InvalidOperationException("queue_poison always fails: " + message);
    }

    // HTTP -> Queue output binding
    public class QueueSendResult
    {
        [QueueOutput("q-in", Connection = "AzureWebJobsStorage")] public string? Queue { get; set; }
        [HttpResult] public HttpResponseData Http { get; set; } = null!;
    }
    [Function("http_queue_send")]
    public QueueSendResult Send([HttpTrigger(AuthorizationLevel.Anonymous, "get", "post")] HttpRequestData req)
    {
        var m = System.Web.HttpUtility.ParseQueryString(req.Url.Query)["m"] ?? "hello";
        var r = req.CreateResponse(HttpStatusCode.OK); r.WriteString("queued " + m);
        return new QueueSendResult { Queue = m, Http = r };
    }

    public class QueueFailSendResult
    {
        [QueueOutput("q-fail", Connection = "AzureWebJobsStorage")] public string? Queue { get; set; }
        [HttpResult] public HttpResponseData Http { get; set; } = null!;
    }
    [Function("http_queue_fail_send")]
    public QueueFailSendResult SendFail([HttpTrigger(AuthorizationLevel.Anonymous, "get", "post")] HttpRequestData req)
    {
        var r = req.CreateResponse(HttpStatusCode.OK); r.WriteString("queued");
        return new QueueFailSendResult { Queue = "boom", Http = r };
    }
}

// ---- Blob trigger: polling (queues + blob receipts) and Event Grid based -----------------------------
public class BlobFns
{
    [Function("blob_poll")]
    [BlobOutput("markers/blob-poll-{name}.txt", Connection = "AzureWebJobsStorage")]
    public string Poll([BlobTrigger("inbox/{name}", Connection = "AzureWebJobsStorage")] string content, string name)
        => $"blob '{name}' content '{content}'";

    [Function("blob_eventgrid")]
    [BlobOutput("markers/blob-eg-{name}.txt", Connection = "AzureWebJobsStorage")]
    public string EventGridBased([BlobTrigger("inbox-eg/{name}", Source = BlobTriggerSource.EventGrid, Connection = "AzureWebJobsStorage")] string content, string name)
        => $"blob '{name}' content '{content}'";
}

// ---- Service Bus ---------------------------------------------------------------------------
public class ServiceBusFns
{
    [Function("sb_queue")]
    [BlobOutput("markers/sb-queue.txt", Connection = "AzureWebJobsStorage")]
    public string Queue([ServiceBusTrigger("sbq", Connection = "ServiceBusConnection")] string message)
        => "sb queue: " + message;

    [Function("sb_topic")]
    [BlobOutput("markers/sb-topic.txt", Connection = "AzureWebJobsStorage")]
    public string Topic([ServiceBusTrigger("sbt", "sub1", Connection = "ServiceBusConnection")] string message)
        => "sb topic: " + message;

    [Function("sb_poison")]
    public void Poison([ServiceBusTrigger("sbq-fail", Connection = "ServiceBusConnection")] string message, int deliveryCount, ILogger<ServiceBusFns> log)
    {
        log.LogWarning("sb_poison delivery {n} for {m}", deliveryCount, message);
        throw new InvalidOperationException("sb_poison always fails");
    }

    public class SbSend
    {
        [ServiceBusOutput("sbq", Connection = "ServiceBusConnection")] public string? Message { get; set; }
        [HttpResult] public HttpResponseData Http { get; set; } = null!;
    }
    [Function("http_sb_send")]
    public SbSend Send([HttpTrigger(AuthorizationLevel.Anonymous, "get", "post")] HttpRequestData req)
    {
        var m = System.Web.HttpUtility.ParseQueryString(req.Url.Query)["m"] ?? "hello";
        var r = req.CreateResponse(HttpStatusCode.OK); r.WriteString("sent " + m);
        return new SbSend { Message = m, Http = r };
    }

    public class SbTopicSend
    {
        [ServiceBusOutput("sbt", Connection = "ServiceBusConnection")] public string? Message { get; set; }
        [HttpResult] public HttpResponseData Http { get; set; } = null!;
    }
    [Function("http_sbt_send")]
    public SbTopicSend SendTopic([HttpTrigger(AuthorizationLevel.Anonymous, "get", "post")] HttpRequestData req)
    {
        var m = System.Web.HttpUtility.ParseQueryString(req.Url.Query)["m"] ?? "hello";
        var r = req.CreateResponse(HttpStatusCode.OK); r.WriteString("sent " + m);
        return new SbTopicSend { Message = m, Http = r };
    }

    public class SbFailSend
    {
        [ServiceBusOutput("sbq-fail", Connection = "ServiceBusConnection")] public string? Message { get; set; }
        [HttpResult] public HttpResponseData Http { get; set; } = null!;
    }
    [Function("http_sb_fail_send")]
    public SbFailSend SendFail([HttpTrigger(AuthorizationLevel.Anonymous, "get", "post")] HttpRequestData req)
    {
        var r = req.CreateResponse(HttpStatusCode.OK); r.WriteString("sent");
        return new SbFailSend { Message = "boom", Http = r };
    }
}

// ---- Event Hubs ----------------------------------------------------------------------------
public class EventHubFns
{
    [Function("eh_batch")]
    [BlobOutput("markers/eh.txt", Connection = "AzureWebJobsStorage")]
    public string Trigger([EventHubTrigger("eh1", Connection = "EventHubConnection")] string[] events)
        => $"eh batch of {events.Length}: " + string.Join("|", events);

    public class EhSend
    {
        [EventHubOutput("eh1", Connection = "EventHubConnection")] public string[]? Events { get; set; }
        [HttpResult] public HttpResponseData Http { get; set; } = null!;
    }
    [Function("http_eh_send")]
    public EhSend Send([HttpTrigger(AuthorizationLevel.Anonymous, "get", "post")] HttpRequestData req)
    {
        var tag = System.Web.HttpUtility.ParseQueryString(req.Url.Query)["tag"] ?? "e";
        var r = req.CreateResponse(HttpStatusCode.OK); r.WriteString("sent");
        return new EhSend { Events = new[] { tag + "1", tag + "2", tag + "3" }, Http = r };
    }
}

// ---- Cosmos DB -----------------------------------------------------------------------------
public class CosmosFns
{
    [Function("cosmos_changefeed")]
    [BlobOutput("markers/cosmos.txt", Connection = "AzureWebJobsStorage")]
    public string Trigger([CosmosDBTrigger("appdb", "items", Connection = "CosmosConnection",
        LeaseContainerName = "leases", CreateLeaseContainerIfNotExists = true)] IReadOnlyList<Item> docs)
        => $"cosmos change feed: {docs.Count} docs: " + string.Join("|", docs.Select(d => d.Id + "=" + d.Value));

    public class CosmosPut
    {
        [CosmosDBOutput("appdb", "items", Connection = "CosmosConnection")] public Item? Doc { get; set; }
        [HttpResult] public HttpResponseData Http { get; set; } = null!;
    }
    [Function("http_cosmos_put")]
    public CosmosPut Put([HttpTrigger(AuthorizationLevel.Anonymous, "get", "post")] HttpRequestData req)
    {
        var q = System.Web.HttpUtility.ParseQueryString(req.Url.Query);
        var r = req.CreateResponse(HttpStatusCode.OK); r.WriteString("put");
        return new CosmosPut { Doc = new Item { Id = q["id"] ?? "i1", Value = q["v"] ?? "v1" }, Http = r };
    }

    [Function("http_cosmos_get")]
    public HttpResponseData Get(
        [HttpTrigger(AuthorizationLevel.Anonymous, "get")] HttpRequestData req,
        [CosmosDBInput("appdb", "items", Connection = "CosmosConnection", Id = "{Query.id}", PartitionKey = "{Query.id}")] Item? doc)
    {
        var r = req.CreateResponse(doc == null ? HttpStatusCode.NotFound : HttpStatusCode.OK);
        r.WriteString(doc == null ? "none" : doc.Id + "=" + doc.Value);
        return r;
    }
}

// ---- Table input / output ------------------------------------------------------------------
public class TableFns
{
    public class TablePut
    {
        [TableOutput("fitems", Connection = "AzureWebJobsStorage")] public TableEntity? Row { get; set; }
        [HttpResult] public HttpResponseData Http { get; set; } = null!;
    }
    [Function("http_table_put")]
    public TablePut Put([HttpTrigger(AuthorizationLevel.Anonymous, "get", "post")] HttpRequestData req)
    {
        var q = System.Web.HttpUtility.ParseQueryString(req.Url.Query);
        var r = req.CreateResponse(HttpStatusCode.OK); r.WriteString("put");
        var e = new TableEntity(q["pk"] ?? "p", q["rk"] ?? "r") { ["Value"] = q["v"] ?? "v" };
        return new TablePut { Row = e, Http = r };
    }

    [Function("http_table_get")]
    public HttpResponseData Get(
        [HttpTrigger(AuthorizationLevel.Anonymous, "get")] HttpRequestData req,
        [TableInput("fitems", "{Query.pk}", "{Query.rk}", Connection = "AzureWebJobsStorage")] TableEntity? row)
    {
        var r = req.CreateResponse(row == null ? HttpStatusCode.NotFound : HttpStatusCode.OK);
        r.WriteString(row == null ? "none" : row.GetString("Value"));
        return r;
    }
}

// ---- Event Grid: an Event Grid trigger (webhook delivery) and an Event Grid output binding (publish to a custom topic) ----------------
public class EventGridFns
{
    [Function("eg_trigger")]
    [BlobOutput("markers/eg-topic.txt", Connection = "AzureWebJobsStorage")]
    public string Trigger([EventGridTrigger] EventGridEvent ev)
        => $"eventgrid: type={ev.EventType} subject={ev.Subject} data={ev.Data}";

    public class EgSend
    {
        [EventGridOutput(TopicEndpointUri = "EgTopicUri", TopicKeySetting = "EgTopicKey")] public EventGridEvent? Event { get; set; }
        [HttpResult] public HttpResponseData Http { get; set; } = null!;
    }
    [Function("http_eg_send")]
    public EgSend Send([HttpTrigger(AuthorizationLevel.Anonymous, "get", "post")] HttpRequestData req)
    {
        var r = req.CreateResponse(HttpStatusCode.OK); r.WriteString("published");
        return new EgSend { Event = new EventGridEvent("/azemu/test", "Azemu.Test.Hello", "1.0", "hello-eventgrid"), Http = r };
    }
}
