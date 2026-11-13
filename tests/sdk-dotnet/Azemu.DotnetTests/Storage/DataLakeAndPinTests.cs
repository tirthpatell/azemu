using Azure;
using Azure.Storage;
using Azure.Storage.Blobs;
using Azure.Storage.Files.DataLake;
using Azure.Storage.Files.DataLake.Models;
using Azure.Storage.Queues;
using Azure.Storage.Sas;
using Azure.Data.Tables;

namespace Azemu.DotnetTests.Storage;

public class DataLakeTests : StorageBase, IClassFixture<StorageFixture>
{
    public DataLakeTests(StorageFixture f) : base(f) { }

    DataLakeServiceClient Svc() => new(DfsUri, Cred, Opts(new DataLakeClientOptions()));
    async Task<DataLakeFileSystemClient> Fs()
    {
        var fs = Svc().GetFileSystemClient(Names.Unique("fs"));
        await fs.CreateAsync();
        return fs;
    }
    static MemoryStream S(string v) => new(System.Text.Encoding.UTF8.GetBytes(v));
    static async Task<List<PathItem>> Paths(DataLakeFileSystemClient fs, string? path = null, bool recursive = false)
    {
        var l = new List<PathItem>(); await foreach (var p in fs.GetPathsAsync(new DataLakeGetPathsOptions { Path = path, Recursive = recursive })) l.Add(p); return l;
    }

    [Fact]
    public async Task CreateFileSystem_Twice_Fails409FilesystemAlreadyExists()
    {
        var fs = await Fs();
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await fs.CreateAsync());
        Assert.Equal(409, ex.Status);
        Assert.Equal("ContainerAlreadyExists", ex.ErrorCode);
    }

    [Fact]
    public async Task FileSystem_MetadataAndDelete()
    {
        var fs = await Fs();
        await fs.SetMetadataAsync(new Dictionary<string, string> { ["k"] = "v" });
        Assert.Equal("v", (await fs.GetPropertiesAsync()).Value.Metadata["k"]);
        await fs.DeleteAsync();
        Assert.False((await fs.ExistsAsync()).Value);
    }

    [Fact]
    public async Task CreateDirectory_AndFile_ThenAppendFlushRead()
    {
        var fs = await Fs();
        var dir = (await fs.CreateDirectoryAsync("a/b")).Value;
        var file = (await dir.CreateFileAsync("f.txt")).Value;
        var data = Bytes(5000);
        await file.AppendAsync(new MemoryStream(data, 0, 3000), 0);
        await file.AppendAsync(new MemoryStream(data, 3000, 2000), 3000);
        Assert.Equal(0, (await file.GetPropertiesAsync()).Value.ContentLength);   // not visible before flush
        await file.FlushAsync(5000);
        Assert.Equal(5000, (await file.GetPropertiesAsync()).Value.ContentLength);
        var ms = new MemoryStream(); await (await file.ReadAsync()).Value.Content.CopyToAsync(ms);
        Assert.Equal(data, ms.ToArray());
        Assert.True((await file.GetPropertiesAsync()).Value.IsDirectory == false);
    }

    [Fact]
    public async Task UploadFile_WithParallelTransfer_AndReadRange()
    {
        var fs = await Fs();
        var file = fs.GetFileClient("big/data.bin");
        var data = Bytes(9 * 1024 * 1024 + 5, 6);
        await file.UploadAsync(new MemoryStream(data), new DataLakeFileUploadOptions { TransferOptions = new StorageTransferOptions { InitialTransferSize = 1024 * 1024, MaximumTransferSize = 1024 * 1024, MaximumConcurrency = 4 } });
        Assert.Equal(data.Length, (await file.GetPropertiesAsync()).Value.ContentLength);
        var part = await file.ReadAsync(new DataLakeFileReadOptions { Range = new HttpRange(1000, 100) });
        var ms = new MemoryStream(); await part.Value.Content.CopyToAsync(ms);
        Assert.Equal(data.Skip(1000).Take(100).ToArray(), ms.ToArray());
        var all = new MemoryStream(); await file.ReadToAsync(all);
        Assert.Equal(Sha(data), Sha(all.ToArray()));
    }

    [Fact]
    public async Task GetPaths_Recursive_ListsDirectoriesAndFilesWithTheirKind()
    {
        var fs = await Fs();
        await fs.CreateDirectoryAsync("d1/d2");
        await fs.GetFileClient("d1/f1").CreateAsync();
        await fs.GetFileClient("d1/d2/f2").CreateAsync();
        await fs.GetFileClient("root").CreateAsync();
        var all = await Paths(fs, recursive: true);
        Assert.Equal(new[] { "d1", "d1/d2", "d1/d2/f2", "d1/f1", "root" }, all.Select(p => p.Name).OrderBy(x => x, StringComparer.Ordinal).ToArray());
        Assert.True(all.Single(p => p.Name == "d1").IsDirectory == true);
        Assert.False(all.Single(p => p.Name == "root").IsDirectory == true);
        Assert.Equal(new[] { "d1/d2", "d1/f1" }, (await Paths(fs, "d1")).Select(p => p.Name).OrderBy(x => x, StringComparer.Ordinal).ToArray());
    }

    [Fact]
    public async Task RenameFile_MovesTheContent_AndTheOldNameIsGone()
    {
        var fs = await Fs();
        var f = fs.GetFileClient("old/name.txt");
        await f.UploadAsync(S("content"), overwrite: true);
        await fs.CreateDirectoryAsync("new");
        var moved = (await f.RenameAsync("new/place.txt")).Value;
        var ms = new MemoryStream(); await (await moved.ReadAsync()).Value.Content.CopyToAsync(ms);
        Assert.Equal("content", System.Text.Encoding.UTF8.GetString(ms.ToArray()));
        Assert.False((await f.ExistsAsync()).Value);
    }

    [Fact]
    public async Task RenameFile_ToAMissingParentDirectory_Fails404RenameDestinationParentPathNotFound()
    {
        var fs = await Fs();
        var f = fs.GetFileClient("a.txt");
        await f.UploadAsync(S("x"), overwrite: true);
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await f.RenameAsync("nodir/b.txt"));
        Assert.Equal(404, ex.Status);
        Assert.Equal("RenameDestinationParentPathNotFound", ex.ErrorCode);
    }

    [Fact]
    public async Task RenameDirectory_MovesEveryChild()
    {
        var fs = await Fs();
        await fs.GetFileClient("src/a/1.txt").UploadAsync(S("1"), overwrite: true);
        await fs.GetFileClient("src/2.txt").UploadAsync(S("2"), overwrite: true);
        await fs.GetDirectoryClient("src").RenameAsync("dst");
        Assert.Equal(new[] { "dst", "dst/2.txt", "dst/a", "dst/a/1.txt" }, (await Paths(fs, recursive: true)).Select(p => p.Name).OrderBy(x => x, StringComparer.Ordinal).ToArray());
    }

    [Fact]
    public async Task DeleteDirectory_NonRecursive_OnANonEmptyOne_Fails409DirectoryNotEmpty()
    {
        var fs = await Fs();
        await fs.GetFileClient("d/f").UploadAsync(S("x"), overwrite: true);
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await fs.GetDirectoryClient("d").DeleteAsync(recursive: false));
        Assert.Equal(409, ex.Status);
        Assert.Equal("DirectoryNotEmpty", ex.ErrorCode);
        await fs.GetDirectoryClient("d").DeleteAsync(recursive: true);
        Assert.Empty(await Paths(fs, recursive: true));
    }

    [Fact]
    public async Task CreateFile_WithoutOverwrite_OnAnExistingPath_Fails409PathAlreadyExists()
    {
        var fs = await Fs();
        await fs.GetFileClient("x").CreateAsync();
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await fs.GetFileClient("x").CreateAsync(new DataLakePathCreateOptions { Conditions = new DataLakeRequestConditions { IfNoneMatch = ETag.All } }));
        Assert.Equal(409, ex.Status);
        Assert.Equal("PathAlreadyExists", ex.ErrorCode);
    }

    [Fact]
    public async Task ReadMissingPath_Fails404BlobNotFound_BecauseTheSdkReadsThroughTheBlobApi()
    {
        var fs = await Fs();
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await fs.GetFileClient("nope").ReadAsync());
        Assert.Equal(404, ex.Status);
        Assert.Equal("BlobNotFound", ex.ErrorCode);
    }

    [Fact]
    public async Task AccessControl_SetAndGet_Acl_Permissions_AndOwner()
    {
        var fs = await Fs();
        var f = fs.GetFileClient("acl.txt");
        await f.CreateAsync();
        var acl = new List<PathAccessControlItem>
        {
            new(AccessControlType.User, RolePermissions.Read | RolePermissions.Write | RolePermissions.Execute, entityId: null),
            new(AccessControlType.Group, RolePermissions.Read, entityId: null),
            new(AccessControlType.Other, RolePermissions.None, entityId: null),
        };
        await f.SetAccessControlListAsync(acl);
        var got = (await f.GetAccessControlAsync()).Value;
        Assert.Equal(RolePermissions.Read | RolePermissions.Write | RolePermissions.Execute, got.AccessControlList.Single(a => a.AccessControlType == AccessControlType.User && a.DefaultScope == false && a.EntityId == null).Permissions);
        Assert.Equal(RolePermissions.Read | RolePermissions.Write | RolePermissions.Execute, got.Permissions.Owner);
        Assert.Equal(RolePermissions.Read, got.Permissions.Group);
        Assert.Equal(RolePermissions.None, got.Permissions.Other);
        await f.SetPermissionsAsync(PathPermissions.ParseSymbolicPermissions("rw-r--r--"));
        var after = (await f.GetAccessControlAsync()).Value.Permissions;
        Assert.Equal(RolePermissions.Read | RolePermissions.Write, after.Owner);
        Assert.Equal(RolePermissions.Read, after.Other);
    }

    [Fact]
    public async Task PathProperties_Metadata_AndHttpHeaders()
    {
        var fs = await Fs();
        var f = fs.GetFileClient("h.txt");
        await f.CreateAsync(new DataLakePathCreateOptions { HttpHeaders = new PathHttpHeaders { ContentType = "text/x-test" }, Metadata = new Dictionary<string, string> { ["m"] = "1" } });
        var p = (await f.GetPropertiesAsync()).Value;
        Assert.Equal("text/x-test", p.ContentType);
        Assert.Equal("1", p.Metadata["m"]);
        await f.SetMetadataAsync(new Dictionary<string, string> { ["m"] = "2" });
        Assert.Equal("2", (await f.GetPropertiesAsync()).Value.Metadata["m"]);
    }

    [Fact]
    public async Task FileLease_BlocksAWriteWithoutTheLeaseId()
    {
        var fs = await Fs();
        var f = fs.GetFileClient("l.txt");
        await f.UploadAsync(S("1"), overwrite: true);
        var lease = f.GetDataLakeLeaseClient();
        await lease.AcquireAsync(TimeSpan.FromSeconds(30));
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await f.AppendAsync(new MemoryStream(new byte[] { 1 }), 1));
        Assert.Equal(412, ex.Status);
        await lease.ReleaseAsync();
    }

    [Fact]
    public async Task FileSystemSas_AllowsListingAndReadingByASecondClient()
    {
        var fs = await Fs();
        await fs.GetFileClient("s.txt").UploadAsync(S("sas-data"), overwrite: true);
        var sas = fs.GenerateSasUri(DataLakeFileSystemSasPermissions.Read | DataLakeFileSystemSasPermissions.List, DateTimeOffset.UtcNow.AddMinutes(10));
        var second = new DataLakeFileSystemClient(sas, Opts(new DataLakeClientOptions()));
        Assert.Single(await Paths(second));
        var ms = new MemoryStream(); await (await second.GetFileClient("s.txt").ReadAsync()).Value.Content.CopyToAsync(ms);
        Assert.Equal("sas-data", System.Text.Encoding.UTF8.GetString(ms.ToArray()));
    }

    [Fact]
    public async Task SameDataIsVisibleThroughTheBlobEndpoint()
    {
        var fs = await Fs();
        await fs.GetFileClient("dir/shared.txt").UploadAsync(S("both"), overwrite: true);
        var blobs = new BlobServiceClient(BlobUri, Cred, Opts(new BlobClientOptions()));
        Assert.Equal("both", (await blobs.GetBlobContainerClient(fs.Name).GetBlobClient("dir/shared.txt").DownloadContentAsync()).Value.Content.ToString());
    }

    [Fact]
    public async Task DevAccount_WrongKey_IsRefused()
    {
        var uri = new Uri($"https://devstoreaccount1.dfs.core.localhost:{Emu.Port}");
        var good = new DataLakeServiceClient(uri, new StorageSharedKeyCredential("devstoreaccount1", DevKey), Opts(new DataLakeClientOptions()));
        var fs = good.GetFileSystemClient(Names.Unique("fs"));
        await fs.CreateAsync();
        var bad = new DataLakeServiceClient(uri, new StorageSharedKeyCredential("devstoreaccount1", AnyKey), Opts(new DataLakeClientOptions()));
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await bad.GetFileSystemClient(fs.Name).GetPropertiesAsync());
        Assert.Equal(403, ex.Status);
    }
}

/// <summary>Azurite-style: --pin blob=P1 queue=P2 table=P3 and path-style connection strings on 127.0.0.1.</summary>
public class PinnedPortTests : IDisposable
{
    readonly Emu _emu = new("storage-pin", pinServices: new[] { "blob", "queue", "table" });
    public void Dispose() => _emu.Dispose();

    string Conn() => $"DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;AccountKey={StorageBase.DevKey};" +
        $"BlobEndpoint=http://127.0.0.1:{_emu.Pins["blob"]}/devstoreaccount1;QueueEndpoint=http://127.0.0.1:{_emu.Pins["queue"]}/devstoreaccount1;TableEndpoint=http://127.0.0.1:{_emu.Pins["table"]}/devstoreaccount1;";

    [Fact]
    public async Task Blob_OnAPinnedPort_WithADevelopmentStyleConnectionString()
    {
        var svc = new BlobServiceClient(Conn());
        var c = svc.GetBlobContainerClient(Names.Unique("c"));
        await c.CreateAsync();
        await c.GetBlobClient("p/q.txt").UploadAsync(BinaryData.FromString("pinned"));
        Assert.Equal("pinned", (await c.GetBlobClient("p/q.txt").DownloadContentAsync()).Value.Content.ToString());
        Assert.Equal("devstoreaccount1", c.AccountName);
        var names = new List<string>(); await foreach (var b in c.GetBlobsAsync()) names.Add(b.Name);
        Assert.Equal(new[] { "p/q.txt" }, names);
    }

    [Fact]
    public async Task Blob_PinnedPort_SasUsedBySecondClient()
    {
        var svc = new BlobServiceClient(Conn());
        var c = svc.GetBlobContainerClient(Names.Unique("c"));
        await c.CreateAsync();
        var b = c.GetBlobClient("s.txt");
        await b.UploadAsync(BinaryData.FromString("sas"));
        var second = new BlobClient(b.GenerateSasUri(BlobSasPermissions.Read, DateTimeOffset.UtcNow.AddMinutes(5)));
        Assert.Equal("sas", (await second.DownloadContentAsync()).Value.Content.ToString());
    }

    [Fact]
    public async Task Queue_OnAPinnedPort()
    {
        var q = new QueueClient(Conn(), Names.Unique("q"));
        await q.CreateAsync();
        await q.SendMessageAsync("pq");
        Assert.Equal("pq", (await q.ReceiveMessageAsync()).Value.Body.ToString());
    }

    [Fact]
    public async Task Table_OnAPinnedPort()
    {
        var t = new TableClient(Conn(), "t" + Names.Unique("x").Substring(0, 10));
        await t.CreateAsync();
        await t.AddEntityAsync(new TableEntity("p", "r") { ["v"] = 3 });
        Assert.Equal(3, (await t.GetEntityAsync<TableEntity>("p", "r")).Value.GetInt32("v"));
    }

    [Fact]
    public async Task WrongKey_OnAPinnedPort_IsRefused()
    {
        var bad = Conn().Replace(StorageBase.DevKey, StorageBase.AnyKey);
        var c = new BlobServiceClient(bad).GetBlobContainerClient(Names.Unique("c"));
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await c.CreateAsync());
        Assert.Equal(403, ex.Status);
    }
}
