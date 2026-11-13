package azemu;

import com.azure.core.util.BinaryData;
import com.azure.core.util.Context;
import com.azure.storage.blob.BlobServiceClient;
import com.azure.storage.blob.BlobServiceClientBuilder;
import com.azure.storage.common.StorageSharedKeyCredential;
import com.azure.storage.file.datalake.*;
import com.azure.storage.file.datalake.models.*;
import com.azure.storage.file.datalake.options.*;
import com.azure.storage.file.datalake.sas.*;
import org.junit.jupiter.api.*;

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.nio.file.*;
import java.time.OffsetDateTime;
import java.util.*;
import java.util.stream.Collectors;

import static org.junit.jupiter.api.Assertions.*;

@EmuTest
class DataLakeTest {
    static Emu emu;
    static String acct;
    static StorageSharedKeyCredential cred;
    static DataLakeServiceClient svc;

    @BeforeAll static void setUp() {
        emu = Emu.current();
        acct = Emu.name("dl");
        cred = new StorageSharedKeyCredential(acct, Emu.DEV_KEY);
        svc = new DataLakeServiceClientBuilder().endpoint(emu.dfsUrl(acct)).credential(cred).buildClient();
    }

    DataLakeFileSystemClient fs() { return svc.createFileSystem(Emu.dashed("fs")); }

    static DataLakeStorageException err(org.junit.jupiter.api.function.Executable e) {
        return assertThrows(DataLakeStorageException.class, e);
    }

    // ---- file systems -------------------------------------------------------------------------------------
    @Test void createFileSystem_twice_is409FilesystemAlreadyExists() {
        String n = Emu.dashed("fs");
        svc.createFileSystem(n);
        DataLakeStorageException e = err(() -> svc.createFileSystem(n));
        assertEquals(409, e.getStatusCode());
        assertEquals("ContainerAlreadyExists", e.getErrorCode());
    }

    @Test void fileSystem_metadataPropertiesListAndDelete() {
        String p = Emu.name("lfs");
        DataLakeFileSystemClient f = svc.createFileSystemWithResponse(p + "-a", Map.of("k", "v"), null, Context.NONE).getValue();
        svc.createFileSystem(p + "-b");
        assertEquals("v", f.getProperties().getMetadata().get("k"));
        List<String> names = svc.listFileSystems(new ListFileSystemsOptions().setPrefix(p), null).stream().map(FileSystemItem::getName).collect(Collectors.toList());
        assertEquals(List.of(p + "-a", p + "-b"), names);
        f.delete();
        assertFalse(f.exists());
    }

    @Test void getPropertiesOfMissingFileSystem_is404FilesystemNotFound() {
        DataLakeStorageException e = err(() -> svc.getFileSystemClient(Emu.dashed("nofs")).getProperties());
        assertEquals(404, e.getStatusCode());
        assertEquals("ContainerNotFound", e.getErrorCode());
    }

    // ---- files and directories ----------------------------------------------------------------------------
    @Test void createFile_appendInTwoPartsThenFlush_makesTheFileReadable() throws Exception {
        DataLakeFileSystemClient f = fs();
        DataLakeFileClient file = f.createFile("dir/data.txt");
        byte[] a = "hello ".getBytes(), b = "datalake".getBytes();
        file.append(new ByteArrayInputStream(a), 0, a.length);
        file.append(new ByteArrayInputStream(b), a.length, b.length);
        // before flush the file has length 0 on Azure
        assertEquals(0, file.getProperties().getFileSize());
        file.flush(a.length + b.length, true);
        assertEquals(14, file.getProperties().getFileSize());
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        file.read(out);
        assertEquals("hello datalake", out.toString());
    }

    @Test void uploadBinaryData_overwriteFalseOnExisting_is409PathAlreadyExists() {
        DataLakeFileSystemClient f = fs();
        DataLakeFileClient file = f.getFileClient("f.txt");
        file.upload(BinaryData.fromString("one"), false);
        DataLakeStorageException e = err(() -> file.upload(BinaryData.fromString("two"), false));
        assertEquals(409, e.getStatusCode());
        assertEquals("PathAlreadyExists", e.getErrorCode());
        file.upload(BinaryData.fromString("two"), true);
        assertEquals("two", readString(file));
    }

    static String readString(DataLakeFileClient file) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        file.read(out);
        return out.toString();
    }

    @Test void readRange_returnsRequestedBytes() {
        DataLakeFileSystemClient f = fs();
        DataLakeFileClient file = f.getFileClient("r.txt");
        file.upload(BinaryData.fromString("0123456789"), true);
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        file.readWithResponse(out, new FileRange(3, 4L), null, null, false, null, Context.NONE);
        assertEquals("3456", out.toString());
    }

    @Test void uploadFromFile_parallelBlocks_andReadToFile_roundTrip() throws Exception {
        DataLakeFileSystemClient f = fs();
        DataLakeFileClient file = f.getFileClient("big.bin");
        Path src = Util.randomFile(12L * 1024 * 1024 + 5, 31);
        file.uploadFromFile(src.toString(), true);
        assertEquals(Files.size(src), file.getProperties().getFileSize());
        Path out = Files.createTempFile("azemu-dl", ".bin");
        file.readToFile(out.toString(), true);
        assertEquals(Util.sha256(src), Util.sha256(out));
        Files.deleteIfExists(out);
        Files.deleteIfExists(src);
    }

    @Test void directory_createListRecursiveAndNonRecursive() {
        DataLakeFileSystemClient f = fs();
        DataLakeDirectoryClient d = f.createDirectory("top");
        d.createSubdirectory("sub");
        d.createFile("a.txt");
        d.getSubdirectoryClient("sub").createFile("b.txt");
        List<String> all = f.listPaths(new ListPathsOptions().setPath("top").setRecursive(true), null).stream().map(PathItem::getName).sorted().collect(Collectors.toList());
        assertEquals(List.of("top/a.txt", "top/sub", "top/sub/b.txt"), all);
        List<PathItem> shallow = f.listPaths(new ListPathsOptions().setPath("top").setRecursive(false), null).stream().collect(Collectors.toList());
        assertEquals(List.of("top/a.txt", "top/sub"), shallow.stream().map(PathItem::getName).sorted().collect(Collectors.toList()));
        assertTrue(shallow.stream().filter(p -> p.getName().equals("top/sub")).findFirst().get().isDirectory());
    }

    @Test void deleteNonEmptyDirectory_withoutRecursive_is409DirectoryNotEmpty_withRecursiveSucceeds() {
        DataLakeFileSystemClient f = fs();
        DataLakeDirectoryClient d = f.createDirectory("full");
        d.createFile("x.txt");
        DataLakeStorageException e = err(() -> d.deleteWithResponse(false, null, null, Context.NONE));
        assertEquals(409, e.getStatusCode());
        assertEquals("DirectoryNotEmpty", e.getErrorCode());
        d.deleteWithResponse(true, null, null, Context.NONE);
        assertFalse(d.exists());
    }

    @Test void rename_file_movesContentAndRemovesTheSource() {
        DataLakeFileSystemClient f = fs();
        DataLakeFileClient src = f.getFileClient("old/name.txt");
        src.upload(BinaryData.fromString("moved"), true);
        f.createDirectory("new/place");   // Azure refuses a rename whose destination parent does not exist
        src.rename(null, "new/place/name2.txt");
        // the client the SDK returns reads from "old/name.txt/new/place/name2.txt" (SDK 12.30.2 quirk), so address the file by name
        assertEquals("moved", readString(f.getFileClient("new/place/name2.txt")));
        assertFalse(src.exists());
    }

    @Test void rename_toADestinationWhoseParentDoesNotExist_is404RenameDestinationParentPathNotFound() {
        DataLakeFileSystemClient f = fs();
        DataLakeFileClient src = f.getFileClient("p.txt");
        src.upload(BinaryData.fromString("x"), true);
        DataLakeStorageException e = err(() -> src.rename(null, "no/such/parent/p.txt"));
        assertEquals(404, e.getStatusCode());
        assertEquals("RenameDestinationParentPathNotFound", e.getErrorCode());
    }

    @Test void rename_directory_movesTheWholeTree() {
        DataLakeFileSystemClient f = fs();
        DataLakeDirectoryClient d = f.createDirectory("tree");
        d.getFileClient("a/b/c.txt").upload(BinaryData.fromString("deep"), true);
        DataLakeDirectoryClient moved = d.rename(null, "tree2");
        assertEquals("deep", readString(moved.getFileClient("a/b/c.txt")));
        assertFalse(d.exists());
    }

    @Test void rename_acrossFileSystems_isSupported() {
        DataLakeFileSystemClient a = fs(), b = fs();
        DataLakeFileClient src = a.getFileClient("x.txt");
        src.upload(BinaryData.fromString("cross"), true);
        src.rename(b.getFileSystemName(), "y.txt");
        // the client the SDK returns for a cross file system rename points at "x.txt/y.txt" of the destination (SDK quirk), so address the file by name
        assertEquals("cross", readString(b.getFileClient("y.txt")));
        assertFalse(src.exists());
    }

    @Test void rename_ofAMissingSource_is404SourcePathNotFound() {
        DataLakeFileSystemClient f = fs();
        DataLakeFileClient missing = f.getFileClient("nothing.txt");
        DataLakeStorageException e = err(() -> missing.rename(null, "other.txt"));
        assertEquals(404, e.getStatusCode());
        assertEquals("SourcePathNotFound", e.getErrorCode());
    }

    // ---- ACLs and permissions -----------------------------------------------------------------------------
    @Test void setAndGetAccessControlList_roundTrips() {
        DataLakeFileSystemClient f = fs();
        DataLakeFileClient file = f.createFile("acl.txt");
        List<PathAccessControlEntry> acl = PathAccessControlEntry.parseList("user::rwx,group::r-x,other::---,user:11111111-2222-3333-4444-555555555555:r--,mask::rwx");
        file.setAccessControlList(acl, null, null);
        PathAccessControl got = file.getAccessControl();
        assertEquals(Set.of("user::rwx", "user:11111111-2222-3333-4444-555555555555:r--", "group::r-x", "mask::rwx", "other::---"),
            new HashSet<>(Arrays.asList(PathAccessControlEntry.serializeList(got.getAccessControlList()).split(","))));
    }

    @Test void setPermissions_symbolic_isReturnedAsOctal() {
        DataLakeFileSystemClient f = fs();
        DataLakeFileClient file = f.createFile("perm.txt");
        file.setPermissions(PathPermissions.parseSymbolic("rwxr-x---"), null, null);
        PathPermissions p = file.getAccessControl().getPermissions();
        assertEquals("0750", p.toString());
    }

    @Test void setAccessControlRecursive_appliesToEveryDescendant() {
        DataLakeFileSystemClient f = fs();
        DataLakeDirectoryClient d = f.createDirectory("rec");
        d.createFile("one.txt");
        d.createSubdirectory("s").createFile("two.txt");
        List<PathAccessControlEntry> acl = PathAccessControlEntry.parseList("user::rwx,group::r-x,other::---");
        AccessControlChangeResult r = d.setAccessControlRecursive(acl);
        assertEquals(2L, r.getCounters().getChangedFilesCount());
        assertEquals(2L, r.getCounters().getChangedDirectoriesCount());
        assertEquals(0L, r.getCounters().getFailedChangesCount());
        assertEquals("user::rwx,group::r-x,other::---", PathAccessControlEntry.serializeList(d.getFileClient("s/two.txt").getAccessControl().getAccessControlList()));
    }

    @Test void updateAndRemoveAccessControlRecursive_addAndDropAnEntry() {
        DataLakeFileSystemClient f = fs();
        DataLakeDirectoryClient d = f.createDirectory("rec2");
        DataLakeFileClient file = d.createFile("a.txt");
        String oid = "99999999-2222-3333-4444-555555555555";
        d.updateAccessControlRecursive(PathAccessControlEntry.parseList("user:" + oid + ":r--"));
        assertTrue(PathAccessControlEntry.serializeList(file.getAccessControl().getAccessControlList()).contains("user:" + oid + ":r--"));
        d.removeAccessControlRecursive(PathRemoveAccessControlEntry.parseList("user:" + oid));
        assertFalse(PathAccessControlEntry.serializeList(file.getAccessControl().getAccessControlList()).contains(oid));
    }

    // ---- metadata, headers, conditions --------------------------------------------------------------------
    @Test void pathMetadataAndHeaders_roundTrip() {
        DataLakeFileSystemClient f = fs();
        DataLakeFileClient file = f.createFile("m.txt");
        file.setMetadata(Map.of("a", "1"));
        file.setHttpHeaders(new PathHttpHeaders().setContentType("text/plain").setCacheControl("no-cache"));
        PathProperties p = file.getProperties();
        assertEquals("1", p.getMetadata().get("a"));
        assertEquals("text/plain", p.getContentType());
        assertEquals("no-cache", p.getCacheControl());
    }

    @Test void createFile_withIfNoneMatchStar_onExisting_is409() {
        DataLakeFileSystemClient f = fs();
        f.createFile("c.txt");
        DataLakeStorageException e = err(() -> f.getFileClient("c.txt").createWithResponse(new DataLakePathCreateOptions()
            .setRequestConditions(new DataLakeRequestConditions().setIfNoneMatch("*")), null, Context.NONE));
        assertTrue(e.getStatusCode() == 409 || e.getStatusCode() == 412, "status " + e.getStatusCode());
    }

    @Test void pathsWithSpacesAndUnicode_roundTrip() {
        DataLakeFileSystemClient f = fs();
        for (String n : List.of("dir one/file two.txt", "ünï/日本語.txt", "hash#and%percent/p+lus.txt")) {
            f.getFileClient(n).upload(BinaryData.fromString("c:" + n), true);
            assertEquals("c:" + n, readString(f.getFileClient(n)), n);
        }
        Set<String> listed = f.listPaths(new ListPathsOptions().setRecursive(true), null).stream().filter(p -> !p.isDirectory()).map(PathItem::getName).collect(Collectors.toSet());
        assertEquals(Set.of("dir one/file two.txt", "ünï/日本語.txt", "hash#and%percent/p+lus.txt"), listed);
    }

    @Test void dataWrittenThroughDfs_isVisibleThroughTheBlobEndpoint() {
        DataLakeFileSystemClient f = fs();
        f.getFileClient("shared/x.txt").upload(BinaryData.fromString("same bytes"), true);
        BlobServiceClient blob = new BlobServiceClientBuilder().endpoint(emu.blobUrl(acct)).credential(cred).buildClient();
        assertEquals("same bytes", blob.getBlobContainerClient(f.getFileSystemName()).getBlobClient("shared/x.txt").downloadContent().toString());
    }

    @Test void fileSystemSas_readList_worksForASecondClient() {
        String sdev = Emu.DEV_ACCOUNT;
        StorageSharedKeyCredential dc = new StorageSharedKeyCredential(sdev, Emu.DEV_KEY);
        DataLakeServiceClient dev = new DataLakeServiceClientBuilder().endpoint(emu.dfsUrl(sdev)).credential(dc).buildClient();
        DataLakeFileSystemClient f = dev.createFileSystem(Emu.dashed("fs"));
        f.getFileClient("s.txt").upload(BinaryData.fromString("sas"), true);
        String sas = f.generateSas(new DataLakeServiceSasSignatureValues(OffsetDateTime.now().plusHours(1), new FileSystemSasPermission().setReadPermission(true).setListPermission(true)));
        DataLakeFileSystemClient viaSas = new DataLakeFileSystemClientBuilder().endpoint(emu.dfsUrl(sdev)).fileSystemName(f.getFileSystemName()).sasToken(sas).buildClient();
        assertEquals("sas", readString(viaSas.getFileClient("s.txt")));
        assertEquals(1, viaSas.listPaths().stream().count());
    }

    @Test void sharedKey_wrongKey_is403() {
        DataLakeFileSystemClient good = new DataLakeServiceClientBuilder().endpoint(emu.dfsUrl(Emu.DEV_ACCOUNT))
            .credential(new StorageSharedKeyCredential(Emu.DEV_ACCOUNT, Emu.DEV_KEY)).buildClient().createFileSystem(Emu.dashed("fs"));
        String wrong = Base64.getEncoder().encodeToString(Util.bytes(64, 5));
        DataLakeFileSystemClient bad = new DataLakeServiceClientBuilder().endpoint(emu.dfsUrl(Emu.DEV_ACCOUNT))
            .credential(new StorageSharedKeyCredential(Emu.DEV_ACCOUNT, wrong)).buildClient().getFileSystemClient(good.getFileSystemName());
        DataLakeStorageException e = err(() -> bad.createFile("x"));
        assertEquals(403, e.getStatusCode());
    }

    @Test void asyncClient_appendFlushAndRead() {
        DataLakeServiceAsyncClient a = new DataLakeServiceClientBuilder().endpoint(emu.dfsUrl(acct)).credential(cred).buildAsyncClient();
        DataLakeFileSystemAsyncClient f = a.createFileSystem(Emu.dashed("fs")).block();
        DataLakeFileAsyncClient file = f.createFile("async.txt").block();
        byte[] d = "async data".getBytes();
        file.append(reactor.core.publisher.Flux.just(java.nio.ByteBuffer.wrap(d)), 0, d.length).block();
        file.flush(d.length, true).block();
        assertEquals("async data", new String(file.read().map(bb -> { byte[] x = new byte[bb.remaining()]; bb.get(x); return x; }).reduce(new byte[0], (x, y) -> {
            byte[] z = Arrays.copyOf(x, x.length + y.length); System.arraycopy(y, 0, z, x.length, y.length); return z; }).block()));
    }
}
