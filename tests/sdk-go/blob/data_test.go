package blob

import (
	"bytes"
	"context"
	"crypto/md5"
	"encoding/base64"
	"fmt"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/streaming"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"

	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/blob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/bloberror"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/blockblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/container"
)

func TestBlockBlob_UploadBufferThenDownloadStreamReturnsSameBytesAndETag(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "ub")
	b := c.NewBlockBlobClient("a.txt")
	data := randBytes(1000)
	up, err := b.UploadBuffer(ctx, data, nil)
	if err != nil {
		t.Fatal(err)
	}
	if up.ETag == nil || up.LastModified == nil {
		t.Fatalf("upload response lacks etag/last-modified")
	}
	r, err := b.DownloadStream(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	if *r.ContentLength != 1000 || *r.ETag != *up.ETag || *r.BlobType != blob.BlobTypeBlockBlob {
		t.Fatalf("download headers: len=%v etag=%v type=%v", *r.ContentLength, *r.ETag, *r.BlobType)
	}
	mustEqual(t, readAll(t, r.Body), data)
}

func TestBlockBlob_UploadBufferOfNineMiBThenDownloadBufferWithParallelRangesIsByteIdentical(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "pb")
	b := c.NewBlockBlobClient("big.bin")
	data := randBytes(9*1024*1024 + 123)
	if _, err := b.UploadBuffer(ctx, data, &blockblob.UploadBufferOptions{BlockSize: 1 << 20, Concurrency: 4}); err != nil {
		t.Fatal(err)
	}
	buf := make([]byte, len(data))
	n, err := b.DownloadBuffer(ctx, buf, &blob.DownloadBufferOptions{BlockSize: 1 << 20, Concurrency: 5})
	if err != nil || n != int64(len(data)) {
		t.Fatalf("download buffer: %d %v", n, err)
	}
	mustEqual(t, buf, data)
}

func TestBlockBlob_BlocksStagedFromParallelGoroutinesCommitInTheOrderOfTheList(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "par")
	b := c.NewBlockBlobClient("p")
	const n = 12
	var want []byte
	ids := make([]string, n)
	parts := make([][]byte, n)
	for i := range ids {
		ids[i] = base64.StdEncoding.EncodeToString([]byte(fmt.Sprintf("block-%04d", i)))
		parts[i] = randBytes(100_000 + i)
		want = append(want, parts[i]...)
	}
	var wg sync.WaitGroup
	errs := make(chan error, n)
	for i := range ids {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			_, err := b.StageBlock(ctx, ids[i], streaming.NopCloser(bytes.NewReader(parts[i])), nil)
			errs <- err
		}(i)
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		if err != nil {
			t.Fatal(err)
		}
	}
	if _, err := b.CommitBlockList(ctx, ids, nil); err != nil {
		t.Fatal(err)
	}
	bl, err := b.GetBlockList(ctx, blockblob.BlockListTypeCommitted, nil)
	if err != nil || len(bl.CommittedBlocks) != n {
		t.Fatalf("committed blocks %d %v", len(bl.CommittedBlocks), err)
	}
	for i, blk := range bl.CommittedBlocks {
		if *blk.Name != ids[i] || *blk.Size != int64(len(parts[i])) {
			t.Fatalf("block %d: %s size %d", i, *blk.Name, *blk.Size)
		}
	}
	mustEqual(t, download(t, b.BlobClient()), want)
}

// plainReader hides every method but Read, so the SDK cannot see the length.
type plainReader struct{ r io.Reader }

func (p plainReader) Read(b []byte) (int, error) { return p.r.Read(b) }

func TestBlockBlob_UploadStreamOfUnknownLengthUsesParallelBlocks(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "us")
	b := c.NewBlockBlobClient("stream.bin")
	data := randBytes(5*1024*1024 + 77)
	_, err := b.UploadStream(ctx, plainReader{bytes.NewReader(data)}, &blockblob.UploadStreamOptions{BlockSize: 1 << 20, Concurrency: 3})
	if err != nil {
		t.Fatal(err)
	}
	// UploadStream stages 1 MiB blocks in parallel and commits them; none may stay uncommitted
	bl, _ := b.GetBlockList(ctx, blockblob.BlockListTypeAll, nil)
	if len(bl.CommittedBlocks) != 6 || len(bl.UncommittedBlocks) != 0 {
		t.Fatalf("blocks: committed %d uncommitted %d", len(bl.CommittedBlocks), len(bl.UncommittedBlocks))
	}
	mustEqual(t, download(t, b.BlobClient()), data)
}

func TestBlockBlob_UploadStreamSmallerThanOneBlockIsASingleUpload(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "us1")
	b := c.NewBlockBlobClient("tiny")
	if _, err := b.UploadStream(ctx, plainReader{strings.NewReader("tiny")}, nil); err != nil {
		t.Fatal(err)
	}
	mustEqual(t, download(t, b.BlobClient()), []byte("tiny"))
}

func TestBlockBlob_EmptyBlobHasZeroLength(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "empty")
	b := c.NewBlockBlobClient("e")
	if _, err := b.UploadBuffer(ctx, nil, nil); err != nil {
		t.Fatal(err)
	}
	p, err := b.GetProperties(ctx, nil)
	if err != nil || *p.ContentLength != 0 {
		t.Fatalf("len %v %v", p.ContentLength, err)
	}
	mustEqual(t, download(t, b.BlobClient()), nil)
}

func TestBlockBlob_StageBlocksCommitListGetBlockListShowsUncommittedUntilCommit(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "blk")
	b := c.NewBlockBlobClient("staged")
	ids := []string{"AAAA", "BBBB", "CCCC"}
	enc := func(s string) string { return s } // ids must be base64 already
	parts := []string{"one-", "two-", "three"}
	for i, id := range ids {
		if _, err := b.StageBlock(ctx, enc(id), streaming.NopCloser(strings.NewReader(parts[i])), nil); err != nil {
			t.Fatal(err)
		}
	}
	bl, err := b.GetBlockList(ctx, blockblob.BlockListTypeUncommitted, nil)
	if err != nil || len(bl.UncommittedBlocks) != 3 || len(bl.CommittedBlocks) != 0 {
		t.Fatalf("uncommitted list: %v %+v", err, bl)
	}
	// commit in a different order, leaving one block out
	if _, err := b.CommitBlockList(ctx, []string{"CCCC", "AAAA"}, nil); err != nil {
		t.Fatal(err)
	}
	mustEqual(t, download(t, b.BlobClient()), []byte("three"+"one-"))
}

func TestBlob_DownloadRangeReturns206WithContentRange(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "rng")
	b := c.NewBlockBlobClient("r")
	data := []byte("0123456789abcdefghij")
	b.UploadBuffer(ctx, data, nil)
	r, err := b.DownloadStream(ctx, &blob.DownloadStreamOptions{Range: blob.HTTPRange{Offset: 5, Count: 6}})
	if err != nil {
		t.Fatal(err)
	}
	if *r.ContentRange != "bytes 5-10/20" || *r.ContentLength != 6 {
		t.Fatalf("content-range %q length %d", *r.ContentRange, *r.ContentLength)
	}
	mustEqual(t, readAll(t, r.Body), data[5:11])
	// open-ended
	r, err = b.DownloadStream(ctx, &blob.DownloadStreamOptions{Range: blob.HTTPRange{Offset: 15}})
	if err != nil {
		t.Fatal(err)
	}
	mustEqual(t, readAll(t, r.Body), data[15:])
}

func TestBlob_RangeStartingPastTheEndIsInvalidRange416(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "rng416")
	b := c.NewBlockBlobClient("r")
	b.UploadBuffer(ctx, []byte("abc"), nil)
	_, err := b.DownloadStream(ctx, &blob.DownloadStreamOptions{Range: blob.HTTPRange{Offset: 100, Count: 5}})
	wantCode(t, err, http.StatusRequestedRangeNotSatisfiable, bloberror.InvalidRange)
}

func TestBlob_RangeGetContentMD5ReturnsMD5OfTheRange(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "md5")
	b := c.NewBlockBlobClient("m")
	data := randBytes(4096)
	b.UploadBuffer(ctx, data, nil)
	r, err := b.DownloadStream(ctx, &blob.DownloadStreamOptions{Range: blob.HTTPRange{Offset: 100, Count: 1000}, RangeGetContentMD5: ptr(true)})
	if err != nil {
		t.Fatal(err)
	}
	want := md5.Sum(data[100:1100])
	if !bytes.Equal(r.ContentMD5, want[:]) {
		t.Fatalf("content-md5 %x want %x", r.ContentMD5, want)
	}
	readAll(t, r.Body)
}

func TestBlob_UploadStoresContentMD5AndPropertiesReturnIt(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "md5b")
	b := c.NewBlockBlobClient("m")
	data := []byte("hash me")
	sum := md5.Sum(data)
	if _, err := b.UploadBuffer(ctx, data, &blockblob.UploadBufferOptions{HTTPHeaders: &blob.HTTPHeaders{BlobContentMD5: sum[:]}}); err != nil {
		t.Fatal(err)
	}
	p, _ := b.GetProperties(ctx, nil)
	if !bytes.Equal(p.ContentMD5, sum[:]) {
		t.Fatalf("md5 %x", p.ContentMD5)
	}
}

func TestBlob_WrongBlobContentMD5OnUploadIsRefusedMd5Mismatch(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "md5bad")
	b := c.NewBlockBlobClient("m")
	bad := md5.Sum([]byte("other"))
	_, err := b.Upload(ctx, streaming.NopCloser(strings.NewReader("hash me")), &blockblob.UploadOptions{HTTPHeaders: &blob.HTTPHeaders{BlobContentMD5: bad[:]}})
	wantCode(t, err, http.StatusBadRequest, bloberror.MD5Mismatch)
}

func TestBlob_UploadWithStructuredMessageCRC64StoresOnlyThePayload(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "xsm")
	b := c.NewBlockBlobClient("s")
	data := randBytes(100_000)
	_, err := b.UploadBuffer(ctx, data, &blockblob.UploadBufferOptions{TransactionalValidation: blob.TransferValidationTypeComputeStructuredMessageCRC64(32 * 1024)})
	if err != nil {
		t.Fatalf("upload with x-ms-structured-body: %v", err)
	}
	mustEqual(t, download(t, b.BlobClient()), data)
}

func TestBlob_DownloadWithStructuredMessageCRC64ReturnsTheDataToTheCaller(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "xsmd")
	b := c.NewBlockBlobClient("s")
	data := randBytes(100_000)
	b.UploadBuffer(ctx, data, nil)
	r, err := b.DownloadStream(ctx, &blob.DownloadStreamOptions{TransactionalValidation: blob.TransferValidationTypeComputeStructuredMessageCRC64(32 * 1024)})
	if err != nil {
		t.Fatalf("download with structured message: %v", err)
	}
	mustEqual(t, readAll(t, r.Body), data)
}

func TestBlob_DownloadBufferWithCRC64Validation(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "crc")
	b := c.NewBlockBlobClient("c")
	data := randBytes(3 << 20)
	b.UploadBuffer(ctx, data, &blockblob.UploadBufferOptions{BlockSize: 1 << 20})
	buf := make([]byte, len(data))
	_, err := b.DownloadBuffer(ctx, buf, &blob.DownloadBufferOptions{BlockSize: 1 << 20, TransactionalValidation: blob.TransferValidationTypeComputeCRC64()})
	if err != nil {
		t.Fatalf("download with CRC64 validation: %v", err)
	}
	mustEqual(t, buf, data)
}

func TestBlob_UploadFileAndDownloadFileRoundTrip(t *testing.T) {
	ctx := context.Background()
	cl, err := azblob.NewClientWithSharedKeyCredential(serviceURL(), devCred(t), nil)
	if err != nil {
		t.Fatal(err)
	}
	cont := newContainer(t, "file")
	data := randBytes(3<<20 + 5)
	dir := t.TempDir()
	src := filepath.Join(dir, "src")
	os.WriteFile(src, data, 0o600)
	f, _ := os.Open(src)
	defer f.Close()
	name := cont.URL()[strings.LastIndex(cont.URL(), "/")+1:]
	if _, err := cl.UploadFile(ctx, name, "f.bin", f, &azblob.UploadFileOptions{BlockSize: 1 << 20, Concurrency: 3}); err != nil {
		t.Fatal(err)
	}
	dst, _ := os.Create(filepath.Join(dir, "dst"))
	defer dst.Close()
	n, err := cl.DownloadFile(ctx, name, "f.bin", dst, &azblob.DownloadFileOptions{BlockSize: 1 << 20, Concurrency: 3})
	if err != nil || n != int64(len(data)) {
		t.Fatalf("download file: %d %v", n, err)
	}
	got, _ := os.ReadFile(filepath.Join(dir, "dst"))
	mustEqual(t, got, data)
}

func TestBlob_MissingBlobIsBlobNotFound404ForGetPropertiesAndDownload(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "nf")
	b := c.NewBlobClient("nope")
	_, err := b.GetProperties(ctx, nil)
	wantCode(t, err, http.StatusNotFound, bloberror.BlobNotFound)
	_, err = b.DownloadStream(ctx, nil)
	wantCode(t, err, http.StatusNotFound, bloberror.BlobNotFound)
}

func TestBlob_SetHTTPHeadersChangesContentTypeAndCacheControl(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "hdr")
	b := c.NewBlockBlobClient("h")
	b.UploadBuffer(ctx, []byte("x"), nil)
	if _, err := b.SetHTTPHeaders(ctx, blob.HTTPHeaders{BlobContentType: ptr("text/csv"), BlobCacheControl: ptr("max-age=60"), BlobContentLanguage: ptr("fr"), BlobContentDisposition: ptr("attachment; filename=\"a b.csv\"")}, nil); err != nil {
		t.Fatal(err)
	}
	p, _ := b.GetProperties(ctx, nil)
	if *p.ContentType != "text/csv" || *p.CacheControl != "max-age=60" || *p.ContentLanguage != "fr" || *p.ContentDisposition != "attachment; filename=\"a b.csv\"" {
		t.Fatalf("headers %v %v %v %v", *p.ContentType, *p.CacheControl, *p.ContentLanguage, *p.ContentDisposition)
	}
}

func TestBlob_MetadataOnUploadAndSetMetadataReplaceEverything(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "md")
	b := c.NewBlockBlobClient("m")
	b.UploadBuffer(ctx, []byte("x"), &blockblob.UploadBufferOptions{Metadata: map[string]*string{"first": ptr("1"), "second": ptr("2")}})
	p, _ := b.GetProperties(ctx, nil)
	if len(p.Metadata) != 2 {
		t.Fatalf("metadata %v", p.Metadata)
	}
	b.SetMetadata(ctx, map[string]*string{"third": ptr("three three")}, nil)
	p, _ = b.GetProperties(ctx, nil)
	if len(p.Metadata) != 1 || metaGet(p.Metadata, "third") != "three three" {
		t.Fatalf("metadata after set %v", p.Metadata)
	}
}

func TestBlob_NonAsciiMetadataValueIsRefusedInvalidMetadata(t *testing.T) {
	c := newContainer(t, "mdna")
	b := c.NewBlockBlobClient("m")
	b.UploadBuffer(context.Background(), []byte("x"), nil)
	_, err := b.SetMetadata(context.Background(), map[string]*string{"k": ptr("café")}, nil)
	if err == nil {
		t.Fatal("Azure refuses non-ASCII metadata values")
	}
	wantCode(t, err, http.StatusBadRequest, bloberror.InvalidMetadata)
}

func TestBlob_TagsSetGetAndFindBlobsByTagAtContainerAndServiceLevel(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "tag")
	b := c.NewBlockBlobClient("t1")
	tagVal := emu_unique("v")
	b.UploadBuffer(ctx, []byte("x"), &blockblob.UploadBufferOptions{Tags: map[string]string{"env": "prod", "run": tagVal}})
	c.NewBlockBlobClient("t2").UploadBuffer(ctx, []byte("y"), &blockblob.UploadBufferOptions{Tags: map[string]string{"env": "dev"}})
	got, err := b.GetTags(ctx, nil)
	if err != nil || got.BlobTagSet == nil || len(got.BlobTagSet) != 2 {
		t.Fatalf("tags %v %v", got.BlobTagSet, err)
	}
	if _, err := b.SetTags(ctx, map[string]string{"env": "prod", "run": tagVal, "extra": "1"}, nil); err != nil {
		t.Fatal(err)
	}
	f, err := c.FilterBlobs(ctx, fmt.Sprintf(`"run"='%s'`, tagVal), nil)
	if err != nil || len(f.Blobs) != 1 || *f.Blobs[0].Name != "t1" {
		t.Fatalf("container filter: %v %+v", err, f.Blobs)
	}
	sf, err := newSvc(t).FilterBlobs(ctx, fmt.Sprintf(`"run"='%s' AND "env"='prod'`, tagVal), nil)
	if err != nil || len(sf.Blobs) != 1 {
		t.Fatalf("service filter: %v", err)
	}
}

func TestBlob_SetTierCoolIsReportedByGetProperties(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "tier")
	b := c.NewBlockBlobClient("t")
	b.UploadBuffer(ctx, []byte("x"), nil)
	if _, err := b.SetTier(ctx, blob.AccessTierCool, nil); err != nil {
		t.Fatal(err)
	}
	p, _ := b.GetProperties(ctx, nil)
	if p.AccessTier == nil || *p.AccessTier != string(blob.AccessTierCool) {
		t.Fatalf("tier %v", p.AccessTier)
	}
}

func TestBlob_DeleteThenGetPropertiesIsBlobNotFound(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "rm")
	b := c.NewBlockBlobClient("x")
	b.UploadBuffer(ctx, []byte("x"), nil)
	if _, err := b.Delete(ctx, nil); err != nil {
		t.Fatal(err)
	}
	_, err := b.GetProperties(ctx, nil)
	wantCode(t, err, http.StatusNotFound, bloberror.BlobNotFound)
	_, err = b.Delete(ctx, nil)
	wantCode(t, err, http.StatusNotFound, bloberror.BlobNotFound)
}

func TestBlob_SoftDeleteThenUndeleteRestoresTheBlob(t *testing.T) {
	ctx := context.Background()
	s := newSvc(t)
	if _, err := s.SetProperties(ctx, &azblob_svcSetOpts{DeleteRetentionPolicy: &azblob_retention{Enabled: ptr(true), Days: ptr(int32(3))}}); err != nil {
		t.Fatal(err)
	}
	c := newContainer(t, "undel")
	b := c.NewBlockBlobClient("u")
	b.UploadBuffer(ctx, []byte("restore me"), nil)
	b.Delete(ctx, nil)
	// shows up in a listing with Include Deleted
	var deleted bool
	for p := c.NewListBlobsFlatPager(&container.ListBlobsFlatOptions{Include: container.ListBlobsInclude{Deleted: true}}); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		for _, it := range pg.Segment.BlobItems {
			if *it.Name == "u" && it.Deleted != nil && *it.Deleted {
				deleted = true
			}
		}
	}
	if !deleted {
		t.Fatal("soft-deleted blob not listed with Include.Deleted")
	}
	if _, err := b.Undelete(ctx, nil); err != nil {
		t.Fatal(err)
	}
	mustEqual(t, download(t, b.BlobClient()), []byte("restore me"))
}

func TestBlob_NamesThatNeedEncodingRoundTripThroughUploadDownloadAndList(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "names")
	names := []string{
		"with space.txt", "unicode-日本語-é.txt", "percent%20sign", "100%", "plus+sign", "hash#tag", "question?mark",
		"amp&equals=semi;colon", "dir/sub dir/file.txt", "dot.", "tilde~name", "quote'dq\"", "brackets[1]{2}", "emoji-😀",
	}
	for _, n := range names {
		n := n
		t.Run(n, func(t *testing.T) {
			b := c.NewBlockBlobClient(n)
			if _, err := b.UploadBuffer(ctx, []byte("content of "+n), nil); err != nil {
				t.Fatalf("upload: %v", err)
			}
			got := download(t, b.BlobClient())
			if string(got) != "content of "+n {
				t.Fatalf("download of %q returned %q", n, got)
			}
			p, err := b.GetProperties(ctx, nil)
			if err != nil || *p.ContentLength != int64(len("content of "+n)) {
				t.Fatalf("properties: %v", err)
			}
		})
	}
	listed := map[string]bool{}
	for p := c.NewListBlobsFlatPager(nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		for _, it := range pg.Segment.BlobItems {
			listed[*it.Name] = true
		}
	}
	for _, n := range names {
		if !listed[n] {
			t.Errorf("name %q missing from the listing (got %d names)", n, len(listed))
		}
	}
}

func TestBlob_ListFlatPagesWithMaxResultsAndPrefix(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "lf")
	for i := 0; i < 25; i++ {
		c.NewBlockBlobClient(fmt.Sprintf("a/%02d", i)).UploadBuffer(ctx, []byte("x"), nil)
	}
	for i := 0; i < 5; i++ {
		c.NewBlockBlobClient(fmt.Sprintf("b/%02d", i)).UploadBuffer(ctx, []byte("x"), nil)
	}
	pre := "a/"
	pages, n := 0, 0
	prev := ""
	for p := c.NewListBlobsFlatPager(&container.ListBlobsFlatOptions{Prefix: &pre, MaxResults: ptr(int32(10))}); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		pages++
		for _, it := range pg.Segment.BlobItems {
			if *it.Name <= prev {
				t.Fatalf("not sorted: %s after %s", *it.Name, prev)
			}
			prev = *it.Name
			n++
		}
	}
	if n != 25 || pages != 3 {
		t.Fatalf("n=%d pages=%d", n, pages)
	}
}

func TestBlob_ListHierarchyWithDelimiterReturnsPrefixesAndBlobs(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "lh")
	for _, n := range []string{"root.txt", "docs/a.txt", "docs/b.txt", "docs/sub/c.txt", "img/x.png", "img/y.png"} {
		c.NewBlockBlobClient(n).UploadBuffer(ctx, []byte("x"), nil)
	}
	top := map[string]bool{}
	var items []string
	for p := c.NewListBlobsHierarchyPager("/", nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		for _, pr := range pg.Segment.BlobPrefixes {
			top[*pr.Name] = true
		}
		for _, it := range pg.Segment.BlobItems {
			items = append(items, *it.Name)
		}
	}
	if len(top) != 2 || !top["docs/"] || !top["img/"] || len(items) != 1 || items[0] != "root.txt" {
		t.Fatalf("prefixes %v items %v", top, items)
	}
	pre := "docs/"
	top = map[string]bool{}
	items = nil
	for p := c.NewListBlobsHierarchyPager("/", &container.ListBlobsHierarchyOptions{Prefix: &pre}); p.More(); {
		pg, _ := p.NextPage(ctx)
		for _, pr := range pg.Segment.BlobPrefixes {
			top[*pr.Name] = true
		}
		for _, it := range pg.Segment.BlobItems {
			items = append(items, *it.Name)
		}
	}
	if len(top) != 1 || !top["docs/sub/"] || len(items) != 2 {
		t.Fatalf("under docs/: prefixes %v items %v", top, items)
	}
}

func TestBlob_ListIncludesMetadataAndTagsWhenAsked(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "li")
	c.NewBlockBlobClient("m").UploadBuffer(ctx, []byte("x"), &blockblob.UploadBufferOptions{Metadata: map[string]*string{"k": ptr("v")}, Tags: map[string]string{"t": "1"}})
	for p := c.NewListBlobsFlatPager(&container.ListBlobsFlatOptions{Include: container.ListBlobsInclude{Metadata: true, Tags: true}}); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		it := pg.Segment.BlobItems[0]
		if metaGet(it.Metadata, "k") != "v" {
			t.Fatalf("metadata %v", it.Metadata)
		}
		if it.BlobTags == nil || len(it.BlobTags.BlobTagSet) != 1 {
			t.Fatalf("tags %v", it.BlobTags)
		}
	}
}

func TestBlob_ConditionalUploadIfNoneMatchStarFailsOnAnExistingBlob(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "cond")
	b := c.NewBlockBlobClient("c")
	b.UploadBuffer(ctx, []byte("v1"), nil)
	star := azcoreETag("*")
	_, err := b.UploadBuffer(ctx, []byte("v2"), &blockblob.UploadBufferOptions{AccessConditions: &blob.AccessConditions{ModifiedAccessConditions: &blob.ModifiedAccessConditions{IfNoneMatch: &star}}})
	wantCode(t, err, http.StatusConflict, bloberror.BlobAlreadyExists)
	mustEqual(t, download(t, b.BlobClient()), []byte("v1"))
}

func TestBlob_IfMatchWithAStaleETagIsConditionNotMet412(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "ifm")
	b := c.NewBlockBlobClient("c")
	first, _ := b.UploadBuffer(ctx, []byte("v1"), nil)
	b.UploadBuffer(ctx, []byte("v2"), nil)
	_, err := b.UploadBuffer(ctx, []byte("v3"), &blockblob.UploadBufferOptions{AccessConditions: &blob.AccessConditions{ModifiedAccessConditions: &blob.ModifiedAccessConditions{IfMatch: first.ETag}}})
	wantCode(t, err, http.StatusPreconditionFailed, bloberror.ConditionNotMet)
}

func TestBlob_DownloadWithIfNoneMatchOfCurrentETagIs304NotModified(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "inm")
	b := c.NewBlockBlobClient("c")
	up, _ := b.UploadBuffer(ctx, []byte("v1"), nil)
	// The generated Download code lists 304 among the accepted statuses, so the SDK answers without an error and with a nil Body.
	r, err := b.DownloadStream(ctx, &blob.DownloadStreamOptions{AccessConditions: &blob.AccessConditions{ModifiedAccessConditions: &blob.ModifiedAccessConditions{IfNoneMatch: up.ETag}}})
	if err != nil {
		if !strings.Contains(err.Error(), "304") && !bloberror.HasCode(err, bloberror.ConditionNotMet) {
			t.Fatalf("unexpected error: %v", err)
		}
		return
	}
	if r.Body != nil && len(readAll(t, r.Body)) != 0 {
		t.Fatal("a 304 answer must have no body")
	}
}
