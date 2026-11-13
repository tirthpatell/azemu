using System.Security.Cryptography;
using Azure;
using Azure.Storage;
using Azure.Storage.Blobs;
using Azure.Storage.Blobs.Models;
using Azure.Storage.Blobs.Specialized;
using Azure.Storage.Sas;

namespace Azemu.DotnetTests.Storage;

public class BlobTests : StorageBase, IClassFixture<StorageFixture>
{
    public BlobTests(StorageFixture f) : base(f) { }

    // ---- containers
    [Fact]
    public async Task CreateContainer_ThenAgain_Fails409ContainerAlreadyExists()
    {
        var c = Blobs().GetBlobContainerClient(Names.Unique("c"));
        var r = await c.CreateAsync();
        Assert.False(string.IsNullOrEmpty(r.Value.ETag.ToString()));
        var ex = await Fails(async () => await c.CreateAsync());
        Assert.Equal(409, ex.Status);
        Assert.Equal("ContainerAlreadyExists", ex.ErrorCode);
    }

    [Fact]
    public async Task CreateIfNotExists_ReturnsNullTheSecondTime()
    {
        var c = Blobs().GetBlobContainerClient(Names.Unique("c"));
        Assert.NotNull((await c.CreateIfNotExistsAsync()).Value);
        Assert.Null((await c.CreateIfNotExistsAsync())?.Value);
    }

    [Fact]
    public async Task ContainerMetadata_SetAndGet_RoundTrips()
    {
        var c = await NewContainer();
        await c.SetMetadataAsync(new Dictionary<string, string> { ["Alpha"] = "1", ["beta"] = "two words" });
        var p = (await c.GetPropertiesAsync()).Value;
        Assert.Equal("1", p.Metadata["Alpha"]);
        Assert.Equal("two words", p.Metadata["beta"]);
    }

    [Fact]
    public async Task ContainerExists_FalseAfterDelete()
    {
        var c = await NewContainer();
        Assert.True((await c.ExistsAsync()).Value);
        await c.DeleteAsync();
        Assert.False((await c.ExistsAsync()).Value);
    }

    [Fact]
    public async Task GetContainers_WithPrefixAndMetadata_ListsMatches()
    {
        var svc = Blobs();
        var prefix = Names.Unique("lst").Substring(0, 10);
        for (int i = 0; i < 3; i++) await svc.GetBlobContainerClient(prefix + i).CreateAsync(metadata: new Dictionary<string, string> { ["k"] = "v" });
        var found = new List<BlobContainerItem>();
        await foreach (var ci in svc.GetBlobContainersAsync(BlobContainerTraits.Metadata, prefix: prefix)) found.Add(ci);
        Assert.Equal(3, found.Count);
        Assert.All(found, ci => Assert.Equal("v", ci.Properties.Metadata["k"]));
    }

    [Fact]
    public async Task ContainerAccessPolicy_SetAndGet_RoundTripsIdentifiers()
    {
        var c = await NewContainer();
        var start = DateTimeOffset.UtcNow.AddMinutes(-5); var expiry = DateTimeOffset.UtcNow.AddHours(1);
        await c.SetAccessPolicyAsync(PublicAccessType.Blob, new[] { new BlobSignedIdentifier { Id = "pol1", AccessPolicy = new BlobAccessPolicy { PolicyStartsOn = start, PolicyExpiresOn = expiry, Permissions = "rl" } } });
        var p = (await c.GetAccessPolicyAsync()).Value;
        Assert.Equal(PublicAccessType.Blob, p.BlobPublicAccess);
        var id = Assert.Single(p.SignedIdentifiers);
        Assert.Equal("pol1", id.Id);
        Assert.Equal("rl", id.AccessPolicy.Permissions);
    }

    [Fact]
    public async Task PublicContainer_AllowsAnonymousBlobRead_ButNotWrite()
    {
        var c = await NewContainer(PublicAccessType.Blob);
        await c.GetBlobClient("pub.txt").UploadAsync(BinaryData.FromString("public"));
        var anon = new BlobClient(c.GetBlobClient("pub.txt").Uri, Opts(new BlobClientOptions()));
        Assert.Equal("public", (await anon.DownloadContentAsync()).Value.Content.ToString());
        var ex = await Fails(async () => await new BlobClient(c.GetBlobClient("w.txt").Uri, Opts(new BlobClientOptions())).UploadAsync(BinaryData.FromString("x")));
        Assert.Contains(ex.Status, new[] { 401, 403, 404 });
    }

    [Fact]
    public async Task PrivateContainer_AnonymousRead_IsRefused()
    {
        var c = await NewContainer();
        await c.GetBlobClient("priv.txt").UploadAsync(BinaryData.FromString("x"));
        var ex = await Fails(async () => await new BlobClient(c.GetBlobClient("priv.txt").Uri, Opts(new BlobClientOptions())).DownloadContentAsync());
        Assert.Contains(ex.Status, new[] { 401, 404 });
    }

    [Fact]
    public async Task ContainerLease_AcquireRenewChangeBreakRelease()
    {
        var c = await NewContainer();
        var lease = c.GetBlobLeaseClient();
        var l = (await lease.AcquireAsync(TimeSpan.FromSeconds(30))).Value;
        Assert.False(string.IsNullOrEmpty(l.LeaseId));
        await lease.RenewAsync();
        var newId = Guid.NewGuid().ToString();
        Assert.Equal(newId, (await lease.ChangeAsync(newId)).Value.LeaseId);
        var ex = await Fails(async () => await c.DeleteAsync());
        Assert.Equal(412, ex.Status);
        Assert.Equal("LeaseIdMissing", ex.ErrorCode);
        var broken = await lease.BreakAsync(TimeSpan.Zero);
        Assert.Equal(0, broken.Value.LeaseTime);
        await c.DeleteAsync();
    }

    // ---- block blobs
    [Fact]
    public async Task Upload_SmallBlob_ReturnsEtagAndRoundTrips()
    {
        var c = await NewVersionedContainer();   // the version id is part of the answer only when versioning is on
        var b = c.GetBlobClient("small.bin");
        var data = Bytes(1000);
        var r = await b.UploadAsync(BinaryData.FromBytes(data), new BlobUploadOptions { HttpHeaders = new BlobHttpHeaders { ContentType = "application/x-test", CacheControl = "no-cache", ContentLanguage = "en", ContentDisposition = "attachment" }, Metadata = new Dictionary<string, string> { ["m"] = "1" }, Tags = new Dictionary<string, string> { ["t"] = "v" } });
        Assert.False(string.IsNullOrEmpty(r.Value.ETag.ToString()));
        Assert.NotNull(r.Value.VersionId);
        var p = (await b.GetPropertiesAsync()).Value;
        Assert.Equal(1000, p.ContentLength);
        Assert.Equal("application/x-test", p.ContentType);
        Assert.Equal("no-cache", p.CacheControl);
        Assert.Equal("en", p.ContentLanguage);
        Assert.Equal("attachment", p.ContentDisposition);
        Assert.Equal("1", p.Metadata["m"]);
        Assert.Equal(BlobType.Block, p.BlobType);
        Assert.Equal(data, (await b.DownloadContentAsync()).Value.Content.ToArray());
        Assert.Equal("v", (await b.GetTagsAsync()).Value.Tags["t"]);
    }

    [Fact]
    public async Task Upload_WithoutOverwrite_ToAnExistingBlob_Fails409BlobAlreadyExists()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("x");
        await b.UploadAsync(BinaryData.FromString("1"));
        var ex = await Fails(async () => await b.UploadAsync(BinaryData.FromString("2"), overwrite: false));
        Assert.Equal(409, ex.Status);
        Assert.Equal("BlobAlreadyExists", ex.ErrorCode);
        await b.UploadAsync(BinaryData.FromString("2"), overwrite: true);
        Assert.Equal("2", (await b.DownloadContentAsync()).Value.Content.ToString());
    }

    [Fact]
    public async Task Upload_LargeBlob_WithParallelBlocks_StoresEveryBlockInOrder()
    {
        var c = await NewContainer();
        var b = c.GetBlockBlobClient("large.bin");
        var data = Bytes(18 * 1024 * 1024 + 123);
        await b.UploadAsync(new MemoryStream(data), new BlobUploadOptions { TransferOptions = new StorageTransferOptions { InitialTransferSize = 1 * 1024 * 1024, MaximumTransferSize = 2 * 1024 * 1024, MaximumConcurrency = 6 } });
        var list = (await b.GetBlockListAsync(BlockListTypes.Committed)).Value;
        Assert.True(list.CommittedBlocks.Count() >= 9, $"{list.CommittedBlocks.Count()} blocks");
        Assert.Equal(data.Length, list.CommittedBlocks.Sum(x => (long)x.Size));
        Assert.Equal(Sha(data), Sha((await b.DownloadContentAsync()).Value.Content.ToArray()));
    }

    [Fact]
    public async Task DownloadTo_Stream_WithParallelRanges_ReturnsTheWholeBlob()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("big.bin");
        var data = Bytes(10 * 1024 * 1024 + 7, 9);
        await b.UploadAsync(new MemoryStream(data));
        var ms = new MemoryStream();
        await b.DownloadToAsync(ms, new BlobDownloadToOptions { TransferOptions = new StorageTransferOptions { InitialTransferSize = 1024 * 1024, MaximumTransferSize = 1024 * 1024, MaximumConcurrency = 5 } });
        Assert.Equal(Sha(data), Sha(ms.ToArray()));
    }

    [Fact]
    public async Task DownloadTo_File_WritesTheContent()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("f.bin");
        var data = Bytes(300_000);
        await b.UploadAsync(new MemoryStream(data));
        var path = Path.Combine(Emu.TestRoot, ".tmp", Names.Unique("dl") + ".bin");
        try { await b.DownloadToAsync(path); Assert.Equal(data, await File.ReadAllBytesAsync(path)); }
        finally { File.Delete(path); }
    }

    [Fact]
    public async Task Download_ByRange_ReturnsThatSliceAnd206()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("r.bin");
        var data = Bytes(5000);
        await b.UploadAsync(new MemoryStream(data));
        var r = await b.DownloadStreamingAsync(new BlobDownloadOptions { Range = new HttpRange(100, 50) });
        var ms = new MemoryStream(); await r.Value.Content.CopyToAsync(ms);
        Assert.Equal(data.Skip(100).Take(50).ToArray(), ms.ToArray());
        Assert.Equal(50, r.Value.Details.ContentLength);
        Assert.Contains("bytes 100-149/5000", r.Value.Details.ContentRange ?? "");
    }

    [Fact]
    public async Task Download_RangeBeyondTheEnd_Fails416InvalidRange()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("r.bin");
        await b.UploadAsync(BinaryData.FromString("abc"));
        var ex = await Fails(async () => await b.DownloadStreamingAsync(new BlobDownloadOptions { Range = new HttpRange(100, 10) }));
        Assert.Equal(416, ex.Status);
        Assert.Equal("InvalidRange", ex.ErrorCode);
    }

    [Fact]
    public async Task OpenWrite_Stream_WritesBlocksAndOpenRead_ReadsThemBack()
    {
        var c = await NewContainer();
        var b = c.GetBlockBlobClient("stream.bin");
        var data = Bytes(5 * 1024 * 1024 + 11, 4);
        await using (var w = await b.OpenWriteAsync(true, new BlockBlobOpenWriteOptions { BufferSize = 1024 * 1024 }))
        {
            for (int i = 0; i < data.Length; i += 700_000) await w.WriteAsync(data.AsMemory(i, Math.Min(700_000, data.Length - i)));
        }
        await using var r = await b.OpenReadAsync(new BlobOpenReadOptions(false) { BufferSize = 512 * 1024 });
        var ms = new MemoryStream(); await r.CopyToAsync(ms);
        Assert.Equal(Sha(data), Sha(ms.ToArray()));
    }

    [Theory]
    [InlineData(StorageChecksumAlgorithm.StorageCrc64)]
    [InlineData(StorageChecksumAlgorithm.MD5)]
    public async Task TransferValidation_UploadAndDownload_PassesWithTheChosenChecksum(StorageChecksumAlgorithm alg)
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("v.bin");
        var data = Bytes(6 * 1024 * 1024, 3);
        await b.UploadAsync(new MemoryStream(data), new BlobUploadOptions { TransferValidation = new UploadTransferValidationOptions { ChecksumAlgorithm = alg }, TransferOptions = new StorageTransferOptions { InitialTransferSize = 1024 * 1024, MaximumTransferSize = 1024 * 1024 } });
        var ms = new MemoryStream();
        await b.DownloadToAsync(ms, new BlobDownloadToOptions { TransferValidation = new DownloadTransferValidationOptions { ChecksumAlgorithm = alg }, TransferOptions = new StorageTransferOptions { InitialTransferSize = 1024 * 1024, MaximumTransferSize = 1024 * 1024 } });
        Assert.Equal(Sha(data), Sha(ms.ToArray()));
    }

    [Fact]
    public async Task Upload_WithAWrongContentMd5_Fails400Md5Mismatch()
    {
        var c = await NewContainer();
        var b = c.GetBlockBlobClient("m.bin");
        var ex = await Fails(async () => await b.UploadAsync(new MemoryStream(Bytes(100)), new BlobUploadOptions { HttpHeaders = new BlobHttpHeaders { ContentHash = MD5.HashData(new byte[] { 1 }) } }));
        Assert.Equal(400, ex.Status);
        Assert.Equal("Md5Mismatch", ex.ErrorCode);
    }

    [Fact]
    public async Task StageBlock_CommitBlockList_GetBlockList_ShowUncommittedAndCommitted()
    {
        var c = await NewContainer();
        var b = c.GetBlockBlobClient("blocks");
        string Id(int i) => Convert.ToBase64String(BitConverter.GetBytes(i).Concat(new byte[4]).ToArray());
        await b.StageBlockAsync(Id(1), new MemoryStream(Bytes(100, 1)));
        await b.StageBlockAsync(Id(2), new MemoryStream(Bytes(200, 2)));
        var unc = (await b.GetBlockListAsync(BlockListTypes.Uncommitted)).Value;
        Assert.Equal(2, unc.UncommittedBlocks.Count());
        await b.CommitBlockListAsync(new[] { Id(2), Id(1) });
        var list = (await b.GetBlockListAsync(BlockListTypes.All)).Value;
        Assert.Equal(new[] { 200, 100 }, list.CommittedBlocks.Select(x => x.Size).ToArray());
        Assert.Equal(300, (await b.GetPropertiesAsync()).Value.ContentLength);
    }

    [Fact]
    public async Task CommitBlockList_WithAnUnknownBlock_Fails400InvalidBlockList()
    {
        var c = await NewContainer();
        var b = c.GetBlockBlobClient("blocks");
        var ex = await Fails(async () => await b.CommitBlockListAsync(new[] { Convert.ToBase64String(new byte[] { 1, 2, 3 }) }));
        Assert.Equal(400, ex.Status);
        Assert.Equal("InvalidBlockList", ex.ErrorCode);
    }

    // ---- append and page blobs
    [Fact]
    public async Task AppendBlob_AppendBlock_GrowsTheBlob_AndAppendPositionConditionIsChecked()
    {
        var c = await NewContainer();
        var b = c.GetAppendBlobClient("log");
        await b.CreateAsync();
        var r1 = await b.AppendBlockAsync(new MemoryStream(new byte[] { 1, 2, 3 }));
        Assert.Equal(0, r1.Value.BlobAppendOffset is { } o ? long.Parse(o) : -1);
        await b.AppendBlockAsync(new MemoryStream(new byte[] { 4, 5 }));
        Assert.Equal(5, (await b.GetPropertiesAsync()).Value.ContentLength);
        Assert.Equal(2, (await b.GetPropertiesAsync()).Value.BlobCommittedBlockCount);
        var ex = await Fails(async () => await b.AppendBlockAsync(new MemoryStream(new byte[] { 9 }), new AppendBlobAppendBlockOptions { Conditions = new AppendBlobRequestConditions { IfAppendPositionEqual = 1 } }));
        Assert.Equal(412, ex.Status);
        Assert.Equal("AppendPositionConditionNotMet", ex.ErrorCode);
        Assert.Equal(new byte[] { 1, 2, 3, 4, 5 }, (await b.DownloadContentAsync()).Value.Content.ToArray());
    }

    [Fact]
    public async Task PageBlob_UploadPages_GetRanges_ClearAndResize()
    {
        var c = await NewContainer();
        var b = c.GetPageBlobClient("disk");
        await b.CreateAsync(4096);
        await b.UploadPagesAsync(new MemoryStream(Bytes(512, 1)), 512);
        await b.UploadPagesAsync(new MemoryStream(Bytes(1024, 2)), 2048);
        var ranges = (await b.GetPageRangesAsync()).Value.PageRanges.Select(r => (r.Offset, r.Length ?? -1)).ToArray();
        Assert.Equal(new[] { (512L, 512L), (2048L, 1024L) }, ranges);
        await b.ClearPagesAsync(new HttpRange(512, 512));
        Assert.Single((await b.GetPageRangesAsync()).Value.PageRanges);
        await b.ResizeAsync(8192);
        Assert.Equal(8192, (await b.GetPropertiesAsync()).Value.ContentLength);
        Assert.Equal(BlobType.Page, (await b.GetPropertiesAsync()).Value.BlobType);
    }

    [Fact]
    public async Task PageBlob_LengthNotMultipleOf512_Fails400()
    {
        var c = await NewContainer();
        var ex = await Fails(async () => await c.GetPageBlobClient("bad").CreateAsync(1000));
        Assert.Equal(400, ex.Status);
    }

    // ---- listing and names
    [Fact]
    public async Task ListBlobs_Flat_WithPrefix_AndHierarchy_WithDelimiter()
    {
        var c = await NewContainer();
        foreach (var n in new[] { "a/1.txt", "a/2.txt", "a/sub/3.txt", "b/4.txt", "root.txt" }) await c.GetBlobClient(n).UploadAsync(BinaryData.FromString(n));
        var flat = new List<string>(); await foreach (var i in List(c, prefix: "a/")) flat.Add(i.Name);
        Assert.Equal(new[] { "a/1.txt", "a/2.txt", "a/sub/3.txt" }, flat);
        var hier = new List<string>(); await foreach (var i in c.GetBlobsByHierarchyAsync(new GetBlobsByHierarchyOptions { Delimiter = "/" })) hier.Add((i.IsPrefix ? "P:" : "B:") + (i.IsPrefix ? i.Prefix : i.Blob.Name));
        Assert.Equal(new[] { "P:a/", "P:b/", "B:root.txt" }, hier);
    }

    [Fact]
    public async Task ListBlobs_PagesOfTwo_FollowTheContinuationToken()
    {
        var c = await NewContainer();
        for (int i = 0; i < 7; i++) await c.GetBlobClient($"p{i}").UploadAsync(BinaryData.FromString("x"));
        var pages = 0; var names = new List<string>();
        await foreach (var page in c.GetBlobsAsync().AsPages(pageSizeHint: 2)) { pages++; names.AddRange(page.Values.Select(v => v.Name)); }
        Assert.Equal(4, pages);
        Assert.Equal(7, names.Count);
    }

    [Theory]
    [InlineData("with space.txt")]
    [InlineData("plus+sign=eq&amp.txt")]
    [InlineData("hash#tag.txt")]
    [InlineData("question?mark.txt")]
    [InlineData("percent%20.txt")]
    [InlineData("ünïcode/日本語/файл.txt")]
    [InlineData("trailing.dot.")]
    [InlineData("emoji-😀.txt")]
    [InlineData("a//double//slash")]
    public async Task BlobNames_NeedingEncoding_RoundTrip(string name)
    {
        var c = await NewContainer();
        var b = c.GetBlobClient(name);
        await b.UploadAsync(BinaryData.FromString("v:" + name));
        Assert.Equal("v:" + name, (await b.DownloadContentAsync()).Value.Content.ToString());
        var listed = new List<string>(); await foreach (var i in c.GetBlobsAsync()) listed.Add(i.Name);
        Assert.Contains(name, listed);
        await b.DeleteAsync();
    }

    // ---- errors and existence
    [Fact]
    public async Task Download_Missing_Fails404BlobNotFound()
    {
        var c = await NewContainer();
        var ex = await Fails(async () => await c.GetBlobClient("nope").DownloadContentAsync());
        Assert.Equal(404, ex.Status);
        Assert.Equal("BlobNotFound", ex.ErrorCode);
    }

    [Fact]
    public async Task Download_FromAMissingContainer_Fails404ContainerNotFound()
    {
        var ex = await Fails(async () => await Blobs().GetBlobContainerClient(Names.Unique("nocont")).GetBlobClient("x").DownloadContentAsync());
        Assert.Equal(404, ex.Status);
        Assert.Equal("ContainerNotFound", ex.ErrorCode);
    }

    [Fact]
    public async Task Exists_And_GetProperties_OnAMissingBlob_UseTheHeadStatus()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("nope");
        Assert.False((await b.ExistsAsync()).Value);
        var ex = await Fails(async () => await b.GetPropertiesAsync());
        Assert.Equal(404, ex.Status);
        Assert.Equal("BlobNotFound", ex.ErrorCode);
    }

    [Fact]
    public async Task Exists_OnAMissingBlob_WithTheDefaultTransport_ReturnsFalse()
    {
        var svc = Blobs(fixHead: false);
        var c = svc.GetBlobContainerClient(Names.Unique("c"));
        await c.CreateAsync();
        Assert.False((await c.GetBlobClient("nope").ExistsAsync()).Value);
    }

    [Fact]
    public async Task GetProperties_OnAMissingBlob_WithTheDefaultTransport_Fails404BlobNotFound()
    {
        var svc = Blobs(fixHead: false);
        var c = svc.GetBlobContainerClient(Names.Unique("c"));
        await c.CreateAsync();
        var ex = await Fails(async () => await c.GetBlobClient("nope").GetPropertiesAsync());
        Assert.Equal("BlobNotFound", ex.ErrorCode);
    }

    [Fact]
    public async Task DeleteIfExists_ReturnsFalseForAMissingBlob_AndDeleteFails404()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("gone");
        Assert.False((await b.DeleteIfExistsAsync()).Value);
        var ex = await Fails(async () => await b.DeleteAsync());
        Assert.Equal("BlobNotFound", ex.ErrorCode);
    }

    [Fact]
    public async Task ConditionalHeaders_IfMatch_IfNoneMatch_IfModifiedSince()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("cond");
        var etag = (await b.UploadAsync(BinaryData.FromString("1"))).Value.ETag;
        var ex = await Fails(async () => await b.UploadAsync(new MemoryStream(new byte[] { 1 }), new BlobUploadOptions { Conditions = new BlobRequestConditions { IfMatch = new ETag("\"nope\"") } }));
        Assert.Equal(412, ex.Status);
        Assert.Equal("ConditionNotMet", ex.ErrorCode);
        // If-None-Match: * on an existing blob is BlobAlreadyExists (409) on Put Blob
        var ex2 = await Fails(async () => await b.UploadAsync(new MemoryStream(new byte[] { 1 }), new BlobUploadOptions { Conditions = new BlobRequestConditions { IfNoneMatch = ETag.All } }));
        Assert.Equal(409, ex2.Status);
        Assert.Equal("BlobAlreadyExists", ex2.ErrorCode);
        await b.UploadAsync(new MemoryStream(new byte[] { 2 }), new BlobUploadOptions { Conditions = new BlobRequestConditions { IfMatch = etag } });
        // the SDK does not throw on 304 for a download: it returns the response with that status
        var notModified = await b.DownloadStreamingAsync(new BlobDownloadOptions { Conditions = new BlobRequestConditions { IfModifiedSince = DateTimeOffset.UtcNow.AddMinutes(5) } });
        Assert.Equal(304, notModified.GetRawResponse().Status);
    }

    // ---- leases, snapshots, versions, tags, copy, soft delete
    [Fact]
    public async Task BlobLease_BlocksWritesWithoutTheLeaseId_AndAllowsThemWithIt()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("leased");
        await b.UploadAsync(BinaryData.FromString("1"));
        var lease = b.GetBlobLeaseClient();
        var id = (await lease.AcquireAsync(TimeSpan.FromSeconds(30))).Value.LeaseId;
        var ex = await Fails(async () => await b.UploadAsync(BinaryData.FromString("2"), overwrite: true));
        Assert.Equal(412, ex.Status);
        Assert.Equal("LeaseIdMissing", ex.ErrorCode);
        await b.UploadAsync(new MemoryStream(new byte[] { 3 }), new BlobUploadOptions { Conditions = new BlobRequestConditions { LeaseId = id } });
        var p = (await b.GetPropertiesAsync()).Value;
        Assert.Equal(LeaseState.Leased, p.LeaseState);
        Assert.Equal(LeaseDurationType.Fixed, p.LeaseDuration);
        await lease.ReleaseAsync();
        Assert.Equal(LeaseState.Available, (await b.GetPropertiesAsync()).Value.LeaseState);
    }

    [Fact]
    public async Task InfiniteLease_AcquireWithMinusOne()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("inf");
        await b.UploadAsync(BinaryData.FromString("1"));
        await b.GetBlobLeaseClient().AcquireAsync(BlobLeaseClient.InfiniteLeaseDuration);
        Assert.Equal(LeaseDurationType.Infinite, (await b.GetPropertiesAsync()).Value.LeaseDuration);
    }

    [Fact]
    public async Task Snapshot_IsAFrozenCopy_AndListedWithSnapshotState()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("snap");
        await b.UploadAsync(BinaryData.FromString("v1"));
        var snap = (await b.CreateSnapshotAsync()).Value;
        await b.UploadAsync(BinaryData.FromString("v2"), overwrite: true);
        Assert.Equal("v1", (await b.WithSnapshot(snap.Snapshot).DownloadContentAsync()).Value.Content.ToString());
        Assert.Equal("v2", (await b.DownloadContentAsync()).Value.Content.ToString());
        var items = new List<BlobItem>(); await foreach (var i in List(c, BlobTraits.None, BlobStates.Snapshots)) items.Add(i);
        Assert.Equal(2, items.Count);
        Assert.Single(items, i => i.Snapshot == snap.Snapshot);
        var ex = await Fails(async () => await b.DeleteAsync());
        Assert.Equal("SnapshotsPresent", ex.ErrorCode);
        await b.DeleteAsync(DeleteSnapshotsOption.IncludeSnapshots);
        Assert.False((await b.ExistsAsync()).Value);
    }

    [Fact]
    public async Task Versions_AnAccountWithoutVersioning_KeepsNoVersions()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("nover");
        Assert.Null((await b.UploadAsync(BinaryData.FromString("one"))).Value.VersionId);
        Assert.Null((await b.UploadAsync(BinaryData.FromString("two"), overwrite: true)).Value.VersionId);
        var versions = new List<BlobItem>(); await foreach (var i in List(c, BlobTraits.None, BlobStates.Version)) versions.Add(i);
        Assert.Single(versions);
    }

    [Fact]
    public async Task Versions_OverwriteCreatesAVersion_ThatCanBeReadAndDeleted()
    {
        var c = await NewVersionedContainer();
        var b = c.GetBlobClient("ver");
        var v1 = (await b.UploadAsync(BinaryData.FromString("one"))).Value.VersionId;
        var v2 = (await b.UploadAsync(BinaryData.FromString("two"), overwrite: true)).Value.VersionId;
        Assert.NotEqual(v1, v2);
        Assert.Equal("one", (await b.WithVersion(v1).DownloadContentAsync()).Value.Content.ToString());
        var versions = new List<BlobItem>(); await foreach (var i in List(c, BlobTraits.None, BlobStates.Version)) versions.Add(i);
        Assert.Equal(2, versions.Count);
        Assert.Contains(versions, i => i.IsLatestVersion == true && i.VersionId == v2);
        await b.WithVersion(v1).DeleteAsync();
        var ex = await Fails(async () => await b.WithVersion(v1).DownloadContentAsync());
        Assert.Equal(404, ex.Status);
    }

    [Fact]
    public async Task Tags_SetGetAndFindBlobsByTags_AtContainerAndAccountLevel()
    {
        var c = await NewContainer();
        var tag = Names.Unique("tg");
        foreach (var (n, v) in new[] { ("t1", "red"), ("t2", "blue"), ("t3", "red") })
        {
            var b = c.GetBlobClient(n);
            await b.UploadAsync(BinaryData.FromString(n));
            await b.SetTagsAsync(new Dictionary<string, string> { [tag] = v, ["common"] = "yes" });
        }
        Assert.Equal("red", (await c.GetBlobClient("t1").GetTagsAsync()).Value.Tags[tag]);
        var inContainer = new List<string>(); await foreach (var i in c.FindBlobsByTagsAsync($"\"{tag}\"='red'")) inContainer.Add(i.BlobName);
        Assert.Equal(new[] { "t1", "t3" }, inContainer.OrderBy(x => x).ToArray());
        var account = new List<string>(); await foreach (var i in Blobs().FindBlobsByTagsAsync($"\"{tag}\"='blue' AND \"common\"='yes'")) account.Add(i.BlobName);
        Assert.Equal(new[] { "t2" }, account);
    }

    [Fact]
    public async Task Tags_MoreThanTen_Fails400()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("x");
        await b.UploadAsync(BinaryData.FromString("x"));
        var ex = await Fails(async () => await b.SetTagsAsync(Enumerable.Range(0, 11).ToDictionary(i => "k" + i, i => "v")));
        Assert.Equal(400, ex.Status);
    }

    [Fact]
    public async Task Copy_StartCopyFromUri_SameAccount_CompletesWithSuccess()
    {
        var c = await NewContainer();
        var src = c.GetBlobClient("src");
        var data = Bytes(100_000);
        await src.UploadAsync(new MemoryStream(data));
        var dst = c.GetBlobClient("dst");
        var op = await dst.StartCopyFromUriAsync(src.GenerateSasUri(BlobSasPermissions.Read, DateTimeOffset.UtcNow.AddMinutes(10)));
        await op.WaitForCompletionAsync();
        Assert.Equal(CopyStatus.Success, (await dst.GetPropertiesAsync()).Value.CopyStatus);
        Assert.Equal(data, (await dst.DownloadContentAsync()).Value.Content.ToArray());
    }

    [Fact]
    public async Task SyncCopy_FromUri_CopiesTheContent()
    {
        var c = await NewContainer();
        var src = c.GetBlobClient("src");
        await src.UploadAsync(BinaryData.FromString("copy me"));
        var dst = c.GetBlobClient("dst");
        var r = await dst.SyncCopyFromUriAsync(src.GenerateSasUri(BlobSasPermissions.Read, DateTimeOffset.UtcNow.AddMinutes(10)));
        Assert.Equal(CopyStatus.Success, r.Value.CopyStatus);
        Assert.Equal("copy me", (await dst.DownloadContentAsync()).Value.Content.ToString());
    }

    [Fact]
    public async Task AccessTier_SetAndRead()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("tier");
        await b.UploadAsync(BinaryData.FromString("x"));
        await b.SetAccessTierAsync(AccessTier.Cool);
        Assert.Equal("Cool", (await b.GetPropertiesAsync()).Value.AccessTier);
    }

    [Fact]
    public async Task SoftDelete_DeletedBlobIsListedAsDeleted_AndUndeleteBringsItBack()
    {
        var svc = Blobs();
        var props = (await svc.GetPropertiesAsync()).Value;   // the SDK needs the whole object back, so read-modify-write
        props.DeleteRetentionPolicy = new BlobRetentionPolicy { Enabled = true, Days = 2 };
        await svc.SetPropertiesAsync(props);
        var got = (await svc.GetPropertiesAsync()).Value;
        Assert.True(got.DeleteRetentionPolicy.Enabled);
        Assert.Equal(2, got.DeleteRetentionPolicy.Days);
        var c = await NewContainer();
        var b = c.GetBlobClient("soft");
        await b.UploadAsync(BinaryData.FromString("keep"));
        await b.DeleteAsync();
        var deleted = new List<BlobItem>(); await foreach (var i in List(c, BlobTraits.None, BlobStates.Deleted)) deleted.Add(i);
        Assert.Contains(deleted, i => i.Name == "soft" && i.Deleted);
        await b.UndeleteAsync();
        Assert.Equal("keep", (await b.DownloadContentAsync()).Value.Content.ToString());
    }

    [Fact]
    public async Task ServiceProperties_Cors_SetAndGet()
    {
        var svc = Blobs();
        var p = (await svc.GetPropertiesAsync()).Value;
        p.Cors.Clear();
        p.Cors.Add(new BlobCorsRule { AllowedOrigins = "https://example.com", AllowedMethods = "GET,PUT", AllowedHeaders = "x-ms-*", ExposedHeaders = "x-ms-request-id", MaxAgeInSeconds = 60 });
        await svc.SetPropertiesAsync(p);
        var back = (await svc.GetPropertiesAsync()).Value;
        Assert.Equal("https://example.com", Assert.Single(back.Cors).AllowedOrigins);
        Assert.Equal(60, back.Cors[0].MaxAgeInSeconds);
    }

    [Fact]
    public async Task AccountInfo_AndStatistics_Work()
    {
        var svc = Blobs();
        var info = (await svc.GetAccountInfoAsync()).Value;
        Assert.NotNull(info.SkuName.ToString());
    }

    // ---- SAS
    [Fact]
    public async Task ServiceSas_Read_LetsASecondClientDownload_ButNotWrite()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("sas.txt");
        await b.UploadAsync(BinaryData.FromString("secret"));
        var sas = b.GenerateSasUri(BlobSasPermissions.Read, DateTimeOffset.UtcNow.AddMinutes(10));
        var second = new BlobClient(sas, Opts(new BlobClientOptions()));
        Assert.Equal("secret", (await second.DownloadContentAsync()).Value.Content.ToString());
        var ex = await Fails(async () => await second.UploadAsync(BinaryData.FromString("x"), overwrite: true));
        Assert.Equal(403, ex.Status);
        Assert.Equal("AuthorizationPermissionMismatch", ex.ErrorCode);
    }

    [Fact]
    public async Task ServiceSas_Expired_IsRefusedWith403AuthenticationFailed()
    {
        var c = await NewContainer();
        var b = c.GetBlobClient("sas.txt");
        await b.UploadAsync(BinaryData.FromString("x"));
        var sas = b.GenerateSasUri(new BlobSasBuilder(BlobSasPermissions.Read, DateTimeOffset.UtcNow.AddMinutes(-1)) { StartsOn = DateTimeOffset.UtcNow.AddHours(-2), BlobContainerName = c.Name, BlobName = "sas.txt" });
        var ex = await Fails(async () => await new BlobClient(sas, Opts(new BlobClientOptions())).DownloadContentAsync());
        Assert.Equal(403, ex.Status);
    }

    [Fact]
    public async Task ServiceSas_WithATamperedSignature_OnTheDevAccount_Is403AuthenticationFailed()
    {
        // signatures are checked for devstoreaccount1 only (README, "Credentials are checked loosely")
        var svc = new BlobServiceClient(new Uri($"https://devstoreaccount1.blob.core.localhost:{Emu.Port}"), new StorageSharedKeyCredential("devstoreaccount1", DevKey), Opts(new BlobClientOptions()));
        var c = svc.GetBlobContainerClient(Names.Unique("c"));
        await c.CreateAsync();
        var b = c.GetBlobClient("sas.txt");
        await b.UploadAsync(BinaryData.FromString("x"));
        var sas = b.GenerateSasUri(BlobSasPermissions.Read, DateTimeOffset.UtcNow.AddMinutes(10));
        Assert.Equal("x", (await new BlobClient(sas, Opts(new BlobClientOptions())).DownloadContentAsync()).Value.Content.ToString());
        var bad = new Uri(sas.ToString().Replace("sig=", "sig=AAAA"));
        var ex = await Fails(async () => await new BlobClient(bad, Opts(new BlobClientOptions())).DownloadContentAsync());
        Assert.Equal(403, ex.Status);
        Assert.Equal("AuthenticationFailed", ex.ErrorCode);
    }

    [Fact]
    public async Task ContainerSas_ListsAndReadsAllBlobs()
    {
        var c = await NewContainer();
        await c.GetBlobClient("a").UploadAsync(BinaryData.FromString("a"));
        var sas = c.GenerateSasUri(BlobContainerSasPermissions.Read | BlobContainerSasPermissions.List, DateTimeOffset.UtcNow.AddMinutes(10));
        var second = new BlobContainerClient(sas, Opts(new BlobClientOptions()));
        var names = new List<string>(); await foreach (var i in second.GetBlobsAsync()) names.Add(i.Name);
        Assert.Equal(new[] { "a" }, names);
        Assert.Equal("a", (await second.GetBlobClient("a").DownloadContentAsync()).Value.Content.ToString());
    }

    [Fact]
    public async Task AccountSas_WithWriteAndCreate_AllowsAnotherClientToUpload()
    {
        var svc = Blobs();
        var sas = svc.GenerateAccountSasUri(AccountSasPermissions.Read | AccountSasPermissions.Write | AccountSasPermissions.Create | AccountSasPermissions.List, DateTimeOffset.UtcNow.AddMinutes(10), AccountSasResourceTypes.All);
        var second = new BlobServiceClient(sas, Opts(new BlobClientOptions()));
        var cont = second.GetBlobContainerClient(Names.Unique("c"));
        await cont.CreateAsync();
        await cont.GetBlobClient("w").UploadAsync(BinaryData.FromString("by sas"));
        Assert.Equal("by sas", (await svc.GetBlobContainerClient(cont.Name).GetBlobClient("w").DownloadContentAsync()).Value.Content.ToString());
    }

    [Fact]
    public async Task SasWithStoredAccessPolicy_UsesThePolicyPermissions()
    {
        var c = await NewContainer();
        await c.GetBlobClient("p").UploadAsync(BinaryData.FromString("x"));
        await c.SetAccessPolicyAsync(permissions: new[] { new BlobSignedIdentifier { Id = "readonly", AccessPolicy = new BlobAccessPolicy { PolicyExpiresOn = DateTimeOffset.UtcNow.AddHours(1), Permissions = "r" } } });
        var sas = c.GenerateSasUri(new BlobSasBuilder { Identifier = "readonly", BlobContainerName = c.Name, Resource = "c" });
        Assert.Equal("x", (await new BlobClient(new BlobUriBuilder(c.Uri) { BlobName = "p", Sas = new BlobUriBuilder(sas).Sas }.ToUri(), Opts(new BlobClientOptions())).DownloadContentAsync()).Value.Content.ToString());
    }

    [Fact]
    public async Task UserDelegationSas_BuiltFromADelegationKey_WorksForASecondClient()
    {
        var oauth = BlobsToken();
        var key = (await oauth.GetUserDelegationKeyAsync(new BlobGetUserDelegationKeyOptions(DateTimeOffset.UtcNow.AddHours(1)) { StartsOn = DateTimeOffset.UtcNow.AddMinutes(-1) })).Value;
        Assert.False(string.IsNullOrEmpty(key.Value));
        var c = await NewContainer();
        await c.GetBlobClient("ud").UploadAsync(BinaryData.FromString("ud-data"));
        var b = new BlobSasBuilder(BlobSasPermissions.Read, DateTimeOffset.UtcNow.AddMinutes(30)) { BlobContainerName = c.Name, BlobName = "ud", Resource = "b" };
        var uri = new BlobUriBuilder(c.GetBlobClient("ud").Uri) { Sas = b.ToSasQueryParameters(key, Account) }.ToUri();
        Assert.Equal("ud-data", (await new BlobClient(uri, Opts(new BlobClientOptions())).DownloadContentAsync()).Value.Content.ToString());
    }

    // ---- batch
    [Fact]
    public async Task BlobBatch_DeleteBlobs_RemovesAllInOneRequest()
    {
        var c = await NewContainer();
        var uris = new List<Uri>();
        for (int i = 0; i < 4; i++) { var b = c.GetBlobClient("d" + i); await b.UploadAsync(BinaryData.FromString("x")); uris.Add(b.Uri); }
        var batch = Blobs().GetBlobBatchClient();
        var resp = await batch.DeleteBlobsAsync(uris);
        Assert.Equal(4, resp.Length);
        Assert.All(resp, r => Assert.Equal(202, r.Status));
        Assert.Empty(await Collect(c.GetBlobsAsync()));
    }

    [Fact]
    public async Task BlobBatch_SetAccessTier_AndOneFailureDoesNotStopTheOthers()
    {
        var c = await NewContainer();
        var ok = c.GetBlobClient("ok"); await ok.UploadAsync(BinaryData.FromString("x"));
        var batch = Blobs().GetBlobBatchClient();
        var b = batch.CreateBatch();    // the SDK allows one operation type per batch
        var r1 = b.SetBlobAccessTier(ok.Uri, AccessTier.Cool);
        var r2 = b.SetBlobAccessTier(c.GetBlobClient("missing").Uri, AccessTier.Cool);
        await batch.SubmitBatchAsync(b, throwOnAnyFailure: false);
        Assert.Equal(200, r1.Status);
        Assert.Equal(404, r2.Status);
        Assert.Equal("Cool", (await ok.GetPropertiesAsync()).Value.AccessTier);
    }

    static AsyncPageable<BlobItem> List(BlobContainerClient c, BlobTraits traits = BlobTraits.None, BlobStates states = BlobStates.None, string? prefix = null) => c.GetBlobsAsync(new GetBlobsOptions { Traits = traits, States = states, Prefix = prefix });

    static async Task<List<string>> Collect(AsyncPageable<BlobItem> p) { var l = new List<string>(); await foreach (var i in p) l.Add(i.Name); return l; }

    // ---- auth
    [Fact]
    public async Task SharedKey_DevStoreAccount_WithTheRightKeyWorks_AndAWrongKeyIsRefused()
    {
        var uri = new Uri($"https://devstoreaccount1.blob.core.localhost:{Emu.Port}");
        var good = new BlobServiceClient(uri, new StorageSharedKeyCredential("devstoreaccount1", DevKey), Opts(new BlobClientOptions()));
        var c = good.GetBlobContainerClient(Names.Unique("c"));
        await c.CreateAsync();
        var bad = new BlobServiceClient(uri, new StorageSharedKeyCredential("devstoreaccount1", AnyKey), Opts(new BlobClientOptions()));
        var ex = await Fails(async () => await bad.GetBlobContainerClient(c.Name).GetPropertiesAsync());
        Assert.Equal(403, ex.Status);
        Assert.Equal("AuthenticationFailed", ex.ErrorCode);
    }

    [Fact]
    public async Task ConnectionString_WithBlobEndpoint_Works()
    {
        var cs = $"DefaultEndpointsProtocol=https;AccountName={Account};AccountKey={AnyKey};BlobEndpoint=https://{Account}.blob.core.localhost:{Emu.Port};";
        var svc = new BlobServiceClient(cs, Opts(new BlobClientOptions()));
        var c = svc.GetBlobContainerClient(Names.Unique("c"));
        await c.CreateAsync();
        await c.GetBlobClient("cs").UploadAsync(BinaryData.FromString("ok"));
        Assert.Equal("ok", (await c.GetBlobClient("cs").DownloadContentAsync()).Value.Content.ToString());
    }

    [Fact]
    public async Task ManyConcurrentUploads_AllSucceed()
    {
        var c = await NewContainer();
        await Task.WhenAll(Enumerable.Range(0, 40).Select(i => c.GetBlobClient("m" + i).UploadAsync(BinaryData.FromBytes(Bytes(10_000, i)))));
        Assert.Equal(40, (await Collect(c.GetBlobsAsync())).Count);
    }
}
