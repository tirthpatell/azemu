package datalake

import (
	"bytes"
	"context"
	"crypto/rand"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/streaming"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/blockblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azdatalake"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azdatalake/datalakeerror"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azdatalake/file"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azdatalake/filesystem"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azdatalake/sas"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azdatalake/service"
)

func TestMain(m *testing.M) { emu.Main(m) }

func cred(t testing.TB) *azdatalake.SharedKeyCredential {
	t.Helper()
	c, err := azdatalake.NewSharedKeyCredential(emu.DevAccount, emu.DevKey)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func dfsURL() string { return emu.E.HTTP("dfs", emu.DevAccount) }

func newFS(t testing.TB, prefix string) *filesystem.Client {
	t.Helper()
	name := emu.Name(prefix)
	fs, err := filesystem.NewClientWithSharedKeyCredential(dfsURL()+"/"+name, cred(t), nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := fs.Create(context.Background(), nil); err != nil {
		t.Fatalf("create filesystem: %v", err)
	}
	return fs
}

func wantCode(t testing.TB, err error, status int, code datalakeerror.StorageErrorCode) {
	t.Helper()
	if err == nil {
		t.Fatalf("expected %d %s, got success", status, code)
	}
	var re *azcore.ResponseError
	if !errors.As(err, &re) {
		t.Fatalf("not a ResponseError: %v", err)
	}
	if re.StatusCode != status {
		t.Fatalf("status %d, want %d: %v", re.StatusCode, status, err)
	}
	if !datalakeerror.HasCode(err, code) {
		t.Fatalf("code %q, want %q", re.ErrorCode, code)
	}
}

func randBytes(n int) []byte { b := make([]byte, n); rand.Read(b); return b }

func readAll(t testing.TB, f *file.Client) []byte {
	t.Helper()
	r, err := f.DownloadStream(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Body.Close()
	b, err := io.ReadAll(r.Body)
	if err != nil {
		t.Fatal(err)
	}
	return b
}

// upload creates the file and uploads data in chunks (the SDK's upload functions do not create the file).
func upload(t testing.TB, f *file.Client, data []byte, o *file.UploadBufferOptions) {
	t.Helper()
	if _, err := f.Create(context.Background(), nil); err != nil {
		t.Fatalf("create %s: %v", f.DFSURL(), err)
	}
	if err := f.UploadBuffer(context.Background(), data, o); err != nil {
		t.Fatalf("upload: %v", err)
	}
}

func ptr[T any](v T) *T { return &v }

func TestFileSystem_CreateTwiceIsFilesystemAlreadyExists409(t *testing.T) {
	fs := newFS(t, "fsdup")
	_, err := fs.Create(context.Background(), nil)
	wantCode(t, err, http.StatusConflict, datalakeerror.FileSystemAlreadyExists)
}

func TestFileSystem_PropertiesOfAMissingOneIsFilesystemNotFound404(t *testing.T) {
	fs, _ := filesystem.NewClientWithSharedKeyCredential(dfsURL()+"/"+emu.Name("ghost"), cred(t), nil)
	_, err := fs.GetProperties(context.Background(), nil)
	wantCode(t, err, http.StatusNotFound, datalakeerror.FileSystemNotFound)
}

func TestFileSystem_DeleteThenCreateAgainWorks(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "fsdel")
	if _, err := fs.Delete(ctx, nil); err != nil {
		t.Fatal(err)
	}
	if _, err := fs.Create(ctx, nil); err != nil {
		t.Fatal(err)
	}
}

func TestFileSystem_ListFilesystemsThroughTheServiceClient(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "fslist")
	sc, err := service.NewClientWithSharedKeyCredential(dfsURL(), cred(t), nil)
	if err != nil {
		t.Fatal(err)
	}
	name := strings.TrimPrefix(fs.DFSURL(), dfsURL()+"/")
	found := false
	for p := sc.NewListFileSystemsPager(&service.ListFileSystemsOptions{Prefix: &name}); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		for _, it := range pg.FileSystemItems {
			if *it.Name == name {
				found = true
			}
		}
	}
	if !found {
		t.Fatal("filesystem not listed")
	}
}

func TestDirectory_CreateNestedAndListPathsRecursiveAndNot(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "dirs")
	if _, err := fs.NewDirectoryClient("a/b/c").Create(ctx, nil); err != nil {
		t.Fatal(err)
	}
	if _, err := fs.NewFileClient("a/b/c/f.txt").Create(ctx, nil); err != nil {
		t.Fatal(err)
	}
	names := func(recursive bool) []string {
		var out []string
		for p := fs.NewListPathsPager(recursive, nil); p.More(); {
			pg, err := p.NextPage(ctx)
			if err != nil {
				t.Fatal(err)
			}
			for _, it := range pg.Paths {
				out = append(out, *it.Name)
			}
		}
		return out
	}
	rec := names(true)
	if len(rec) != 4 || rec[0] != "a" || rec[1] != "a/b" || rec[2] != "a/b/c" || rec[3] != "a/b/c/f.txt" {
		t.Fatalf("recursive: %v", rec)
	}
	if flat := names(false); len(flat) != 1 || flat[0] != "a" {
		t.Fatalf("non recursive: %v", flat)
	}
}

func TestDirectory_ListPathsMarksDirectoriesAndGivesFileSizes(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "dirinfo")
	fs.NewDirectoryClient("d").Create(ctx, nil)
	f := fs.NewFileClient("d/f.bin")
	upload(t, f, randBytes(1234), nil)
	for p := fs.NewListPathsPager(true, nil); p.More(); {
		pg, _ := p.NextPage(ctx)
		for _, it := range pg.Paths {
			switch *it.Name {
			case "d":
				if it.IsDirectory == nil || !*it.IsDirectory {
					t.Fatalf("d should be a directory: %+v", it)
				}
			case "d/f.bin":
				if it.ContentLength == nil || *it.ContentLength != 1234 {
					t.Fatalf("file length %v", it.ContentLength)
				}
			}
		}
	}
}

func TestDirectory_DeleteWithoutRecursiveOnANonEmptyDirectoryIsDirectoryNotEmpty409(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "dirrm")
	d := fs.NewDirectoryClient("d")
	d.Create(ctx, nil)
	fs.NewFileClient("d/f").Create(ctx, nil)
	// file.Client.Delete sends recursive=false, directory.Client.Delete sends recursive=true
	_, err := fs.NewFileClient("d").Delete(ctx, nil)
	wantCode(t, err, http.StatusConflict, datalakeerror.StorageErrorCode("DirectoryNotEmpty"))
}

func TestDirectory_DeleteIsRecursiveAndRemovesTheWholeSubtree(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "dirrm2")
	d := fs.NewDirectoryClient("d")
	d.Create(ctx, nil)
	fs.NewFileClient("d/sub/f").Create(ctx, nil)
	if _, err := d.Delete(ctx, nil); err != nil {
		t.Fatalf("delete: %v", err)
	}
	_, err := fs.NewFileClient("d/sub/f").GetProperties(ctx, nil)
	wantCode(t, err, http.StatusNotFound, datalakeerror.PathNotFound)
}

func TestFile_AppendThenFlushMakesTheDataVisibleOnlyAfterTheFlush(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "appfl")
	f := fs.NewFileClient("log.txt")
	if _, err := f.Create(ctx, nil); err != nil {
		t.Fatal(err)
	}
	parts := []string{"hello ", "data ", "lake"}
	var off int64
	for _, p := range parts {
		if _, err := f.AppendData(ctx, off, streaming.NopCloser(strings.NewReader(p)), nil); err != nil {
			t.Fatalf("append at %d: %v", off, err)
		}
		off += int64(len(p))
	}
	props, _ := f.GetProperties(ctx, nil)
	if *props.ContentLength != 0 {
		t.Fatalf("length before flush = %d", *props.ContentLength)
	}
	if _, err := f.FlushData(ctx, off, nil); err != nil {
		t.Fatalf("flush: %v", err)
	}
	if got := readAll(t, f); string(got) != "hello data lake" {
		t.Fatalf("content %q", got)
	}
}

func TestFile_FlushOverAGapInTheAppendedDataIsInvalidFlushPosition400(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "appbad")
	f := fs.NewFileClient("x")
	f.Create(ctx, nil)
	f.AppendData(ctx, 0, streaming.NopCloser(strings.NewReader("abc")), nil)
	// appends may arrive in any order and at any offset; the flush checks that they are contiguous
	if _, err := f.AppendData(ctx, 99, streaming.NopCloser(strings.NewReader("d")), nil); err != nil {
		t.Fatalf("append at a later offset is allowed: %v", err)
	}
	_, err := f.FlushData(ctx, 100, nil)
	wantCode(t, err, http.StatusBadRequest, datalakeerror.StorageErrorCode("InvalidFlushPosition"))
}

func TestFile_UploadBufferWithParallelChunksAndDownloadBufferRoundTrip(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "updl")
	f := fs.NewFileClient("dir/big.bin")
	data := randBytes(6<<20 + 17)
	upload(t, f, data, &file.UploadBufferOptions{ChunkSize: 1 << 20, Concurrency: 4})
	buf := make([]byte, len(data))
	n, err := f.DownloadBuffer(ctx, buf, &file.DownloadBufferOptions{ChunkSize: 1 << 20, Concurrency: 4})
	if err != nil || n != int64(len(data)) {
		t.Fatalf("download: %d %v", n, err)
	}
	if !bytes.Equal(buf, data) {
		t.Fatal("content differs")
	}
}

func TestFile_UploadStreamOfUnknownLength(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "upst")
	f := fs.NewFileClient("s.bin")
	f.Create(ctx, nil)
	data := randBytes(3<<20 + 5)
	if err := f.UploadStream(ctx, struct{ io.Reader }{bytes.NewReader(data)}, &file.UploadStreamOptions{ChunkSize: 1 << 20, Concurrency: 3}); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(readAll(t, f), data) {
		t.Fatal("content differs")
	}
}

func TestFile_DownloadRange(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "rng")
	f := fs.NewFileClient("r")
	upload(t, f, []byte("0123456789"), nil)
	r, err := f.DownloadStream(ctx, &file.DownloadStreamOptions{Range: &file.HTTPRange{Offset: 3, Count: 4}})
	if err != nil {
		t.Fatal(err)
	}
	b, _ := io.ReadAll(r.Body)
	if string(b) != "3456" {
		t.Fatalf("range %q", b)
	}
}

func TestFile_RenameMovesContentAndTheSourceIsGone(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "mv")
	src := fs.NewFileClient("old/name.txt")
	upload(t, src, []byte("moved"), nil)
	// the destination's parent must exist (RenameDestinationParentPathNotFound otherwise)
	if _, err := src.Rename(ctx, "new/dir/renamed.txt", nil); err == nil {
		t.Fatal("rename into a missing parent directory must fail")
	} else {
		wantCode(t, err, http.StatusNotFound, datalakeerror.StorageErrorCode("RenameDestinationParentPathNotFound"))
	}
	fs.NewDirectoryClient("new/dir").Create(ctx, nil)
	if _, err := src.Rename(ctx, "new/dir/renamed.txt", nil); err != nil {
		t.Fatalf("rename: %v", err)
	}
	if got := readAll(t, fs.NewFileClient("new/dir/renamed.txt")); string(got) != "moved" {
		t.Fatalf("destination content %q", got)
	}
	_, err := src.GetProperties(ctx, nil)
	wantCode(t, err, http.StatusNotFound, datalakeerror.PathNotFound)
}

func TestDirectory_RenameMovesTheWholeSubtree(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "mvdir")
	for _, n := range []string{"src/a.txt", "src/sub/b.txt"} {
		upload(t, fs.NewFileClient(n), []byte("content "+n), nil)
	}
	if _, err := fs.NewDirectoryClient("src").Rename(ctx, "dst", nil); err != nil {
		t.Fatalf("rename dir: %v", err)
	}
	if got := readAll(t, fs.NewFileClient("dst/sub/b.txt")); string(got) != "content src/sub/b.txt" {
		t.Fatalf("content %q", got)
	}
	_, err := fs.NewDirectoryClient("src").GetProperties(ctx, nil)
	wantCode(t, err, http.StatusNotFound, datalakeerror.PathNotFound)
}

func TestFile_RenameOverAnExistingFileReplacesIt(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "mvover")
	upload(t, fs.NewFileClient("a"), []byte("A"), nil)
	upload(t, fs.NewFileClient("b"), []byte("B"), nil)
	if _, err := fs.NewFileClient("a").Rename(ctx, "b", nil); err != nil {
		t.Fatal(err)
	}
	if got := readAll(t, fs.NewFileClient("b")); string(got) != "A" {
		t.Fatalf("content %q", got)
	}
}

func TestACL_SetAndGetAccessControlRoundTripsAclAndPermissions(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "acl")
	f := fs.NewFileClient("secure.txt")
	f.Create(ctx, nil)
	acl := "user::rwx,group::r-x,other::---"
	if _, err := f.SetAccessControl(ctx, &file.SetAccessControlOptions{ACL: &acl, Owner: ptr("owner-oid"), Group: ptr("group-oid")}); err != nil {
		t.Fatal(err)
	}
	got, err := f.GetAccessControl(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	if *got.ACL != acl {
		t.Fatalf("acl %q", *got.ACL)
	}
	if *got.Owner != "owner-oid" || *got.Group != "group-oid" {
		t.Fatalf("owner %v group %v", *got.Owner, *got.Group)
	}
	if *got.Permissions != "rwxr-x---" {
		t.Fatalf("permissions %q", *got.Permissions)
	}
}

func TestACL_SetPermissionsInOctalUpdatesTheSymbolicForm(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "perm")
	f := fs.NewFileClient("p")
	f.Create(ctx, nil)
	if _, err := f.SetAccessControl(ctx, &file.SetAccessControlOptions{Permissions: ptr("0640")}); err != nil {
		t.Fatal(err)
	}
	got, _ := f.GetAccessControl(ctx, nil)
	if *got.Permissions != "rw-r-----" {
		t.Fatalf("permissions %q", *got.Permissions)
	}
}

func TestACL_RecursiveSetAppliesToEveryDescendantAndCountsThem(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "aclrec")
	d := fs.NewDirectoryClient("top")
	d.Create(ctx, nil)
	for _, n := range []string{"top/a", "top/s/b", "top/s/t/c"} {
		fs.NewFileClient(n).Create(ctx, nil)
	}
	res, err := d.SetAccessControlRecursive(ctx, "user::rwx,group::r-x,other::r--", nil)
	if err != nil {
		t.Fatalf("recursive acl: %v", err)
	}
	if res.DirectoriesSuccessful == nil || res.FilesSuccessful == nil || *res.DirectoriesSuccessful != 3 || *res.FilesSuccessful != 3 || *res.FailureCount != 0 {
		t.Fatalf("counts dirs=%v files=%v failed=%v", res.DirectoriesSuccessful, res.FilesSuccessful, res.FailureCount)
	}
	got, _ := fs.NewFileClient("top/s/t/c").GetAccessControl(ctx, nil)
	if *got.ACL != "user::rwx,group::r-x,other::r--" {
		t.Fatalf("leaf acl %q", *got.ACL)
	}
}

func TestACL_UpdateAddsANamedEntryWithoutDroppingTheOthers(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "aclupd")
	f := fs.NewFileClient("f")
	f.Create(ctx, nil)
	f.SetAccessControl(ctx, &file.SetAccessControlOptions{ACL: ptr("user::rwx,group::r-x,other::---")})
	oid := "11111111-2222-3333-4444-555555555555"
	if _, err := f.UpdateAccessControl(ctx, "user:"+oid+":r--", nil); err != nil {
		t.Fatalf("update: %v", err)
	}
	got, _ := f.GetAccessControl(ctx, nil)
	if !strings.Contains(*got.ACL, "user:"+oid+":r--") || !strings.Contains(*got.ACL, "user::rwx") {
		t.Fatalf("acl %q", *got.ACL)
	}
}

func TestFile_MetadataAndHTTPHeadersRoundTrip(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "prop")
	f := fs.NewFileClient("p.json")
	f.Create(ctx, nil)
	if _, err := f.SetMetadata(ctx, map[string]*string{"kind": ptr("json")}, nil); err != nil {
		t.Fatal(err)
	}
	if _, err := f.SetHTTPHeaders(ctx, file.HTTPHeaders{ContentType: ptr("application/json"), CacheControl: ptr("no-cache")}, nil); err != nil {
		t.Fatal(err)
	}
	p, err := f.GetProperties(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	var kind string
	for k, v := range p.Metadata {
		if strings.EqualFold(k, "kind") {
			kind = *v
		}
	}
	if kind != "json" || *p.ContentType != "application/json" || *p.CacheControl != "no-cache" {
		t.Fatalf("props: kind=%q type=%v cache=%v", kind, *p.ContentType, *p.CacheControl)
	}
}

func TestFile_DataLakeAndBlobEndpointsShareTheSameData(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "shared")
	f := fs.NewFileClient("dir/shared.txt")
	upload(t, f, []byte("seen by both"), nil)
	bc, err := blockblob.NewClientWithSharedKeyCredential(f.BlobURL(), func() *azblob.SharedKeyCredential {
		c, _ := azblob.NewSharedKeyCredential(emu.DevAccount, emu.DevKey)
		return c
	}(), nil)
	if err != nil {
		t.Fatal(err)
	}
	r, err := bc.DownloadStream(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	b, _ := io.ReadAll(r.Body)
	if string(b) != "seen by both" {
		t.Fatalf("blob endpoint returned %q", b)
	}
	// and a blob written through the blob endpoint appears in the file system listing
	bc2, _ := blockblob.NewClientWithSharedKeyCredential(fs.BlobURL()+"/from-blob/x.txt", func() *azblob.SharedKeyCredential {
		c, _ := azblob.NewSharedKeyCredential(emu.DevAccount, emu.DevKey)
		return c
	}(), nil)
	if _, err := bc2.UploadBuffer(ctx, []byte("blob side"), nil); err != nil {
		t.Fatal(err)
	}
	var names []string
	for p := fs.NewListPathsPager(true, nil); p.More(); {
		pg, _ := p.NextPage(ctx)
		for _, it := range pg.Paths {
			names = append(names, *it.Name)
		}
	}
	joined := strings.Join(names, ",")
	if !strings.Contains(joined, "from-blob/x.txt") || !strings.Contains(joined, "from-blob,") && !strings.HasSuffix(joined, "from-blob") && !strings.Contains(joined, "from-blob") {
		t.Fatalf("paths %v", names)
	}
}

func TestFile_NamesWithSpacesUnicodeAndPercentRoundTrip(t *testing.T) {
	fs := newFS(t, "dfsnames")
	for _, n := range []string{"sp ace/file one.txt", "ünï/日本語.txt", "pct/100%.txt", "plus/a+b.txt", "hash/a#b.txt"} {
		n := n
		t.Run(n, func(t *testing.T) {
			f := fs.NewFileClient(n)
			upload(t, f, []byte("x "+n), nil)
			if got := readAll(t, f); string(got) != "x "+n {
				t.Fatalf("content %q", got)
			}
		})
	}
}

func TestFile_DeleteThenPropertiesIsPathNotFound(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "frm")
	f := fs.NewFileClient("x")
	f.Create(ctx, nil)
	if _, err := f.Delete(ctx, nil); err != nil {
		t.Fatal(err)
	}
	_, err := f.GetProperties(ctx, nil)
	wantCode(t, err, http.StatusNotFound, datalakeerror.PathNotFound)
}

func TestSAS_FileSystemSasLetsAnotherClientListAndRead(t *testing.T) {
	ctx := context.Background()
	fs := newFS(t, "dsas")
	upload(t, fs.NewFileClient("a.txt"), []byte("sas read"), nil)
	u, err := fs.GetSASURL(sas.FileSystemPermissions{Read: true, List: true}, time.Now().Add(time.Hour), nil)
	if err != nil {
		t.Fatal(err)
	}
	fsSAS, err := filesystem.NewClientWithNoCredential(u, nil)
	if err != nil {
		t.Fatal(err)
	}
	var n int
	for p := fsSAS.NewListPathsPager(true, nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatalf("list with SAS: %v", err)
		}
		n += len(pg.Paths)
	}
	if n != 1 {
		t.Fatalf("listed %d", n)
	}
	r, err := fsSAS.NewFileClient("a.txt").DownloadStream(ctx, nil)
	if err != nil {
		t.Fatalf("read with SAS: %v", err)
	}
	b, _ := io.ReadAll(r.Body)
	if string(b) != "sas read" {
		t.Fatalf("content %q", b)
	}
}

var _ = fmt.Sprint
