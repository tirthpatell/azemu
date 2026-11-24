package file

import (
	"bytes"
	"context"
	"errors"
	"io"
	"net/http"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/to"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azfile/directory"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azfile/file"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azfile/fileerror"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azfile/lease"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azfile/sas"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azfile/service"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azfile/share"
)

func TestMain(m *testing.M) { emu.Main(m) }

func cred(t testing.TB) *service.SharedKeyCredential {
	t.Helper()
	c, err := service.NewSharedKeyCredential(emu.DevAccount, emu.DevKey)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func svc(t testing.TB) *service.Client {
	t.Helper()
	s, err := service.NewClientWithSharedKeyCredential(emu.E.HTTP("file", emu.DevAccount), cred(t), nil)
	if err != nil {
		t.Fatal(err)
	}
	return s
}

func newShare(t testing.TB, prefix string) *share.Client {
	t.Helper()
	s := svc(t)
	name := emu.Name(prefix)
	if _, err := s.CreateShare(context.Background(), name, nil); err != nil {
		t.Fatalf("create share: %v", err)
	}
	return s.NewShareClient(name)
}

func wantCode(t testing.TB, err error, status int, code fileerror.Code) {
	t.Helper()
	if err == nil {
		t.Fatalf("expected %d %s, got success", status, code)
	}
	var re *azcore.ResponseError
	if !errors.As(err, &re) {
		t.Fatalf("not a ResponseError: %v", err)
	}
	if re.StatusCode != status || !fileerror.HasCode(err, code) {
		t.Fatalf("got %d %q, want %d %q", re.StatusCode, re.ErrorCode, status, code)
	}
}

func TestShare_CreateReadPropertiesAndDelete(t *testing.T) {
	ctx := context.Background()
	s := svc(t)
	name := emu.Name("sh")
	if _, err := s.CreateShare(ctx, name, &service.CreateShareOptions{Quota: to.Ptr(int32(9)), Metadata: map[string]*string{"owner": to.Ptr("me")}}); err != nil {
		t.Fatal(err)
	}
	sh := s.NewShareClient(name)
	p, err := sh.GetProperties(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	if p.Quota == nil || *p.Quota != 9 || p.ETag == nil {
		t.Fatalf("properties: %+v", p)
	}
	// the Go SDK hands metadata names back as the HTTP library writes them (first letter upper case)
	if m := p.Metadata["Owner"]; m == nil || *m != "me" {
		t.Fatalf("metadata: %v", p.Metadata)
	}
	if _, err := s.CreateShare(ctx, name, nil); err == nil {
		t.Fatal("second create should fail")
	} else {
		wantCode(t, err, http.StatusConflict, fileerror.ShareAlreadyExists)
	}
	if _, err := sh.Delete(ctx, nil); err != nil {
		t.Fatal(err)
	}
	_, err = sh.GetProperties(ctx, nil)
	wantCode(t, err, http.StatusNotFound, fileerror.ShareNotFound)
}

func TestShare_ListWithPrefixAndMetadata(t *testing.T) {
	ctx := context.Background()
	s := svc(t)
	prefix := emu.Name("lst")
	for _, n := range []string{prefix + "a", prefix + "b"} {
		if _, err := s.CreateShare(ctx, n, &service.CreateShareOptions{Metadata: map[string]*string{"n": to.Ptr(n)}}); err != nil {
			t.Fatal(err)
		}
	}
	pager := s.NewListSharesPager(&service.ListSharesOptions{Prefix: &prefix, Include: service.ListSharesInclude{Metadata: true}, MaxResults: to.Ptr(int32(1))})
	var names []string
	pages := 0
	for pager.More() {
		page, err := pager.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		pages++
		for _, sh := range page.Shares {
			names = append(names, *sh.Name)
			if m := sh.Metadata["n"]; m == nil || *m != *sh.Name {
				t.Fatalf("metadata of %s: %v", *sh.Name, sh.Metadata)
			}
		}
	}
	if pages != 2 || len(names) != 2 || names[0] != prefix+"a" {
		t.Fatalf("pages=%d names=%v", pages, names)
	}
}

func TestDirectoriesAndFiles_UploadDownloadAndList(t *testing.T) {
	ctx := context.Background()
	sh := newShare(t, "df")
	dir := sh.NewDirectoryClient("docs")
	if _, err := dir.Create(ctx, &directory.CreateOptions{Metadata: map[string]*string{"k": to.Ptr("v")}}); err != nil {
		t.Fatal(err)
	}
	if _, err := dir.Create(ctx, nil); err == nil {
		t.Fatal("create twice")
	} else {
		wantCode(t, err, http.StatusConflict, fileerror.ResourceAlreadyExists)
	}
	f := dir.NewFileClient("hello.txt")
	// more than one range of 4 MiB: the SDK sends it in pieces
	data := bytes.Repeat([]byte("0123456789abcdef"), 700_000)
	if _, err := f.Create(ctx, int64(len(data)), &file.CreateOptions{HTTPHeaders: &file.HTTPHeaders{ContentType: to.Ptr("text/plain")}}); err != nil {
		t.Fatal(err)
	}
	if err := f.UploadBuffer(ctx, data, &file.UploadBufferOptions{ChunkSize: 4 << 20, Concurrency: 2}); err != nil {
		t.Fatal(err)
	}
	got := make([]byte, len(data))
	if _, err := f.DownloadBuffer(ctx, got, &file.DownloadBufferOptions{ChunkSize: 1 << 20, Concurrency: 2}); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(got, data) {
		t.Fatal("downloaded content differs")
	}
	p, err := f.GetProperties(ctx, nil)
	if err != nil || *p.ContentLength != int64(len(data)) || *p.ContentType != "text/plain" {
		t.Fatalf("properties: %v %v", err, p)
	}
	pager := dir.NewListFilesAndDirectoriesPager(nil)
	var names []string
	for pager.More() {
		page, err := pager.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		for _, e := range page.Segment.Files {
			names = append(names, *e.Name)
		}
	}
	if len(names) != 1 || names[0] != "hello.txt" {
		t.Fatalf("listing: %v", names)
	}
}

func TestFile_RangesAndClear(t *testing.T) {
	ctx := context.Background()
	sh := newShare(t, "rg")
	f := sh.NewRootDirectoryClient().NewFileClient("r.bin")
	if _, err := f.Create(ctx, 4096, nil); err != nil {
		t.Fatal(err)
	}
	body := streaming("hello")
	if _, err := f.UploadRange(ctx, 1024, body, nil); err != nil {
		t.Fatal(err)
	}
	r, err := f.GetRangeList(ctx, nil)
	if err != nil || len(r.Ranges) != 1 || *r.Ranges[0].Start != 1024 || *r.Ranges[0].End != 1535 {
		t.Fatalf("ranges: %v %+v", err, r.Ranges)
	}
	if _, err := f.ClearRange(ctx, file.HTTPRange{Offset: 1024, Count: 512}, nil); err != nil {
		t.Fatal(err)
	}
	r, err = f.GetRangeList(ctx, nil)
	if err != nil || len(r.Ranges) != 0 {
		t.Fatalf("after clear: %v %+v", err, r.Ranges)
	}
	_, err = f.UploadRange(ctx, 4090, streaming("too long for the file"), nil)
	wantCode(t, err, http.StatusRequestedRangeNotSatisfiable, fileerror.InvalidRange)
}

func TestFile_LeaseAndRename(t *testing.T) {
	ctx := context.Background()
	sh := newShare(t, "ls")
	root := sh.NewRootDirectoryClient()
	f := root.NewFileClient("a.txt")
	if _, err := f.Create(ctx, 3, nil); err != nil {
		t.Fatal(err)
	}
	lc, err := lease.NewFileClient(f, &lease.FileClientOptions{LeaseID: to.Ptr("11111111-1111-1111-1111-111111111111")})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := lc.Acquire(ctx, nil); err != nil {
		t.Fatal(err)
	}
	_, err = f.SetMetadata(ctx, &file.SetMetadataOptions{Metadata: map[string]*string{"a": to.Ptr("b")}})
	wantCode(t, err, http.StatusPreconditionFailed, fileerror.Code("LeaseIdMissing"))
	if _, err := lc.Release(ctx, nil); err != nil {
		t.Fatal(err)
	}
	moved, err := f.Rename(ctx, "b.txt", nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := root.NewFileClient("b.txt").GetProperties(ctx, nil); err != nil {
		t.Fatal(err)
	}
	_ = moved
	_, err = f.GetProperties(ctx, nil)
	wantCode(t, err, http.StatusNotFound, fileerror.ResourceNotFound)
}

func TestShare_SnapshotAndSAS(t *testing.T) {
	ctx := context.Background()
	sh := newShare(t, "sn")
	f := sh.NewRootDirectoryClient().NewFileClient("s.txt")
	if _, err := f.Create(ctx, 5, nil); err != nil {
		t.Fatal(err)
	}
	if _, err := f.UploadRange(ctx, 0, streaming("hello"), nil); err != nil {
		t.Fatal(err)
	}
	snap, err := sh.CreateSnapshot(ctx, nil)
	if err != nil || snap.Snapshot == nil {
		t.Fatalf("snapshot: %v", err)
	}
	if _, err := f.UploadRange(ctx, 0, streaming("HELLO"), nil); err != nil {
		t.Fatal(err)
	}
	old, err := sh.WithSnapshot(*snap.Snapshot)
	if err != nil {
		t.Fatal(err)
	}
	dl, err := old.NewRootDirectoryClient().NewFileClient("s.txt").DownloadStream(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	b, _ := io.ReadAll(dl.Body)
	if string(b) != "hello" {
		t.Fatalf("snapshot content %q", b)
	}
	// a read-only SAS for the file, signed with the account key
	url, err := f.GetSASURL(sas.FilePermissions{Read: true}, time.Now().Add(time.Hour), nil)
	if err != nil {
		t.Fatal(err)
	}
	viaSAS, err := file.NewClientWithNoCredential(url, nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := viaSAS.GetProperties(ctx, nil); err != nil {
		t.Fatalf("get with SAS: %v", err)
	}
	if _, err := viaSAS.Delete(ctx, nil); err == nil {
		t.Fatal("a read-only SAS deleted a file")
	} else {
		wantCode(t, err, http.StatusForbidden, fileerror.AuthorizationPermissionMismatch)
	}
}

type reader struct{ *bytes.Reader }

func (reader) Close() error { return nil }

func streaming(s string) io.ReadSeekCloser { return reader{bytes.NewReader([]byte(s))} }
