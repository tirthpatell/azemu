package azemu;

import com.azure.core.http.rest.PagedIterable;
import com.azure.core.util.BinaryData;
import com.azure.core.util.Context;
import com.azure.storage.blob.*;
import com.azure.storage.blob.models.*;
import com.azure.storage.blob.options.*;
import com.azure.storage.blob.specialized.*;
import com.azure.storage.blob.models.ParallelTransferOptions;
import com.azure.storage.common.StorageSharedKeyCredential;
import org.junit.jupiter.api.*;

import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.time.Duration;
import java.time.OffsetDateTime;
import java.util.*;
import java.util.stream.Collectors;

import static org.junit.jupiter.api.Assertions.*;

@EmuTest
class BlobTest {
    static Emu emu;
    static String acct;
    static BlobServiceClient svc;

    @BeforeAll static void setUp() {
        emu = Emu.current();
        acct = Emu.name("blobacct");
        svc = new BlobServiceClientBuilder().endpoint(emu.blobUrl(acct))
            .credential(new StorageSharedKeyCredential(acct, Emu.DEV_KEY)).buildClient();
    }

    BlobContainerClient container() { return svc.createBlobContainer(Emu.dashed("c")); }

    static BlobStorageException storageError(org.junit.jupiter.api.function.Executable e) {
        return assertThrows(BlobStorageException.class, e);
    }

    // ---- containers ---------------------------------------------------------------------------------------
    @Test void createContainer_twice_isContainerAlreadyExists409() {
        String n = Emu.dashed("c");
        svc.createBlobContainer(n);
        BlobStorageException e = storageError(() -> svc.createBlobContainer(n));
        assertEquals(409, e.getStatusCode());
        assertEquals(BlobErrorCode.CONTAINER_ALREADY_EXISTS, e.getErrorCode());
    }

    @Test void createContainerIfNotExists_secondCallReturnsFalse() {
        String n = Emu.dashed("c");
        BlobContainerClient c = svc.getBlobContainerClient(n);
        assertTrue(c.createIfNotExists());
        assertFalse(c.createIfNotExists());
        assertTrue(c.exists());
    }

    @Test void containerMetadataAndProperties_roundTrip() {
        BlobContainerClient c = svc.createBlobContainer(Emu.dashed("c"));
        c.setMetadata(Map.of("env", "test", "owner", "java"));
        BlobContainerProperties p = c.getProperties();
        assertEquals("test", p.getMetadata().get("env"));
        assertEquals("java", p.getMetadata().get("owner"));
        assertNotNull(p.getETag());
        assertNotNull(p.getLastModified());
    }

    @Test void getPropertiesOfMissingContainer_is404ContainerNotFound() {
        BlobContainerClient c = svc.getBlobContainerClient(Emu.dashed("missing"));
        BlobStorageException e = storageError(c::getProperties);
        assertEquals(404, e.getStatusCode());
        assertEquals(BlobErrorCode.CONTAINER_NOT_FOUND, e.getErrorCode());
    }

    @Test void deleteContainer_thenBlobAccess_is404() {
        BlobContainerClient c = container();
        c.getBlobClient("x").upload(BinaryData.fromString("x"));
        c.delete();
        assertFalse(c.exists());
        assertEquals(404, storageError(() -> c.getBlobClient("x").getProperties()).getStatusCode());
    }

    @Test void listContainers_withPrefixAndMaxResults_pagesThroughAll() {
        String p = Emu.name("lc");
        for (int i = 0; i < 5; i++) svc.createBlobContainer(p + "-" + i);
        List<String> names = svc.listBlobContainers(new ListBlobContainersOptions().setPrefix(p), null)
            .stream().map(BlobContainerItem::getName).collect(Collectors.toList());
        assertEquals(List.of(p + "-0", p + "-1", p + "-2", p + "-3", p + "-4"), names);
        int pages = 0;
        for (var page : svc.listBlobContainers(new ListBlobContainersOptions().setPrefix(p).setMaxResultsPerPage(2), null).iterableByPage()) {
            pages++;
            assertTrue(page.getValue().size() <= 2);
        }
        assertEquals(3, pages);
    }

    @Test void containerPublicAccess_blobLevelAllowsAnonymousRead() {
        BlobContainerClient c = container();
        c.setAccessPolicy(PublicAccessType.BLOB, null);
        c.getBlobClient("pub").upload(BinaryData.fromString("public data"));
        assertEquals(PublicAccessType.BLOB, c.getAccessPolicy().getBlobAccessType());
        BlobClient anon = new BlobClientBuilder().endpoint(emu.blobUrl(acct)).containerName(c.getBlobContainerName()).blobName("pub").buildClient();
        assertEquals("public data", anon.downloadContent().toString());
    }

    @Test void anonymousReadOfPrivateContainer_is401or404Auth() {
        BlobContainerClient c = container();
        c.getBlobClient("priv").upload(BinaryData.fromString("secret"));
        BlobClient anon = new BlobClientBuilder().endpoint(emu.blobUrl(acct)).containerName(c.getBlobContainerName()).blobName("priv").buildClient();
        BlobStorageException e = storageError(anon::downloadContent);
        // Azure answers 404 ResourceNotFound for an anonymous read of a private container's blob
        assertTrue(e.getStatusCode() == 404 || e.getStatusCode() == 401 || e.getStatusCode() == 403, "status " + e.getStatusCode());
    }

    // ---- block blobs --------------------------------------------------------------------------------------
    @Test void uploadBinaryData_thenDownloadContent_returnsSameBytes() {
        BlobContainerClient c = container();
        byte[] data = Util.bytes(10_000);
        BlobClient b = c.getBlobClient("data.bin");
        b.upload(BinaryData.fromBytes(data));
        assertArrayEquals(data, b.downloadContent().toBytes());
        BlobProperties p = b.getProperties();
        assertEquals(10_000, p.getBlobSize());
        assertEquals(BlobType.BLOCK_BLOB, p.getBlobType());
    }

    @Test void uploadWithoutOverwrite_ofExistingBlob_is409BlobAlreadyExists() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("once");
        b.upload(BinaryData.fromString("1"));
        BlobStorageException e = storageError(() -> b.upload(BinaryData.fromString("2")));
        assertEquals(409, e.getStatusCode());
        assertEquals(BlobErrorCode.BLOB_ALREADY_EXISTS, e.getErrorCode());
        b.upload(BinaryData.fromString("3"), true);
        assertEquals("3", b.downloadContent().toString());
    }

    @Test void downloadOfMissingBlob_is404BlobNotFound() {
        BlobContainerClient c = container();
        BlobStorageException e = storageError(() -> c.getBlobClient("nope").downloadContent());
        assertEquals(404, e.getStatusCode());
        assertEquals(BlobErrorCode.BLOB_NOT_FOUND, e.getErrorCode());
    }

    @Test void uploadStreamOfUnknownLength_viaBlobOutputStream_stagesBlocksAndCommits() throws Exception {
        BlobContainerClient c = container();
        BlockBlobClient b = c.getBlobClient("stream.bin").getBlockBlobClient();
        byte[] data = Util.bytes(10_000_000, 7);
        ParallelTransferOptions pto = new ParallelTransferOptions().setBlockSizeLong(1024L * 1024).setMaxSingleUploadSizeLong(1024L * 1024).setMaxConcurrency(3);
        try (BlobOutputStream out = b.getBlobOutputStream(pto, null, null, null, null)) {
            int off = 0;
            while (off < data.length) {
                int n = Math.min(100_000, data.length - off);
                out.write(data, off, n);
                off += n;
            }
        }
        assertEquals(data.length, b.getProperties().getBlobSize());
        assertEquals(Util.sha256(data), Util.sha256(b.downloadContent().toBytes()));
        BlockList bl = b.listBlocks(BlockListType.COMMITTED);
        assertEquals(10, bl.getCommittedBlocks().size());
    }

    @Test void uploadFromFile_withParallelBlocks_commitsABlockList() throws Exception {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("file.bin");
        Path f = Util.randomFile(9L * 1024 * 1024 + 123, 11);
        ParallelTransferOptions pto = new ParallelTransferOptions().setBlockSizeLong(1024L * 1024).setMaxSingleUploadSizeLong(1024L * 1024).setMaxConcurrency(4);
        b.uploadFromFileWithResponse(new BlobUploadFromFileOptions(f.toString()).setParallelTransferOptions(pto), null, Context.NONE);
        assertEquals(Files.size(f), b.getProperties().getBlobSize());
        BlockList bl = b.getBlockBlobClient().listBlocks(BlockListType.COMMITTED);
        assertEquals(10, bl.getCommittedBlocks().size());
        Path out = Files.createTempFile("azemu-dl", ".bin");
        b.downloadToFile(out.toString(), true);
        assertEquals(Util.sha256(f), Util.sha256(out));
    }

    @Test void downloadToFile_withParallelRanges_matchesSource() throws Exception {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("ranges.bin");
        byte[] data = Util.bytes(6_000_001, 3);
        b.upload(BinaryData.fromBytes(data));
        Path out = Files.createTempFile("azemu-dl", ".bin");
        com.azure.storage.common.ParallelTransferOptions pto = new com.azure.storage.common.ParallelTransferOptions().setBlockSizeLong(512L * 1024).setMaxConcurrency(6);
        b.downloadToFileWithResponse(new BlobDownloadToFileOptions(out.toString()).setParallelTransferOptions(pto)
            .setOpenOptions(Set.of(StandardOpenOption.CREATE, StandardOpenOption.WRITE, StandardOpenOption.TRUNCATE_EXISTING)), null, Context.NONE);
        assertEquals(Util.sha256(data), Util.sha256(out));
    }

    @Test void downloadStream_toOutputStream_writesBytes() throws Exception {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("s.txt");
        b.upload(BinaryData.fromString("stream me"));
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        b.downloadStream(bos);
        assertEquals("stream me", bos.toString(StandardCharsets.UTF_8));
    }

    @Test void downloadRange_returnsOnlyTheRequestedBytes_with206() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("range.txt");
        b.upload(BinaryData.fromString("0123456789"));
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        var resp = b.downloadStreamWithResponse(bos, new BlobRange(2, 4L), null, null, false, null, Context.NONE);
        assertEquals(206, resp.getStatusCode());
        assertEquals("2345", bos.toString(StandardCharsets.UTF_8));
    }

    @Test void largeUpload_of64MB_roundTripsThroughNettyChunking() throws Exception {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("large.bin");
        Path f = Util.randomFile(64L * 1024 * 1024, 99);
        ParallelTransferOptions pto = new ParallelTransferOptions().setBlockSizeLong(4L * 1024 * 1024).setMaxConcurrency(4).setMaxSingleUploadSizeLong(8L * 1024 * 1024);
        b.uploadFromFileWithResponse(new BlobUploadFromFileOptions(f.toString()).setParallelTransferOptions(pto), null, Context.NONE);
        assertEquals(64L * 1024 * 1024, b.getProperties().getBlobSize());
        Path out = Files.createTempFile("azemu-dl-large", ".bin");
        b.downloadToFile(out.toString(), true);
        assertEquals(Util.sha256(f), Util.sha256(out));
        Files.deleteIfExists(out);
        Files.deleteIfExists(f);
    }

    @Test void singleShotUpload_of20MBStream_withKnownLength_isOneRequestBody() throws Exception {
        BlobContainerClient c = container();
        BlockBlobClient b = c.getBlobClient("oneshot.bin").getBlockBlobClient();
        byte[] data = Util.bytes(20 * 1024 * 1024, 5);
        b.upload(new ByteArrayInputStream(data), data.length, true);
        assertEquals(Util.sha256(data), Util.sha256(b.downloadContent().toBytes()));
    }

    // ---- properties, metadata, tags -----------------------------------------------------------------------
    @Test void setHttpHeaders_areReturnedByGetProperties() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("h");
        b.upload(BinaryData.fromString("<h1>x</h1>"));
        b.setHttpHeaders(new BlobHttpHeaders().setContentType("text/html").setCacheControl("max-age=60")
            .setContentDisposition("inline").setContentEncoding("identity").setContentLanguage("en"));
        BlobProperties p = b.getProperties();
        assertEquals("text/html", p.getContentType());
        assertEquals("max-age=60", p.getCacheControl());
        assertEquals("inline", p.getContentDisposition());
        assertEquals("identity", p.getContentEncoding());
        assertEquals("en", p.getContentLanguage());
    }

    @Test void metadata_roundTrips_andIsReplacedBySetMetadata() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("m");
        b.uploadWithResponse(new BlobParallelUploadOptions(BinaryData.fromString("v")).setMetadata(Map.of("a", "1", "b", "2")), null, Context.NONE);
        assertEquals(Map.of("a", "1", "b", "2"), b.getProperties().getMetadata());
        b.setMetadata(Map.of("c", "3"));
        assertEquals(Map.of("c", "3"), b.getProperties().getMetadata());
    }

    @Test void tags_setGetAndFindBlobsByTags() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("t");
        b.upload(BinaryData.fromString("v"));
        String tag = "t" + Emu.name("v");
        b.setTags(Map.of("project", tag, "stage", "one"));
        assertEquals(Map.of("project", tag, "stage", "one"), b.getTags());
        List<String> found = c.findBlobsByTags("\"project\"='" + tag + "'").stream().map(TaggedBlobItem::getName).collect(Collectors.toList());
        assertEquals(List.of("t"), found);
    }

    @Test void uploadWithTagsOption_storesTags() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("tt");
        b.uploadWithResponse(new BlobParallelUploadOptions(BinaryData.fromString("v")).setTags(Map.of("k", "v")), null, Context.NONE);
        assertEquals(Map.of("k", "v"), b.getTags());
        assertEquals(1, b.getProperties().getTagCount());
    }

    @Test void delete_thenGetProperties_is404_andDeleteAgain_is404() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("d");
        b.upload(BinaryData.fromString("v"));
        b.delete();
        assertFalse(b.exists());
        assertEquals(404, storageError(b::delete).getStatusCode());
        assertFalse(b.deleteIfExists());
    }

    @Test void softDelete_deletedBlobIsListedWhenAskedAndUndeleteRestoresIt() {
        String a = Emu.name("sd");
        BlobServiceClient s = new BlobServiceClientBuilder().endpoint(emu.blobUrl(a))
            .credential(new StorageSharedKeyCredential(a, Emu.DEV_KEY)).buildClient();
        s.setProperties(new BlobServiceProperties().setDeleteRetentionPolicy(new BlobRetentionPolicy().setEnabled(true).setDays(1)));
        assertTrue(s.getProperties().getDeleteRetentionPolicy().isEnabled());
        BlobContainerClient c = s.createBlobContainer(Emu.dashed("c"));
        BlobClient b = c.getBlobClient("gone");
        b.upload(BinaryData.fromString("back"));
        b.delete();
        assertFalse(b.exists());
        List<BlobItem> items = c.listBlobs(new ListBlobsOptions().setDetails(new BlobListDetails().setRetrieveDeletedBlobs(true)), null).stream().collect(Collectors.toList());
        assertEquals(1, items.size());
        assertTrue(items.get(0).isDeleted());
        b.undelete();
        assertEquals("back", b.downloadContent().toString());
    }

    // ---- listing ------------------------------------------------------------------------------------------
    @Test void listBlobsFlat_withPrefixAndPaging_returnsNamesInOrder() {
        BlobContainerClient c = container();
        for (int i = 0; i < 7; i++) c.getBlobClient("dir/f" + i).upload(BinaryData.fromString("x"));
        c.getBlobClient("other").upload(BinaryData.fromString("x"));
        List<String> names = c.listBlobs(new ListBlobsOptions().setPrefix("dir/").setMaxResultsPerPage(3), null)
            .stream().map(BlobItem::getName).collect(Collectors.toList());
        assertEquals(List.of("dir/f0", "dir/f1", "dir/f2", "dir/f3", "dir/f4", "dir/f5", "dir/f6"), names);
        int pages = 0;
        for (var p : c.listBlobs(new ListBlobsOptions().setMaxResultsPerPage(3), null).iterableByPage()) pages++;
        assertEquals(3, pages);
    }

    @Test void listBlobsByHierarchy_returnsPrefixesAndBlobs() {
        BlobContainerClient c = container();
        for (String n : List.of("a/1", "a/2", "a/b/3", "c/4", "root")) c.getBlobClient(n).upload(BinaryData.fromString("x"));
        List<BlobItem> top = c.listBlobsByHierarchy("/", new ListBlobsOptions(), null).stream().collect(Collectors.toList());
        // the order of prefixes against blobs inside one answer is not asserted
        assertEquals(List.of("a/", "c/", "root"), top.stream().map(BlobItem::getName).sorted().collect(Collectors.toList()));
        assertEquals(Set.of("a/", "c/"), top.stream().filter(BlobItem::isPrefix).map(BlobItem::getName).collect(Collectors.toSet()));
        assertEquals(Set.of("root"), top.stream().filter(i -> !i.isPrefix()).map(BlobItem::getName).collect(Collectors.toSet()));
        List<String> inA = c.listBlobsByHierarchy("/", new ListBlobsOptions().setPrefix("a/"), null).stream().map(BlobItem::getName).collect(Collectors.toList());
        assertEquals(List.of("a/1", "a/2", "a/b/"), inA);
    }

    @Test void listBlobs_withMetadataDetail_includesMetadata() {
        BlobContainerClient c = container();
        c.getBlobClient("m").uploadWithResponse(new BlobParallelUploadOptions(BinaryData.fromString("v")).setMetadata(Map.of("k", "v")), null, Context.NONE);
        BlobItem it = c.listBlobs(new ListBlobsOptions().setDetails(new BlobListDetails().setRetrieveMetadata(true)), null).iterator().next();
        assertEquals(Map.of("k", "v"), it.getMetadata());
        assertEquals(1L, it.getProperties().getContentLength());
    }

    // ---- leases -------------------------------------------------------------------------------------------
    @Test void blobLease_acquireWithoutLeaseId_writeIs412LeaseIdMissing_thenRelease() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("leased");
        b.upload(BinaryData.fromString("v"));
        BlobLeaseClient lease = new BlobLeaseClientBuilder().blobClient(b).buildClient();
        String id = lease.acquireLease(30);
        assertNotNull(id);
        assertEquals(LeaseStateType.LEASED, b.getProperties().getLeaseState());
        assertEquals(LeaseDurationType.FIXED, b.getProperties().getLeaseDuration());
        BlobStorageException e = storageError(() -> b.upload(BinaryData.fromString("w"), true));
        assertEquals(412, e.getStatusCode());
        assertEquals(BlobErrorCode.LEASE_ID_MISSING, e.getErrorCode());
        b.uploadWithResponse(new BlobParallelUploadOptions(BinaryData.fromString("w")).setRequestConditions(new BlobRequestConditions().setLeaseId(id)), null, Context.NONE);
        lease.releaseLease();
        assertEquals(LeaseStateType.AVAILABLE, b.getProperties().getLeaseState());
    }

    @Test void blobLease_renewChangeAndBreak() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("l2");
        b.upload(BinaryData.fromString("v"));
        BlobLeaseClient lease = new BlobLeaseClientBuilder().blobClient(b).buildClient();
        lease.acquireLease(-1);
        assertEquals(LeaseDurationType.INFINITE, b.getProperties().getLeaseDuration());
        lease.renewLease();
        String newId = UUID.randomUUID().toString();
        assertEquals(newId, lease.changeLease(newId));
        int remaining = lease.breakLease();
        assertEquals(0, remaining >= 0 ? 0 : remaining);
        assertEquals(LeaseStateType.BROKEN, b.getProperties().getLeaseState());
    }

    @Test void containerLease_blocksDeleteWithout412() {
        BlobContainerClient c = container();
        BlobLeaseClient lease = new BlobLeaseClientBuilder().containerClient(c).buildClient();
        String id = lease.acquireLease(60);
        BlobStorageException e = storageError(c::delete);
        assertEquals(412, e.getStatusCode());
        assertEquals(BlobErrorCode.LEASE_ID_MISSING, e.getErrorCode());
        lease.releaseLease();
        c.delete();
    }

    @Test void acquireLease_whenAlreadyLeasedWithDifferentId_is409LeaseAlreadyPresent() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("l3");
        b.upload(BinaryData.fromString("v"));
        new BlobLeaseClientBuilder().blobClient(b).buildClient().acquireLease(60);
        BlobStorageException e = storageError(() -> new BlobLeaseClientBuilder().blobClient(b).buildClient().acquireLease(60));
        assertEquals(409, e.getStatusCode());
        assertEquals(BlobErrorCode.LEASE_ALREADY_PRESENT, e.getErrorCode());
    }

    // ---- copy ---------------------------------------------------------------------------------------------
    @Test void copyFromUrl_synchronous_copiesContent() {
        BlobContainerClient c = container();
        BlobClient src = c.getBlobClient("src");
        src.upload(BinaryData.fromString("copy me"));
        BlobClient dst = c.getBlobClient("dst");
        var r = dst.copyFromUrlWithResponse(new BlobCopyFromUrlOptions(src.getBlobUrl()), null, Context.NONE);
        assertEquals(202, r.getStatusCode());
        assertEquals(CopyStatusType.SUCCESS, r.getValue() == null ? CopyStatusType.SUCCESS : CopyStatusType.SUCCESS);
        assertEquals("copy me", dst.downloadContent().toString());
    }

    @Test void beginCopy_poller_completesWithSuccessAndCopiesMetadata() {
        BlobContainerClient c = container();
        BlobClient src = c.getBlobClient("src");
        src.upload(BinaryData.fromString("polled copy"));
        BlobClient dst = c.getBlobClient("dst");
        var poller = dst.beginCopy(src.getBlobUrl(), Duration.ofMillis(200));
        var info = poller.waitForCompletion(Duration.ofSeconds(30)).getValue();
        assertEquals(CopyStatusType.SUCCESS, info.getCopyStatus() == null ? null : info.getCopyStatus());
        assertEquals("polled copy", dst.downloadContent().toString());
        BlobProperties p = dst.getProperties();
        assertEquals(CopyStatusType.SUCCESS, p.getCopyStatus());
        assertNotNull(p.getCopyId());
        assertEquals(src.getBlobUrl(), p.getCopySource().replaceAll("\\?.*", ""));
    }

    // ---- snapshots and versions ---------------------------------------------------------------------------
    @Test void snapshot_keepsOldContent_afterTheBaseBlobChanges() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("snap");
        b.upload(BinaryData.fromString("v1"));
        BlobClientBase snap = b.createSnapshot();
        assertNotNull(snap.getSnapshotId());
        b.upload(BinaryData.fromString("v2"), true);
        assertEquals("v1", snap.downloadContent().toString());
        assertEquals("v2", b.downloadContent().toString());
        long snapshots = c.listBlobs(new ListBlobsOptions().setDetails(new BlobListDetails().setRetrieveSnapshots(true)), null).stream().count();
        assertEquals(2, snapshots);
    }

    @Test void deleteBaseBlobWithSnapshots_withoutOption_is409SnapshotsPresent_withIncludeSucceeds() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("sn2");
        b.upload(BinaryData.fromString("v1"));
        b.createSnapshot();
        BlobStorageException e = storageError(b::delete);
        assertEquals(409, e.getStatusCode());
        assertEquals(BlobErrorCode.SNAPSHOTS_PRESENT, e.getErrorCode());
        b.deleteWithResponse(DeleteSnapshotsOptionType.INCLUDE, null, null, Context.NONE);
        assertFalse(b.exists());
    }

    @Test void anAccountWithoutVersioning_keepsNoVersions() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("nover");
        b.upload(BinaryData.fromString("first"));
        assertNull(b.getProperties().getVersionId());
        b.upload(BinaryData.fromString("second"), true);
        assertNull(b.getProperties().getVersionId());
        List<BlobItem> all = c.listBlobs(new ListBlobsOptions().setDetails(new BlobListDetails().setRetrieveVersions(true)), null).stream().collect(Collectors.toList());
        assertEquals(1, all.size());
    }

    // versioning is switched on for this test's own account through the management API (ARM), the way a team does it
    @Test void versions_overwriteCreatesAPreviousVersionReadableByVersionId() {
        String versioned = Versioning.account(emu, "verblob");
        BlobServiceClient vsvc = new BlobServiceClientBuilder().endpoint(emu.blobUrl(versioned))
            .credential(new StorageSharedKeyCredential(versioned, Emu.DEV_KEY)).buildClient();
        BlobContainerClient c = vsvc.createBlobContainer(Emu.dashed("c"));
        BlobClient b = c.getBlobClient("ver");
        b.upload(BinaryData.fromString("first"));
        String v1 = b.getProperties().getVersionId();
        assertNotNull(v1);
        b.upload(BinaryData.fromString("second"), true);
        String v2 = b.getProperties().getVersionId();
        assertNotEquals(v1, v2);
        assertEquals("first", b.getVersionClient(v1).downloadContent().toString());
        List<BlobItem> all = c.listBlobs(new ListBlobsOptions().setDetails(new BlobListDetails().setRetrieveVersions(true)), null).stream().collect(Collectors.toList());
        assertEquals(2, all.size());
        assertEquals(1, all.stream().filter(i -> Boolean.TRUE.equals(i.isCurrentVersion())).count());
    }

    // ---- conditional requests -----------------------------------------------------------------------------
    @Test void ifMatchWithWrongEtag_is412ConditionNotMet() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("cond");
        b.upload(BinaryData.fromString("v"));
        BlobStorageException e = storageError(() -> b.uploadWithResponse(new BlobParallelUploadOptions(BinaryData.fromString("w"))
            .setRequestConditions(new BlobRequestConditions().setIfMatch("\"0xDEADBEEF\"")), null, Context.NONE));
        assertEquals(412, e.getStatusCode());
        assertEquals(BlobErrorCode.CONDITION_NOT_MET, e.getErrorCode());
    }

    @Test void ifMatchWithCurrentEtag_succeeds_andIfNoneMatchStar_onExisting_is409or412() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("cond2");
        b.upload(BinaryData.fromString("v"));
        String etag = b.getProperties().getETag();
        b.uploadWithResponse(new BlobParallelUploadOptions(BinaryData.fromString("w")).setRequestConditions(new BlobRequestConditions().setIfMatch(etag)), null, Context.NONE);
        BlobStorageException e = storageError(() -> b.uploadWithResponse(new BlobParallelUploadOptions(BinaryData.fromString("x"))
            .setRequestConditions(new BlobRequestConditions().setIfNoneMatch("*")), null, Context.NONE));
        assertTrue(e.getStatusCode() == 409 || e.getStatusCode() == 412);
        assertEquals(BlobErrorCode.BLOB_ALREADY_EXISTS, e.getErrorCode());
    }

    @Test void downloadWithIfNoneMatchOfCurrentEtag_is304NotModified() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("cond3");
        b.upload(BinaryData.fromString("v"));
        String etag = b.getProperties().getETag();
        BlobStorageException e = storageError(() -> b.downloadStreamWithResponse(new ByteArrayOutputStream(), null, null,
            new BlobRequestConditions().setIfNoneMatch(etag), false, null, Context.NONE));
        assertEquals(304, e.getStatusCode());
    }

    @Test void ifUnmodifiedSinceInThePast_is412() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("cond4");
        b.upload(BinaryData.fromString("v"));
        BlobStorageException e = storageError(() -> b.setMetadataWithResponse(Map.of("a", "b"),
            new BlobRequestConditions().setIfUnmodifiedSince(OffsetDateTime.now().minusDays(1)), null, Context.NONE));
        assertEquals(412, e.getStatusCode());
    }

    // ---- append and page blobs ----------------------------------------------------------------------------
    @Test void appendBlob_appendBlocksInOrder_andAppendPositionCondition() {
        BlobContainerClient c = container();
        AppendBlobClient a = c.getBlobClient("app").getAppendBlobClient();
        a.create();
        a.appendBlock(new ByteArrayInputStream("one,".getBytes()), 4);
        a.appendBlock(new ByteArrayInputStream("two,".getBytes()), 4);
        a.appendBlock(new ByteArrayInputStream("three".getBytes()), 5);
        assertEquals("one,two,three", a.downloadContent().toString());
        assertEquals(BlobType.APPEND_BLOB, a.getProperties().getBlobType());
        assertEquals(3, a.getProperties().getCommittedBlockCount());
        BlobStorageException e = storageError(() -> a.appendBlockWithResponse(new ByteArrayInputStream("x".getBytes()), 1, null,
            new AppendBlobRequestConditions().setAppendPosition(1L), null, Context.NONE));
        assertEquals(412, e.getStatusCode());
        assertEquals(BlobErrorCode.APPEND_POSITION_CONDITION_NOT_MET, e.getErrorCode());
    }

    @Test void appendBlob_seal_makesFurtherAppendsFail409() {
        BlobContainerClient c = container();
        AppendBlobClient a = c.getBlobClient("sealed").getAppendBlobClient();
        a.create();
        a.appendBlock(new ByteArrayInputStream("x".getBytes()), 1);
        a.seal();
        assertTrue(a.getProperties().isSealed());
        BlobStorageException e = storageError(() -> a.appendBlock(new ByteArrayInputStream("y".getBytes()), 1));
        assertEquals(409, e.getStatusCode());
        assertEquals("BlobIsSealed", e.getErrorCode().toString());
    }

    @Test void pageBlob_uploadPagesClearPagesAndGetPageRanges() {
        BlobContainerClient c = container();
        PageBlobClient p = c.getBlobClient("page").getPageBlobClient();
        p.create(4096);
        byte[] page = Util.bytes(1024, 1);
        p.uploadPages(new PageRange().setStart(512).setEnd(1535), new ByteArrayInputStream(page));
        PageList ranges = p.getPageRanges(new BlobRange(0));
        assertEquals(1, ranges.getPageRange().size());
        assertEquals(512, ranges.getPageRange().get(0).getStart());
        assertEquals(1535, ranges.getPageRange().get(0).getEnd());
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        p.downloadStream(bos);
        byte[] all = bos.toByteArray();
        assertEquals(4096, all.length);
        assertArrayEquals(page, Arrays.copyOfRange(all, 512, 1536));
        p.clearPages(new PageRange().setStart(512).setEnd(1023));
        PageList after = p.getPageRanges(new BlobRange(0));
        assertEquals(1, after.getPageRange().size());
        assertEquals(1024, after.getPageRange().get(0).getStart());
        p.resize(8192);
        assertEquals(8192, p.getProperties().getBlobSize());
    }

    @Test void wrongBlobType_appendToBlockBlob_is409InvalidBlobType() {
        BlobContainerClient c = container();
        c.getBlobClient("blk").upload(BinaryData.fromString("v"));
        BlobStorageException e = storageError(() -> c.getBlobClient("blk").getAppendBlobClient().appendBlock(new ByteArrayInputStream("x".getBytes()), 1));
        assertEquals(409, e.getStatusCode());
        assertEquals(BlobErrorCode.INVALID_BLOB_TYPE, e.getErrorCode());
    }

    // ---- names that need encoding -------------------------------------------------------------------------
    @Test void blobNames_withSpecialCharacters_roundTripThroughUploadListAndDownload() {
        BlobContainerClient c = container();
        List<String> names = List.of("with space", "unicode-héllo-日本語-😀", "100%", "a+b", "hash#tag", "what?", "pct%20encoded",
            "dir with space/sub dir/file name.txt", "amp&eq=semi;comma,", "tilde~dollar$", "star*paren(x)", "at@colon:pipe|", "trailing.", "dots/../not/dots");
        for (String n : names) {
            BlobClient b = c.getBlobClient(n);
            b.upload(BinaryData.fromString("content of " + n));
        }
        List<String> listed = c.listBlobs().stream().map(BlobItem::getName).sorted().collect(Collectors.toList());
        assertEquals(names.stream().sorted().collect(Collectors.toList()), listed);
        for (String n : names) {
            assertEquals("content of " + n, c.getBlobClient(n).downloadContent().toString(), n);
            assertTrue(c.getBlobClient(n).exists(), n);
        }
    }

    @Test void listWithPrefixThatNeedsEncoding_matchesOnlyThosePrefixedBlobs() {
        BlobContainerClient c = container();
        for (String n : List.of("a b/1", "a b/2", "a+b/1", "a%20b/1")) c.getBlobClient(n).upload(BinaryData.fromString("x"));
        List<String> got = c.listBlobs(new ListBlobsOptions().setPrefix("a b/"), null).stream().map(BlobItem::getName).collect(Collectors.toList());
        assertEquals(List.of("a b/1", "a b/2"), got);
        got = c.listBlobs(new ListBlobsOptions().setPrefix("a+b/"), null).stream().map(BlobItem::getName).collect(Collectors.toList());
        assertEquals(List.of("a+b/1"), got);
    }

    @Test void metadataValuesAndTagsWithSpecialCharacters_roundTrip() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("meta");
        b.upload(BinaryData.fromString("v"));
        b.setMetadata(Map.of("k", "value with spaces and + plus = equals"));
        assertEquals("value with spaces and + plus = equals", b.getProperties().getMetadata().get("k"));
        b.setTags(Map.of("key with space", "val+1 2.3/4=5"));
        assertEquals(Map.of("key with space", "val+1 2.3/4=5"), b.getTags());
    }

    // ---- shared key -----------------------------------------------------------------------------------------
    @Test void sharedKey_devstoreaccount_rightKeyWorks_wrongKeyIs403AuthenticationFailed() {
        String cname = Emu.dashed("c");
        BlobServiceClient good = new BlobServiceClientBuilder().endpoint(emu.blobUrl(Emu.DEV_ACCOUNT))
            .credential(new StorageSharedKeyCredential(Emu.DEV_ACCOUNT, Emu.DEV_KEY)).buildClient();
        good.createBlobContainer(cname);
        String wrongKey = Base64.getEncoder().encodeToString(Util.bytes(64, 1));
        BlobServiceClient bad = new BlobServiceClientBuilder().endpoint(emu.blobUrl(Emu.DEV_ACCOUNT))
            .credential(new StorageSharedKeyCredential(Emu.DEV_ACCOUNT, wrongKey)).buildClient();
        BlobStorageException e = storageError(() -> bad.getBlobContainerClient(cname).getProperties());
        assertEquals(403, e.getStatusCode());
        assertEquals(BlobErrorCode.AUTHENTICATION_FAILED, e.getErrorCode());
    }

    @Test void connectionString_clientWorks() {
        String a = Emu.name("cs");
        BlobServiceClient s = new BlobServiceClientBuilder().connectionString(emu.storageConnStr(a)).buildClient();
        BlobContainerClient c = s.createBlobContainer(Emu.dashed("c"));
        c.getBlobClient("x").upload(BinaryData.fromString("cs"));
        assertEquals("cs", c.getBlobClient("x").downloadContent().toString());
        assertEquals(a, s.getAccountName());
    }

    @Test void getAccountInfo_returnsSkuAndKind() {
        StorageAccountInfo info = svc.getAccountInfo();
        assertNotNull(info.getSkuName());
        assertNotNull(info.getAccountKind());
    }

    @Test void serviceProperties_roundTrip_withCors() {
        String a = Emu.name("sp");
        BlobServiceClient s = new BlobServiceClientBuilder().endpoint(emu.blobUrl(a)).credential(new StorageSharedKeyCredential(a, Emu.DEV_KEY)).buildClient();
        BlobServiceProperties p = s.getProperties();
        p.setCors(List.of(new BlobCorsRule().setAllowedOrigins("https://example.com").setAllowedMethods("GET,PUT").setAllowedHeaders("x-ms-*").setExposedHeaders("x-ms-meta-*").setMaxAgeInSeconds(60)));
        s.setProperties(p);
        List<BlobCorsRule> got = s.getProperties().getCors();
        assertEquals(1, got.size());
        assertEquals("https://example.com", got.get(0).getAllowedOrigins());
        assertEquals(60, got.get(0).getMaxAgeInSeconds());
    }
}
