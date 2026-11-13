using System.Diagnostics;
using System.Net;
using System.Net.Http;
using System.Net.Sockets;
using System.Net.Security;
using System.Security.Cryptography.X509Certificates;
using Azure.Core.Pipeline;

namespace Azemu.DotnetTests;

/// <summary>One emulator process on a free port with its own certificate directory.</summary>
public sealed class Emu : IDisposable
{
    public int Port { get; }
    public string CertDir { get; }
    public string CaPem { get; }
    public string LogPath { get; }
    public Dictionary<string, int> Pins { get; } = new();
    readonly Process _proc;
    readonly X509Certificate2 _ca;

    public static string TestRoot { get; } = FindRoot();

    static string FindRoot()
    {
        var d = new DirectoryInfo(AppContext.BaseDirectory);
        while (d != null && !File.Exists(Path.Combine(d.FullName, "run.sh"))) d = d.Parent;
        return d?.FullName ?? throw new InvalidOperationException("tests/sdk-dotnet not found above " + AppContext.BaseDirectory);
    }

    public static string BinaryPath()
    {
        var p = Environment.GetEnvironmentVariable("AZEMU_BIN");
        if (!string.IsNullOrEmpty(p)) return p;
        return Path.Combine(TestRoot, ".azemu-under-test");
    }

    public static int FreePort()
    {
        var l = new TcpListener(IPAddress.Loopback, 0);
        l.Start();
        var port = ((IPEndPoint)l.LocalEndpoint).Port;
        l.Stop();
        return port;
    }

    public Emu(string name, Dictionary<string, string>? env = null, string[]? pinServices = null, Dictionary<string, int>? fixedPins = null)
    {
        Port = FreePort();
        var tmp = Path.Combine(TestRoot, ".tmp");
        Directory.CreateDirectory(tmp);
        CertDir = Path.Combine(tmp, $"{name}-{Guid.NewGuid():N}".Substring(0, Math.Min(40, name.Length + 33)));
        Directory.CreateDirectory(CertDir);
        var logs = Path.Combine(TestRoot, "logs");
        Directory.CreateDirectory(logs);
        LogPath = Path.Combine(logs, name + ".log");
        var args = $"--port {Port} --cert-dir '{CertDir}' --log-level debug";
        if (pinServices != null)
            foreach (var s in pinServices) { var pp = FreePort(); Pins[s] = pp; args += $" --pin {s}={pp}"; }
        if (fixedPins != null)
            foreach (var kv in fixedPins) { Pins[kv.Key] = kv.Value; args += $" --pin {kv.Key}={kv.Value}"; }
        var psi = new ProcessStartInfo("/bin/sh")
        {
            UseShellExecute = false,
            RedirectStandardInput = true,
        };
        psi.ArgumentList.Add("-c");
        psi.ArgumentList.Add($"exec '{BinaryPath()}' {args} > '{LogPath}' 2>&1");
        // never inherit the real home state
        psi.Environment["HOME"] = CertDir;
        if (env != null) foreach (var kv in env) psi.Environment[kv.Key] = kv.Value;
        _proc = Process.Start(psi)!;
        CaPem = Path.Combine(CertDir, "ca.pem");
        WaitReady();
        _ca = X509Certificate2.CreateFromPem(File.ReadAllText(CaPem));
    }

    void WaitReady()
    {
        using var http = new HttpClient { Timeout = TimeSpan.FromSeconds(2) };
        var deadline = DateTime.UtcNow.AddSeconds(40);
        while (DateTime.UtcNow < deadline)
        {
            if (_proc.HasExited) throw new InvalidOperationException("azemu exited: " + File.ReadAllText(LogPath));
            try
            {
                var r = http.GetAsync($"http://127.0.0.1:{Port}/_azemu/health").GetAwaiter().GetResult();
                if (r.IsSuccessStatusCode && File.Exists(Path.Combine(CertDir, "ca.pem"))) return;
            }
            catch { }
            Thread.Sleep(100);
        }
        throw new TimeoutException("azemu did not become ready; log: " + File.ReadAllText(LogPath));
    }

    public string Host(string label, string name) => $"{name}.{label}.localhost";
    public Uri BlobUri(string acct) => new($"https://{acct}.blob.core.localhost:{Port}");
    public Uri VaultUri(string v) => new($"https://{v}.vault.localhost:{Port}");
    public Uri AppConfigUri(string s) => new($"https://{s}.azconfig.localhost:{Port}");
    public Uri CosmosUri(string a) => new($"https://{a}.documents.localhost:{Port}/");
    public string SbConn(string ns = "default") =>
        $"Endpoint=sb://{ns}.servicebus.localhost:{Port};SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true";
    public string EhConn(string ns, string? hub = null) =>
        $"Endpoint=sb://{ns}.eventhub.localhost:{Port};SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true" + (hub != null ? $";EntityPath={hub}" : "");

    /// <summary>True when the server certificate chains to the emulator CA and the name matches.</summary>
    public bool ValidateCert(object? sender, X509Certificate? cert, X509Chain? chain, SslPolicyErrors errors)
    {
        if (cert == null) return false;
        if ((errors & SslPolicyErrors.RemoteCertificateNameMismatch) != 0) return false;
        if ((errors & SslPolicyErrors.RemoteCertificateNotAvailable) != 0) return false;
        using var c = new X509Chain();
        c.ChainPolicy.TrustMode = X509ChainTrustMode.CustomRootTrust;
        c.ChainPolicy.CustomTrustStore.Add(_ca);
        c.ChainPolicy.RevocationMode = X509RevocationMode.NoCheck;
        if (chain != null)
            foreach (var e in chain.ChainElements) c.ChainPolicy.ExtraStore.Add(e.Certificate);
        return c.Build(new X509Certificate2(cert));
    }

    public SocketsHttpHandler Handler(bool http2 = true)
    {
        var h = new SocketsHttpHandler
        {
            SslOptions = new SslClientAuthenticationOptions { RemoteCertificateValidationCallback = ValidateCert },
            PooledConnectionLifetime = TimeSpan.FromMinutes(5),
            ConnectTimeout = TimeSpan.FromSeconds(10),
        };
        return h;
    }

    public HttpClientTransport Transport() => new(Handler());

    /// <summary>
    /// Transport that drops Content-Type from answers to HEAD requests. azemu sends "content-type: application/xml" with
    /// "content-length: 223" on a HEAD error and no body (older builds); Azure sends neither, and Azure.Storage then throws
    /// XmlException instead of RequestFailedException. Used only where a test needs the SDK to get past that.
    /// </summary>
    public HttpClientTransport TransportWithoutHeadContentType() => new(new HttpClient(new HeadContentTypeHandler(Handler())));

    sealed class HeadContentTypeHandler : DelegatingHandler
    {
        public HeadContentTypeHandler(HttpMessageHandler inner) : base(inner) { }
        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var r = await base.SendAsync(request, ct);
            if (request.Method == HttpMethod.Head && !r.IsSuccessStatusCode) { r.Content.Headers.ContentType = null; r.Content.Headers.ContentLength = null; }
            return r;
        }
    }
    public HttpClient NewHttpClient() => new(Handler());

    public string Log() => File.Exists(LogPath) ? File.ReadAllText(LogPath) : "";

    public void Dispose()
    {
        try { if (!_proc.HasExited) { _proc.Kill(); _proc.WaitForExit(5000); } } catch { }
        _proc.Dispose();
        try { Directory.Delete(CertDir, true); } catch { }
    }
}

/// <summary>Per-class emulator: used as IClassFixture.</summary>
public class EmuFixture : IDisposable
{
    public Emu Emu { get; }
    protected EmuFixture(string name, Dictionary<string, string>? env = null, string[]? pins = null)
    {
        Emu = new Emu(name, env, pins);
    }
    public void Dispose() => Emu.Dispose();
}

public static class Names
{
    static int _n;
    public static string Unique(string prefix = "t")
    {
        var n = Interlocked.Increment(ref _n);
        return $"{prefix}{DateTime.UtcNow:HHmmss}{n}{Guid.NewGuid():N}".Substring(0, 24).ToLowerInvariant();
    }
    public static string Dashed(string prefix = "t") => Unique(prefix);
}
