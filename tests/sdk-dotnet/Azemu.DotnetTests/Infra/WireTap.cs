using System.Net;
using System.Net.Sockets;
using System.Text;

namespace Azemu.DotnetTests;

/// <summary>A TCP relay in front of the emulator that records the bytes of each direction (for reading AMQP frames in a failure).</summary>
public sealed class WireTap : IDisposable
{
    readonly TcpListener _l;
    readonly int _target;
    readonly StreamWriter _w;
    readonly CancellationTokenSource _cts = new();
    public int Port { get; }

    public WireTap(int targetPort, string logFile)
    {
        _target = targetPort;
        _l = new TcpListener(IPAddress.Loopback, 0);
        _l.Start();
        Port = ((IPEndPoint)_l.LocalEndpoint).Port;
        Directory.CreateDirectory(Path.Combine(Emu.TestRoot, "logs"));
        _w = new StreamWriter(Path.Combine(Emu.TestRoot, "logs", logFile)) { AutoFlush = true };
        _ = Task.Run(Accept);
    }

    async Task Accept()
    {
        int n = 0;
        try
        {
            while (!_cts.IsCancellationRequested)
            {
                var c = await _l.AcceptTcpClientAsync(_cts.Token);
                var id = ++n;
                _ = Task.Run(async () =>
                {
                    using var up = new TcpClient();
                    await up.ConnectAsync(IPAddress.Loopback, _target);
                    var a = Pump(c, up, $"c{id} C->S");
                    var b = Pump(up, c, $"c{id} S->C");
                    await Task.WhenAny(a, b);
                    c.Dispose(); up.Dispose();
                });
            }
        }
        catch { }
    }

    async Task Pump(TcpClient from, TcpClient to, string label)
    {
        var buf = new byte[65536];
        try
        {
            var fs = from.GetStream(); var ts = to.GetStream();
            while (true)
            {
                var n = await fs.ReadAsync(buf);
                if (n == 0) break;
                await ts.WriteAsync(buf.AsMemory(0, n));
                lock (_w) _w.WriteLine($"{DateTime.UtcNow:HH:mm:ss.fff} {label} {n} bytes: {Printable(buf, n)}");
            }
        }
        catch { }
    }

    static string Printable(byte[] b, int n)
    {
        var sb = new StringBuilder();
        for (int i = 0; i < Math.Min(n, 3000); i++) sb.Append(b[i] >= 32 && b[i] < 127 ? (char)b[i] : '.');
        return sb.ToString();
    }

    public void Dispose() { _cts.Cancel(); _l.Stop(); _w.Dispose(); }
}
