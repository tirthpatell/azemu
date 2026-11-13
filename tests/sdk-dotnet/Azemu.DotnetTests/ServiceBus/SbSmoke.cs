using Azure.Messaging.ServiceBus;
using Azure.Messaging.ServiceBus.Administration;

namespace Azemu.DotnetTests.ServiceBus;

public class SbSmoke : SbBase, IClassFixture<SbFixture>
{
    public SbSmoke(SbFixture f) : base(f) { }

    [Fact]
    public async Task SendAndReceive_OverTcp_ReturnsTheBody()
    {
        var q = await NewQueue();
        await using var c = Client();
        await c.CreateSender(q).SendMessageAsync(Msg("hi"));
        var m = await Recv(c.CreateReceiver(q));
        Assert.NotNull(m);
        Assert.Equal("hi", m!.Body.ToString());
    }
}
