package azemu;

import com.azure.core.credential.AccessToken;
import com.azure.core.credential.TokenCredential;
import com.azure.core.util.BinaryData;
import com.azure.core.util.Context;
import com.azure.storage.blob.*;
import com.azure.storage.blob.batch.*;
import com.azure.storage.blob.models.*;
import com.azure.storage.blob.options.*;
import com.azure.storage.blob.sas.*;
import com.azure.storage.common.StorageSharedKeyCredential;
import com.azure.storage.common.sas.*;
import org.junit.jupiter.api.*;
import reactor.core.publisher.Flux;
import reactor.core.publisher.Mono;

import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.time.OffsetDateTime;
import java.util.*;
import java.util.stream.Collectors;

import static org.junit.jupiter.api.Assertions.*;

@EmuTest
class BlobSasBatchAsyncTest {
    static Emu emu;
    static BlobServiceClient dev;      // devstoreaccount1: signatures are checked by the emulator
    static StorageSharedKeyCredential devCred = new StorageSharedKeyCredential(Emu.DEV_ACCOUNT, Emu.DEV_KEY);

    @BeforeAll static void setUp() {
        emu = Emu.current();
        dev = new BlobServiceClientBuilder().endpoint(emu.blobUrl(Emu.DEV_ACCOUNT)).credential(devCred).buildClient();
    }

    BlobContainerClient container() { return dev.createBlobContainer(Emu.dashed("c")); }

    BlobClient withSas(BlobClient b, String sas) {
        return new BlobClientBuilder().endpoint(emu.blobUrl(Emu.DEV_ACCOUNT)).containerName(b.getContainerName()).blobName(b.getBlobName()).sasToken(sas).buildClient();
    }

    static BlobSasPermission read() { return new BlobSasPermission().setReadPermission(true); }

    // ---- SAS ----------------------------------------------------------------------------------------------
    @Test void serviceSas_readOnly_letsASecondClientReadButNotWrite() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("sas.txt");
        b.upload(BinaryData.fromString("sas data"));
        String sas = b.generateSas(new BlobServiceSasSignatureValues(OffsetDateTime.now().plusHours(1), read()));
        BlobClient viaSas = withSas(b, sas);
        assertEquals("sas data", viaSas.downloadContent().toString());
        BlobStorageException e = assertThrows(BlobStorageException.class, () -> viaSas.upload(BinaryData.fromString("x"), true));
        assertEquals(403, e.getStatusCode());
        assertEquals(BlobErrorCode.AUTHORIZATION_PERMISSION_MISMATCH, e.getErrorCode());
    }

    @Test void serviceSas_withWritePermission_letsASecondClientUpload() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("w.txt");
        String sas = b.generateSas(new BlobServiceSasSignatureValues(OffsetDateTime.now().plusHours(1),
            new BlobSasPermission().setCreatePermission(true).setWritePermission(true).setReadPermission(true)));
        withSas(b, sas).upload(BinaryData.fromString("written via sas"));
        assertEquals("written via sas", b.downloadContent().toString());
    }

    @Test void serviceSas_withTamperedPermissions_is403AuthenticationFailed() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("t.txt");
        b.upload(BinaryData.fromString("v"));
        String sas = b.generateSas(new BlobServiceSasSignatureValues(OffsetDateTime.now().plusHours(1), read()));
        String tampered = sas.replace("sp=r", "sp=racwd");
        assertNotEquals(sas, tampered);
        BlobStorageException e = assertThrows(BlobStorageException.class, () -> withSas(b, tampered).delete());
        assertEquals(403, e.getStatusCode());
        assertEquals(BlobErrorCode.AUTHENTICATION_FAILED, e.getErrorCode());
        assertTrue(b.exists());
    }

    @Test void serviceSas_forAnotherBlob_doesNotOpenThisBlob() {
        BlobContainerClient c = container();
        BlobClient a = c.getBlobClient("a.txt");
        BlobClient other = c.getBlobClient("b.txt");
        a.upload(BinaryData.fromString("a"));
        other.upload(BinaryData.fromString("b"));
        String sas = a.generateSas(new BlobServiceSasSignatureValues(OffsetDateTime.now().plusHours(1), read()));
        BlobStorageException e = assertThrows(BlobStorageException.class, () -> withSas(other, sas).downloadContent());
        assertEquals(403, e.getStatusCode());
    }

    @Test void serviceSas_expired_is403() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("exp.txt");
        b.upload(BinaryData.fromString("v"));
        String sas = b.generateSas(new BlobServiceSasSignatureValues(OffsetDateTime.now().minusMinutes(5), read()).setStartTime(OffsetDateTime.now().minusHours(1)));
        BlobStorageException e = assertThrows(BlobStorageException.class, () -> withSas(b, sas).downloadContent());
        assertEquals(403, e.getStatusCode());
    }

    @Test void containerSas_listAndRead_worksForAnyBlobInTheContainer() {
        BlobContainerClient c = container();
        c.getBlobClient("one").upload(BinaryData.fromString("1"));
        c.getBlobClient("two").upload(BinaryData.fromString("2"));
        String sas = c.generateSas(new BlobServiceSasSignatureValues(OffsetDateTime.now().plusHours(1),
            new BlobContainerSasPermission().setReadPermission(true).setListPermission(true)));
        BlobContainerClient viaSas = new BlobContainerClientBuilder().endpoint(emu.blobUrl(Emu.DEV_ACCOUNT)).containerName(c.getBlobContainerName()).sasToken(sas).buildClient();
        assertEquals(List.of("one", "two"), viaSas.listBlobs().stream().map(BlobItem::getName).collect(Collectors.toList()));
        assertEquals("2", viaSas.getBlobClient("two").downloadContent().toString());
    }

    @Test void accountSas_createsContainerAndBlobWithServiceResourceTypeAndPermissions() {
        AccountSasSignatureValues v = new AccountSasSignatureValues(OffsetDateTime.now().plusHours(1),
            new AccountSasPermission().setReadPermission(true).setWritePermission(true).setCreatePermission(true).setListPermission(true).setDeletePermission(true),
            new AccountSasService().setBlobAccess(true),
            new AccountSasResourceType().setContainer(true).setObject(true).setService(true));
        String sas = dev.generateAccountSas(v);
        BlobServiceClient viaSas = new BlobServiceClientBuilder().endpoint(emu.blobUrl(Emu.DEV_ACCOUNT)).sasToken(sas).buildClient();
        String name = Emu.dashed("c");
        BlobContainerClient c = viaSas.createBlobContainer(name);
        c.getBlobClient("x").upload(BinaryData.fromString("acct sas"));
        assertEquals("acct sas", c.getBlobClient("x").downloadContent().toString());
        assertTrue(viaSas.listBlobContainers().stream().anyMatch(i -> i.getName().equals(name)));
    }

    @Test void accountSas_withoutDeletePermission_cannotDelete() {
        AccountSasSignatureValues v = new AccountSasSignatureValues(OffsetDateTime.now().plusHours(1),
            new AccountSasPermission().setReadPermission(true).setWritePermission(true).setCreatePermission(true),
            new AccountSasService().setBlobAccess(true),
            new AccountSasResourceType().setContainer(true).setObject(true));
        BlobServiceClient viaSas = new BlobServiceClientBuilder().endpoint(emu.blobUrl(Emu.DEV_ACCOUNT)).sasToken(dev.generateAccountSas(v)).buildClient();
        BlobContainerClient c = viaSas.createBlobContainer(Emu.dashed("c"));
        c.getBlobClient("x").upload(BinaryData.fromString("v"));
        BlobStorageException e = assertThrows(BlobStorageException.class, () -> c.getBlobClient("x").delete());
        assertEquals(403, e.getStatusCode());
    }

    @Test void storedAccessPolicy_sasByIdentifier_followsThePolicyPermissions() {
        BlobContainerClient c = container();
        c.setAccessPolicy(null, List.of(new BlobSignedIdentifier().setId("readonly")
            .setAccessPolicy(new BlobAccessPolicy().setPermissions("r").setExpiresOn(OffsetDateTime.now().plusHours(1)))));
        assertEquals("readonly", c.getAccessPolicy().getIdentifiers().get(0).getId());
        BlobClient b = c.getBlobClient("p.txt");
        b.upload(BinaryData.fromString("policy"));
        String sas = b.generateSas(new BlobServiceSasSignatureValues("readonly"));
        assertEquals("policy", withSas(b, sas).downloadContent().toString());
        assertThrows(BlobStorageException.class, () -> withSas(b, sas).delete());
    }

    @Test void userDelegationSas_signedWithATokenCredentialKey_isAcceptedByASecondClient() {
        // bearer tokens need https; the emulator accepts any token
        TokenCredential cred = req -> Mono.just(new AccessToken("any-token", OffsetDateTime.now().plusHours(1)));
        String acct = Emu.name("udk");
        BlobServiceClient oauth = new BlobServiceClientBuilder().endpoint(emu.https("blob", acct)).credential(cred).buildClient();
        BlobContainerClient c = oauth.createBlobContainer(Emu.dashed("c"));
        BlobClient b = c.getBlobClient("ud.txt");
        b.upload(BinaryData.fromString("user delegation"));
        UserDelegationKey key = oauth.getUserDelegationKey(OffsetDateTime.now().minusMinutes(1), OffsetDateTime.now().plusHours(1));
        assertNotNull(key.getValue());
        String sas = b.generateUserDelegationSas(new BlobServiceSasSignatureValues(OffsetDateTime.now().plusHours(1), read()), key);
        assertTrue(sas.contains("skoid="));
        BlobClient second = new BlobClientBuilder().endpoint(emu.https("blob", acct)).containerName(c.getBlobContainerName()).blobName("ud.txt").sasToken(sas).buildClient();
        assertEquals("user delegation", second.downloadContent().toString());
    }

    @Test void tokenCredentialOverPlainHttp_isRefusedByTheSdkBeforeAnyRequest() {
        TokenCredential cred = req -> Mono.just(new AccessToken("t", OffsetDateTime.now().plusHours(1)));
        // SDK limit (not the emulator): "token credentials require a URL using the HTTPS protocol scheme"
        assertThrows(IllegalArgumentException.class, () -> new BlobServiceClientBuilder().endpoint(emu.blobUrl("x")).credential(cred).buildClient());
    }

    // ---- batch --------------------------------------------------------------------------------------------
    @Test void batchDelete_ofSeveralBlobs_returnsAnAcceptedResponsePerBlob() {
        BlobContainerClient c = container();
        List<String> urls = new ArrayList<>();
        for (int i = 0; i < 5; i++) {
            BlobClient b = c.getBlobClient("b" + i);
            b.upload(BinaryData.fromString("x"));
            urls.add(b.getBlobUrl());
        }
        BlobBatchClient batch = new BlobBatchClientBuilder(dev).buildClient();
        List<Integer> codes = batch.deleteBlobs(urls, DeleteSnapshotsOptionType.INCLUDE).stream().map(r -> r.getStatusCode()).collect(Collectors.toList());
        assertEquals(List.of(202, 202, 202, 202, 202), codes);
        assertEquals(0, c.listBlobs().stream().count());
    }

    @Test void batchDelete_withAMissingBlob_reportsThatSubRequestAs404WithoutFailingTheRest() {
        BlobContainerClient c = container();
        c.getBlobClient("here").upload(BinaryData.fromString("x"));
        BlobBatchClient client = new BlobBatchClientBuilder(dev).buildClient();
        BlobBatch batch = client.getBlobBatch();
        var ok = batch.deleteBlob(c.getBlobContainerName(), "here");
        var missing = batch.deleteBlob(c.getBlobContainerName(), "not-there");
        client.submitBatchWithResponse(batch, false, null, Context.NONE);
        assertEquals(202, ok.getStatusCode());
        // getStatusCode() of a failed sub-request throws the BlobStorageException the sub-response carried
        BlobStorageException e = assertThrows(BlobStorageException.class, missing::getStatusCode);
        assertEquals(404, e.getStatusCode());
    }

    @Test void batchDelete_withDefaultThrowOnAnyFailure_throwsBlobBatchStorageException() {
        BlobContainerClient c = container();
        BlobBatchClient client = new BlobBatchClientBuilder(dev).buildClient();
        BlobBatch batch = client.getBlobBatch();
        batch.deleteBlob(c.getBlobContainerName(), "not-there");
        BlobBatchStorageException e = assertThrows(BlobBatchStorageException.class, () -> client.submitBatch(batch));
        List<BlobStorageException> inner = new ArrayList<>();
        e.getBatchExceptions().forEach(inner::add);
        assertEquals(1, inner.size());
        assertEquals(404, inner.get(0).getStatusCode());
        assertEquals(BlobErrorCode.BLOB_NOT_FOUND, inner.get(0).getErrorCode());
    }

    @Test void batchDelete_isAppliedByTheServer_evenWhenTheSdkCannotReadTheAnswer() {
        // the answer is B1-broken for the Java client, the work itself is done: this pins the server side of the batch
        BlobContainerClient c = container();
        BlobBatchClient client = new BlobBatchClientBuilder(dev).buildClient();
        BlobBatch batch = client.getBlobBatch();
        for (int i = 0; i < 3; i++) {
            c.getBlobClient("d" + i).upload(BinaryData.fromString("x"));
            batch.deleteBlob(c.getBlobContainerName(), "d" + i);
        }
        c.getBlobClient("keep").upload(BinaryData.fromString("x"));
        try { client.submitBatchWithResponse(batch, false, null, Context.NONE); } catch (RuntimeException ignored) { }
        assertEquals(List.of("keep"), c.listBlobs().stream().map(BlobItem::getName).collect(Collectors.toList()));
    }

    @Test void batchSetTier_changesTheAccessTier() {
        BlobContainerClient c = container();
        BlobClient b = c.getBlobClient("tier");
        b.upload(BinaryData.fromString("x"));
        BlobBatchClient client = new BlobBatchClientBuilder(dev).buildClient();
        BlobBatch batch = client.getBlobBatch();
        batch.setBlobAccessTier(c.getBlobContainerName(), "tier", AccessTier.COOL);
        client.submitBatch(batch);
        assertEquals(AccessTier.COOL, b.getProperties().getAccessTier());
    }

    // ---- async clients ------------------------------------------------------------------------------------
    @Test void asyncClient_uploadFluxOfUnknownLength_downloadFlux_roundTrips() {
        BlobServiceAsyncClient svc = new BlobServiceClientBuilder().endpoint(emu.blobUrl(Emu.name("as"))).credential(devCred).buildAsyncClient();
        // credential for a different account name would not match; use a connection string for that account instead
        String a = Emu.name("as");
        svc = new BlobServiceClientBuilder().endpoint(emu.blobUrl(a)).credential(new StorageSharedKeyCredential(a, Emu.DEV_KEY)).buildAsyncClient();
        BlobContainerAsyncClient c = svc.createBlobContainer(Emu.dashed("c")).block();
        BlobAsyncClient b = c.getBlobAsyncClient("flux.bin");
        byte[] data = Util.bytes(5_000_000, 21);
        Flux<ByteBuffer> flux = Flux.range(0, 50).map(i -> ByteBuffer.wrap(data, i * 100_000, 100_000));
        b.upload(flux, new ParallelTransferOptions().setBlockSizeLong(1024L * 1024).setMaxSingleUploadSizeLong(1024L * 1024), true).block();
        assertEquals(5_000_000, b.getProperties().block().getBlobSize());
        byte[] got = b.downloadContent().block().toBytes();
        assertEquals(Util.sha256(data), Util.sha256(got));
        byte[] viaFlux = Flux.from(b.downloadStream()).map(bb -> { byte[] x = new byte[bb.remaining()]; bb.get(x); return x; })
            .reduce(new byte[0], (x, y) -> { byte[] z = Arrays.copyOf(x, x.length + y.length); System.arraycopy(y, 0, z, x.length, y.length); return z; }).block();
        assertEquals(Util.sha256(data), Util.sha256(viaFlux));
    }

    @Test void asyncClient_errorsArriveAsBlobStorageExceptionInTheMono() {
        String a = Emu.name("ase");
        BlobServiceAsyncClient svc = new BlobServiceClientBuilder().endpoint(emu.blobUrl(a)).credential(new StorageSharedKeyCredential(a, Emu.DEV_KEY)).buildAsyncClient();
        BlobStorageException e = assertThrows(BlobStorageException.class, () -> svc.getBlobContainerAsyncClient(Emu.dashed("none")).getProperties().block());
        assertEquals(404, e.getStatusCode());
    }

    @Test void asyncClient_listBlobsPagedFlux_andConcurrentUploads() {
        String a = Emu.name("asl");
        BlobServiceAsyncClient svc = new BlobServiceClientBuilder().endpoint(emu.blobUrl(a)).credential(new StorageSharedKeyCredential(a, Emu.DEV_KEY)).buildAsyncClient();
        BlobContainerAsyncClient c = svc.createBlobContainer(Emu.dashed("c")).block();
        Flux.range(0, 20).flatMap(i -> c.getBlobAsyncClient("n" + i).upload(BinaryData.fromString("v" + i)), 8).blockLast();
        List<String> names = c.listBlobs().map(BlobItem::getName).collectList().block();
        assertEquals(20, names.size());
        assertEquals("v7", c.getBlobAsyncClient("n7").downloadContent().block().toString());
    }

    @Test void asyncClient_uploadFromFileAndDownloadToFile_with40MB() throws Exception {
        String a = Emu.name("asf");
        BlobServiceAsyncClient svc = new BlobServiceClientBuilder().endpoint(emu.blobUrl(a)).credential(new StorageSharedKeyCredential(a, Emu.DEV_KEY)).buildAsyncClient();
        BlobAsyncClient b = svc.createBlobContainer(Emu.dashed("c")).block().getBlobAsyncClient("big.bin");
        java.nio.file.Path f = Util.randomFile(40L * 1024 * 1024, 5);
        b.uploadFromFile(f.toString(), true).block();
        java.nio.file.Path out = java.nio.file.Files.createTempFile("azemu-async", ".bin");
        b.downloadToFile(out.toString(), true).block();
        assertEquals(Util.sha256(f), Util.sha256(out));
        java.nio.file.Files.deleteIfExists(out);
        java.nio.file.Files.deleteIfExists(f);
    }
}
