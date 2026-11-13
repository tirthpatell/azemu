using Azure;
using Azure.Core;
using Azure.Storage;
using Azure.Storage.Blobs;
using Azure.Storage.Blobs.Models;
using Azure.Storage.Queues;
using Azure.Data.Tables;
using Azure.Storage.Files.DataLake;
using Azure.ResourceManager;
using Azure.ResourceManager.Resources;
using Azure.ResourceManager.Storage;
using Azure.ResourceManager.Storage.Models;

namespace Azemu.DotnetTests.Storage;

public sealed class StorageFixture : EmuFixture
{
    public StorageFixture() : base("storage") { }
}

public sealed class FakeTokenCredential : TokenCredential
{
    public override AccessToken GetToken(TokenRequestContext r, CancellationToken ct) => new("not-a-real-token", DateTimeOffset.UtcNow.AddHours(1));
    public override ValueTask<AccessToken> GetTokenAsync(TokenRequestContext r, CancellationToken ct) => new(GetToken(r, ct));
}

public abstract class StorageBase
{
    public const string DevKey = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
    public static readonly string AnyKey = Convert.ToBase64String(new byte[32]);
    protected readonly Emu Emu;
    protected readonly string Account;
    protected readonly StorageSharedKeyCredential Cred;
    protected StorageBase(EmuFixture f)
    {
        Emu = f.Emu;
        Account = Names.Unique("sa");
        Cred = new StorageSharedKeyCredential(Account, AnyKey);
    }

    // A precaution: a HEAD error with a content type makes Azure.Storage.Blobs throw XmlException. This transport removes it.
    protected T Opts<T>(T o, bool fixHead = true) where T : ClientOptions { o.Transport = fixHead ? Emu.TransportWithoutHeadContentType() : Emu.Transport(); return o; }

    protected Uri BlobUri => new($"https://{Account}.blob.core.localhost:{Emu.Port}");
    protected Uri DfsUri => new($"https://{Account}.dfs.core.localhost:{Emu.Port}");
    protected Uri QueueUri => new($"https://{Account}.queue.core.localhost:{Emu.Port}");
    protected Uri TableUri => new($"https://{Account}.table.core.localhost:{Emu.Port}");

    protected BlobServiceClient Blobs(bool fixHead = true) => new(BlobUri, Cred, Opts(new BlobClientOptions(), fixHead));
    protected BlobServiceClient BlobsToken() => new(BlobUri, new FakeTokenCredential(), Opts(new BlobClientOptions { }));
    protected async Task<BlobContainerClient> NewContainer(PublicAccessType access = PublicAccessType.None)
    {
        var c = Blobs().GetBlobContainerClient(Names.Unique("c"));
        await c.CreateAsync(access);
        return c;
    }
    /// <summary>
    /// A container in a storage account with blob versioning on. Versioning is off until someone turns it on, and a team
    /// turns it on through the management API: the storage account and <c>isVersioningEnabled</c> on <c>blobServices/default</c>.
    /// Any key works with such an account (the emulator checks the keys of the development account only).
    /// </summary>
    protected async Task<BlobContainerClient> NewVersionedContainer()
    {
        var endpoint = new Uri($"https://management.localhost:{Emu.Port}");
        var env = new ArmEnvironment(endpoint, $"https://management.localhost:{Emu.Port}/.default");
        var arm = new ArmClient(new FakeTokenCredential(), "11111111-2222-3333-4444-555555555555", new ArmClientOptions { Environment = env, Transport = Emu.Transport() });
        var sub = await arm.GetDefaultSubscriptionAsync();
        var rg = (await sub.GetResourceGroups().CreateOrUpdateAsync(WaitUntil.Completed, Names.Dashed("rgver"), new ResourceGroupData(AzureLocation.EastUS))).Value;
        var name = Names.Unique("sav");
        var acct = (await rg.GetStorageAccounts().CreateOrUpdateAsync(WaitUntil.Completed, name, new StorageAccountCreateOrUpdateContent(new StorageSku(StorageSkuName.StandardLrs), StorageKind.StorageV2, AzureLocation.EastUS))).Value;
        var props = (await acct.GetBlobService().CreateOrUpdateAsync(WaitUntil.Completed, new BlobServiceData { IsVersioningEnabled = true })).Value;
        Assert.True(props.Data.IsVersioningEnabled);
        var svc = new BlobServiceClient(new Uri($"https://{name}.blob.core.localhost:{Emu.Port}"), new StorageSharedKeyCredential(name, AnyKey), Opts(new BlobClientOptions()));
        var c = svc.GetBlobContainerClient(Names.Unique("c"));
        await c.CreateAsync();
        return c;
    }
    protected static string Sha(byte[] b) => Convert.ToHexString(System.Security.Cryptography.SHA256.HashData(b));
    protected static byte[] Bytes(int n, int seed = 1) { var b = new byte[n]; new Random(seed).NextBytes(b); return b; }

    protected static async Task<RequestFailedException> Fails(Func<Task> f) => await Assert.ThrowsAsync<RequestFailedException>(f);
}
