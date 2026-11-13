using Azure.Messaging.ServiceBus;
using Azure.Messaging.ServiceBus.Administration;

namespace Azemu.DotnetTests.ServiceBus;

public sealed class SbFixture : EmuFixture
{
    public SbFixture() : base("servicebus") { }
}

public sealed class SbNoAutoFixture : EmuFixture
{
    public SbNoAutoFixture() : base("servicebus-noauto", new() { ["AZEMU_SERVICEBUS_AUTOCREATE"] = "0" }) { }
}

public abstract class SbBase
{
    protected readonly Emu Emu;
    protected readonly string Ns;
    protected readonly string Conn;
    protected SbBase(EmuFixture f, string? ns = null)
    {
        Emu = f.Emu;
        Ns = ns ?? Names.Unique("ns");
        Conn = Emu.SbConn(Ns);
    }

    protected ServiceBusClient Client(ServiceBusTransportType t = ServiceBusTransportType.AmqpTcp, bool crossEntity = false, int retries = 0, int tryTimeoutSeconds = 30)
    {
        var o = new ServiceBusClientOptions
        {
            TransportType = t,
            EnableCrossEntityTransactions = crossEntity,
            RetryOptions = new ServiceBusRetryOptions { MaxRetries = retries, TryTimeout = TimeSpan.FromSeconds(tryTimeoutSeconds) },
            CertificateValidationCallback = (s, c, ch, e) => Emu.ValidateCert(s, c, ch, e),
        };
        if (t == ServiceBusTransportType.AmqpWebSockets)
            o.CustomEndpointAddress = new Uri($"https://{Ns}.servicebus.localhost:{Emu.Port}");
        return new ServiceBusClient(Conn, o);
    }

    protected ServiceBusAdministrationClient Admin()
    {
        var o = new ServiceBusAdministrationClientOptions { Transport = Emu.Transport() };
        return new ServiceBusAdministrationClient(Conn, o);
    }

    protected async Task<string> NewQueue(Action<CreateQueueOptions>? cfg = null)
    {
        var name = Names.Unique("q");
        var o = new CreateQueueOptions(name);
        cfg?.Invoke(o);
        await Admin().CreateQueueAsync(o);
        return name;
    }

    protected static ServiceBusMessage Msg(string body = "hello") => new(body);

    protected static async Task<ServiceBusReceivedMessage?> Recv(ServiceBusReceiver r, int secs = 10)
        => await r.ReceiveMessageAsync(TimeSpan.FromSeconds(secs));
}
