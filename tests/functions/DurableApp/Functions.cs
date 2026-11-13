using System.Net;
using Microsoft.Azure.Functions.Worker;
using Microsoft.Azure.Functions.Worker.Http;
using Microsoft.DurableTask;
using Microsoft.DurableTask.Client;

namespace DurableApp;

// Durable Functions: function chaining, a durable timer, fan-out / fan-in. Storage provider = AzureWebJobsStorage
// (queues for control and work items, tables for history and instances, blobs for leases and large messages).
public static class Hello
{
    [Function("Orchestrator")]
    public static async Task<string> Run([OrchestrationTrigger] TaskOrchestrationContext ctx)
    {
        var outputs = new List<string>();
        outputs.Add(await ctx.CallActivityAsync<string>("Say", "Tokyo"));
        await ctx.CreateTimer(ctx.CurrentUtcDateTime.AddSeconds(3), CancellationToken.None);
        var tasks = new[] { "Seattle", "London" }.Select(c => ctx.CallActivityAsync<string>("Say", c)).ToList();
        outputs.AddRange(await Task.WhenAll(tasks));
        return string.Join(", ", outputs);
    }

    [Function("Say")]
    public static string Say([ActivityTrigger] string name) => $"Hello {name}!";

    [Function("Start")]
    public static async Task<HttpResponseData> Start(
        [HttpTrigger(AuthorizationLevel.Anonymous, "get", "post")] HttpRequestData req,
        [DurableClient] DurableTaskClient client)
    {
        string id = await client.ScheduleNewOrchestrationInstanceAsync("Orchestrator");
        return await client.CreateCheckStatusResponseAsync(req, id);
    }

    [Function("ping")]
    public static HttpResponseData Ping([HttpTrigger(AuthorizationLevel.Anonymous, "get")] HttpRequestData req)
    {
        var r = req.CreateResponse(HttpStatusCode.OK);
        r.WriteString("pong");
        return r;
    }
}
