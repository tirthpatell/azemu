using System.Diagnostics.Tracing;
using Azure.Core.Diagnostics;

namespace Azemu.DotnetTests;

/// <summary>Writes the Azure SDK's own event source log to a file (for work out why a test fails).</summary>
public sealed class SdkLog : IDisposable
{
    readonly AzureEventSourceListener _l;
    readonly StreamWriter _w;
    public SdkLog(string file, EventLevel level = EventLevel.Verbose)
    {
        Directory.CreateDirectory(Path.Combine(Emu.TestRoot, "logs"));
        _w = new StreamWriter(Path.Combine(Emu.TestRoot, "logs", file)) { AutoFlush = true };
        _l = new AzureEventSourceListener((e, m) => { lock (_w) _w.WriteLine($"{DateTime.UtcNow:HH:mm:ss.fff} {e.EventSource.Name} {m}"); }, level);
    }
    public void Dispose() { _l.Dispose(); _w.Dispose(); }
}
