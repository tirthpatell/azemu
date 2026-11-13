using Azure.Core;
using Azure.Core.Pipeline;

namespace Azemu.DotnetTests;

/// <summary>Records request line and response status and body of every call that goes through a client (for looking at what the SDK got).</summary>
public sealed class CapturePolicy : HttpPipelinePolicy
{
    public List<string> Entries { get; } = new();
    public override void Process(HttpMessage m, ReadOnlyMemory<HttpPipelinePolicy> p) { ProcessNext(m, p); Add(m); }
    public override async ValueTask ProcessAsync(HttpMessage m, ReadOnlyMemory<HttpPipelinePolicy> p) { await ProcessNextAsync(m, p); Add(m); }
    void Add(HttpMessage m)
    {
        string body = "";
        try { if (m.Response.ContentStream is { CanSeek: true } s) { var pos = s.Position; body = new StreamReader(s).ReadToEnd(); s.Position = pos; } } catch { }
        lock (Entries) Entries.Add($"{m.Request.Method} {m.Request.Uri} -> {m.Response.Status}\n{body}");
    }
}
