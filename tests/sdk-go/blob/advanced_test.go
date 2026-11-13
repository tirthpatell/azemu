package blob

import (
	"bytes"
	"context"
	"fmt"
	"net/http"
	"strings"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore/streaming"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/appendblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/blob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/bloberror"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/blockblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/container"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/lease"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/pageblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/sas"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/service"
)

// ---- leases ----

func TestLease_WriteWithoutTheLeaseIdIsLeaseIdMissing412AndWithItWorks(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "lease")
	b := c.NewBlockBlobClient("l")
	b.UploadBuffer(ctx, []byte("v1"), nil)
	lc, err := lease.NewBlobClient(b, nil)
	if err != nil {
		t.Fatal(err)
	}
	acq, err := lc.AcquireLease(ctx, 60, nil)
	if err != nil || acq.LeaseID == nil {
		t.Fatalf("acquire: %v", err)
	}
	_, err = b.UploadBuffer(ctx, []byte("v2"), nil)
	wantCode(t, err, http.StatusPreconditionFailed, bloberror.LeaseIDMissing)
	_, err = b.Upload(ctx, streaming.NopCloser(strings.NewReader("v2")), &blockblob.UploadOptions{AccessConditions: &blob.AccessConditions{LeaseAccessConditions: &blob.LeaseAccessConditions{LeaseID: acq.LeaseID}}})
	if err != nil {
		t.Fatalf("write with lease: %v", err)
	}
	p, _ := b.GetProperties(ctx, nil)
	if *p.LeaseState != lease.StateTypeLeased || *p.LeaseStatus != lease.StatusTypeLocked || *p.LeaseDuration != lease.DurationTypeFixed {
		t.Fatalf("lease props: %v %v %v", *p.LeaseState, *p.LeaseStatus, *p.LeaseDuration)
	}
}

func TestLease_AcquireTwiceIsLeaseAlreadyPresent409(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "lease2")
	b := c.NewBlockBlobClient("l")
	b.UploadBuffer(ctx, []byte("v1"), nil)
	l1, _ := lease.NewBlobClient(b, nil)
	l2, _ := lease.NewBlobClient(b, nil)
	l1.AcquireLease(ctx, 30, nil)
	_, err := l2.AcquireLease(ctx, 30, nil)
	wantCode(t, err, http.StatusConflict, bloberror.LeaseAlreadyPresent)
}

func TestLease_ChangeRenewReleaseAndBreakFollowTheStateMachine(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "lease3")
	b := c.NewBlockBlobClient("l")
	b.UploadBuffer(ctx, []byte("v1"), nil)
	lc, _ := lease.NewBlobClient(b, nil)
	acq, _ := lc.AcquireLease(ctx, -1, nil) // infinite
	p, _ := b.GetProperties(ctx, nil)
	if *p.LeaseDuration != lease.DurationTypeInfinite {
		t.Fatalf("duration %v", *p.LeaseDuration)
	}
	proposed := "11111111-2222-3333-4444-555555555555"
	ch, err := lc.ChangeLease(ctx, proposed, nil)
	if err != nil || *ch.LeaseID != proposed {
		t.Fatalf("change: %v", err)
	}
	if _, err := lc.RenewLease(ctx, nil); err != nil {
		t.Fatalf("renew: %v", err)
	}
	br, err := lc.BreakLease(ctx, &lease.BlobBreakOptions{BreakPeriod: ptr(int32(0))})
	if err != nil {
		t.Fatalf("break: %v", err)
	}
	if br.LeaseTime == nil || *br.LeaseTime != 0 {
		t.Fatalf("lease time %v", br.LeaseTime)
	}
	// broken: anyone may acquire
	other, _ := lease.NewBlobClient(b, nil)
	if _, err := other.AcquireLease(ctx, 15, nil); err != nil {
		t.Fatalf("acquire after break: %v", err)
	}
	_ = acq
	if _, err := other.ReleaseLease(ctx, nil); err != nil {
		t.Fatalf("release: %v", err)
	}
	p, _ = b.GetProperties(ctx, nil)
	if *p.LeaseState != lease.StateTypeAvailable {
		t.Fatalf("state %v", *p.LeaseState)
	}
}

func TestLease_ContainerLeaseBlocksDeleteWithoutLeaseId(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "clease")
	lc, err := lease.NewContainerClient(c, nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := lc.AcquireLease(ctx, 60, nil); err != nil {
		t.Fatal(err)
	}
	_, err = c.Delete(ctx, nil)
	wantCode(t, err, http.StatusPreconditionFailed, bloberror.LeaseIDMissing)
	if _, err := c.Delete(ctx, &container.DeleteOptions{AccessConditions: &container.AccessConditions{LeaseAccessConditions: &container.LeaseAccessConditions{LeaseID: lc.LeaseID()}}}); err != nil {
		t.Fatalf("delete with lease: %v", err)
	}
}

// ---- snapshots, versions, copy ----

func TestSnapshot_OldContentStaysReadableThroughTheSnapshotAfterOverwrite(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "snap")
	b := c.NewBlockBlobClient("s")
	b.UploadBuffer(ctx, []byte("original"), nil)
	snap, err := b.CreateSnapshot(ctx, nil)
	if err != nil || snap.Snapshot == nil {
		t.Fatalf("snapshot: %v", err)
	}
	b.UploadBuffer(ctx, []byte("changed"), nil)
	sb, err := b.WithSnapshot(*snap.Snapshot)
	if err != nil {
		t.Fatal(err)
	}
	mustEqual(t, download(t, sb.BlobClient()), []byte("original"))
	mustEqual(t, download(t, b.BlobClient()), []byte("changed"))
	n := 0
	for p := c.NewListBlobsFlatPager(&container.ListBlobsFlatOptions{Include: container.ListBlobsInclude{Snapshots: true}}); p.More(); {
		pg, _ := p.NextPage(ctx)
		n += len(pg.Segment.BlobItems)
	}
	if n != 2 {
		t.Fatalf("listing with snapshots: %d items", n)
	}
}

func TestSnapshot_DeletingABlobWithSnapshotsNeedsDeleteSnapshotsOption(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "snapdel")
	b := c.NewBlockBlobClient("s")
	b.UploadBuffer(ctx, []byte("x"), nil)
	b.CreateSnapshot(ctx, nil)
	_, err := b.Delete(ctx, nil)
	wantCode(t, err, http.StatusConflict, bloberror.SnapshotsPresent)
	if _, err := b.Delete(ctx, &blob.DeleteOptions{DeleteSnapshots: ptr(blob.DeleteSnapshotsOptionTypeInclude)}); err != nil {
		t.Fatal(err)
	}
}

func TestVersions_AnAccountWithoutVersioningKeepsNoVersions(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "nover")
	b := c.NewBlockBlobClient("v")
	for i := 1; i <= 2; i++ {
		r, err := b.UploadBuffer(ctx, []byte(fmt.Sprintf("version %d", i)), nil)
		if err != nil {
			t.Fatal(err)
		}
		if r.VersionID != nil {
			t.Fatalf("an account without versioning answered a version id: %s", *r.VersionID)
		}
	}
	n := 0
	for p := c.NewListBlobsFlatPager(&container.ListBlobsFlatOptions{Include: container.ListBlobsInclude{Versions: true}}); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		n += len(pg.Segment.BlobItems)
	}
	if n != 1 {
		t.Fatalf("listed %d entries, want 1", n)
	}
}

// Versioning is switched on for this test's own account through the management API (ARM), the way a team does it.
func TestVersions_EachOverwriteMakesAVersionThatCanBeReadByVersionId(t *testing.T) {
	ctx := context.Background()
	account := emu.AccountWithVersioning(t, "verify")
	key, err := azblob.NewSharedKeyCredential(account, emu.DevKey)
	if err != nil {
		t.Fatal(err)
	}
	s, err := service.NewClientWithSharedKeyCredential(emu.E.HTTP("blob", account), key, nil)
	if err != nil {
		t.Fatal(err)
	}
	cname := emu.Name("ver")
	if _, err := s.CreateContainer(ctx, cname, nil); err != nil {
		t.Fatal(err)
	}
	c := s.NewContainerClient(cname)
	b := c.NewBlockBlobClient("v")
	var ids []string
	for i := 1; i <= 3; i++ {
		r, err := b.UploadBuffer(ctx, []byte(fmt.Sprintf("version %d", i)), nil)
		if err != nil {
			t.Fatal(err)
		}
		if r.VersionID == nil {
			t.Fatal("versioning was switched on for this account and the write answered no version id")
		}
		ids = append(ids, *r.VersionID)
	}
	if ids[0] == ids[1] || ids[1] == ids[2] {
		t.Fatalf("version ids repeat: %v", ids)
	}
	vb, err := b.WithVersionID(ids[0])
	if err != nil {
		t.Fatal(err)
	}
	mustEqual(t, download(t, vb.BlobClient()), []byte("version 1"))
	n, cur := 0, 0
	for p := c.NewListBlobsFlatPager(&container.ListBlobsFlatOptions{Include: container.ListBlobsInclude{Versions: true}}); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		for _, it := range pg.Segment.BlobItems {
			n++
			if it.IsCurrentVersion != nil && *it.IsCurrentVersion {
				cur++
			}
		}
	}
	if n != 3 || cur != 1 {
		t.Fatalf("versions listed %d, current %d", n, cur)
	}
	// deleting the base blob keeps the versions
	b.Delete(ctx, nil)
	mustEqual(t, download(t, vb.BlobClient()), []byte("version 1"))
}

func TestCopy_StartCopyFromURLInTheSameAccountCompletesWithSuccessStatus(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "cp")
	src := c.NewBlockBlobClient("src")
	data := randBytes(300_000)
	src.UploadBuffer(ctx, data, &blockblob.UploadBufferOptions{Metadata: map[string]*string{"origin": ptr("src")}})
	dst := c.NewBlockBlobClient("dst")
	cp, err := dst.StartCopyFromURL(ctx, src.URL(), nil)
	if err != nil {
		t.Fatal(err)
	}
	if cp.CopyID == nil || cp.CopyStatus == nil {
		t.Fatal("no copy id/status")
	}
	deadline := time.Now().Add(10 * time.Second)
	for {
		p, _ := dst.GetProperties(ctx, nil)
		if p.CopyStatus != nil && *p.CopyStatus == blob.CopyStatusTypeSuccess {
			if metaGet(p.Metadata, "origin") != "src" {
				t.Fatalf("copy should carry metadata: %v", p.Metadata)
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("copy status %v", p.CopyStatus)
		}
		time.Sleep(100 * time.Millisecond)
	}
	mustEqual(t, download(t, dst.BlobClient()), data)
}

func TestCopy_SynchronousCopyFromURLWithASasOnTheSource(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "cpsas")
	src := c.NewBlockBlobClient("src")
	src.UploadBuffer(ctx, []byte("sync copy"), nil)
	srcSAS, err := src.GetSASURL(sas.BlobPermissions{Read: true}, time.Now().Add(time.Hour), nil)
	if err != nil {
		t.Fatal(err)
	}
	dst := c.NewBlockBlobClient("dst")
	r, err := dst.CopyFromURL(ctx, srcSAS, nil)
	if err != nil {
		t.Fatal(err)
	}
	if *r.CopyStatus != "success" {
		t.Fatalf("status %v", *r.CopyStatus)
	}
	mustEqual(t, download(t, dst.BlobClient()), []byte("sync copy"))
}

func TestCopy_FromAMissingSourceIsAnError(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "cpmiss")
	dst := c.NewBlockBlobClient("dst")
	_, err := dst.StartCopyFromURL(ctx, c.NewBlobClient("ghost").URL(), nil)
	if err == nil {
		t.Fatal("copy of a missing source must fail")
	}
}

// ---- SAS ----

func TestSAS_ServiceSasForOneBlobReadsThatBlobWithAnAnonymousClient(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "sas")
	b := c.NewBlockBlobClient("a b+c.txt") // a name that must be escaped in the signed path
	b.UploadBuffer(ctx, []byte("sas content"), nil)
	u, err := b.GetSASURL(sas.BlobPermissions{Read: true}, time.Now().Add(time.Hour), nil)
	if err != nil {
		t.Fatal(err)
	}
	anon, err := blob.NewClientWithNoCredential(u, nil)
	if err != nil {
		t.Fatal(err)
	}
	mustEqual(t, download(t, anon), []byte("sas content"))
	// the same SAS does not open another blob. The emulator checks signatures for sv up to 2026-10-06 only (see docs/sdk-notes.md),
	// so sign with an older version for this part.
	other := c.NewBlockBlobClient("other.txt")
	other.UploadBuffer(ctx, []byte("secret"), nil)
	cname := c.URL()[strings.LastIndex(c.URL(), "/")+1:]
	q, err := sas.BlobSignatureValues{Version: "2020-12-06", ExpiryTime: time.Now().Add(time.Hour), Permissions: "r", ContainerName: cname, BlobName: "a b+c.txt"}.SignWithSharedKey(devCred(t))
	if err != nil {
		t.Fatal(err)
	}
	anonOther, _ := blob.NewClientWithNoCredential(other.URL()+"?"+q.Encode(), nil)
	_, err = anonOther.DownloadStream(ctx, nil)
	wantCode(t, err, http.StatusForbidden, bloberror.AuthenticationFailed)
	anonOwn, _ := blob.NewClientWithNoCredential(b.URL()+"?"+q.Encode(), nil)
	mustEqual(t, download(t, anonOwn), []byte("sas content"))
}

func TestSAS_ReadOnlySasCannotWriteAuthorizationPermissionMismatch403(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "sasro")
	b := c.NewBlockBlobClient("a")
	b.UploadBuffer(ctx, []byte("x"), nil)
	u, _ := b.GetSASURL(sas.BlobPermissions{Read: true}, time.Now().Add(time.Hour), nil)
	anon, _ := blockblob.NewClientWithNoCredential(u, nil)
	_, err := anon.UploadBuffer(ctx, []byte("y"), nil)
	wantCode(t, err, http.StatusForbidden, bloberror.AuthorizationPermissionMismatch)
}

func TestSAS_ExpiredSasIsAuthenticationFailed403(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "sasexp")
	b := c.NewBlockBlobClient("a")
	b.UploadBuffer(ctx, []byte("x"), nil)
	vals := sas.BlobSignatureValues{Protocol: sas.ProtocolHTTPSandHTTP, StartTime: time.Now().Add(-2 * time.Hour), ExpiryTime: time.Now().Add(-time.Hour),
		Permissions: (&sas.BlobPermissions{Read: true}).String(), ContainerName: c.URL()[strings.LastIndex(c.URL(), "/")+1:], BlobName: "a"}
	q, err := vals.SignWithSharedKey(devCred(t))
	if err != nil {
		t.Fatal(err)
	}
	anon, _ := blob.NewClientWithNoCredential(b.URL()+"?"+q.Encode(), nil)
	_, err = anon.DownloadStream(ctx, nil)
	wantCode(t, err, http.StatusForbidden, bloberror.AuthenticationFailed)
}

func TestSAS_TamperedSignatureOnTheDevelopmentAccountIsRefused(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "sastamper")
	b := c.NewBlockBlobClient("a")
	b.UploadBuffer(ctx, []byte("x"), nil)
	cname := c.URL()[strings.LastIndex(c.URL(), "/")+1:]
	q, _ := sas.BlobSignatureValues{Version: "2020-12-06", ExpiryTime: time.Now().Add(time.Hour), Permissions: "r", ContainerName: cname, BlobName: "a"}.SignWithSharedKey(devCred(t))
	// widen the permissions without re-signing
	u := b.URL() + "?" + strings.Replace(q.Encode(), "sp=r", "sp=racwd", 1)
	anon, _ := blob.NewClientWithNoCredential(u, nil)
	_, err := anon.DownloadStream(ctx, nil)
	wantCode(t, err, http.StatusForbidden, bloberror.AuthenticationFailed)
}

func TestSAS_TamperedSignatureWithTheSdksNewestVersionIsRefused(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "sastamper2")
	b := c.NewBlockBlobClient("a")
	b.UploadBuffer(ctx, []byte("x"), nil)
	u, _ := b.GetSASURL(sas.BlobPermissions{Read: true}, time.Now().Add(time.Hour), nil)
	u = strings.Replace(u, "sp=r", "sp=racwd", 1)
	anon, _ := blob.NewClientWithNoCredential(u, nil)
	_, err := anon.DownloadStream(ctx, nil)
	wantCode(t, err, http.StatusForbidden, bloberror.AuthenticationFailed)
}

func TestSAS_ContainerSasListsAndWritesBlobsWithAnotherClient(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "sasc")
	u, err := c.GetSASURL(sas.ContainerPermissions{Read: true, Write: true, List: true, Create: true}, time.Now().Add(time.Hour), nil)
	if err != nil {
		t.Fatal(err)
	}
	cc, err := container.NewClientWithNoCredential(u, nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := cc.NewBlockBlobClient("from-sas").UploadBuffer(ctx, []byte("sas write"), nil); err != nil {
		t.Fatalf("upload with container SAS: %v", err)
	}
	n := 0
	for p := cc.NewListBlobsFlatPager(nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatalf("list with container SAS: %v", err)
		}
		n += len(pg.Segment.BlobItems)
	}
	if n != 1 {
		t.Fatalf("listed %d", n)
	}
	// but a container SAS without delete cannot delete
	_, err = cc.NewBlobClient("from-sas").Delete(ctx, nil)
	wantCode(t, err, http.StatusForbidden, bloberror.AuthorizationPermissionMismatch)
}

func TestSAS_StoredAccessPolicyIdentifierControlsPermissionsAndExpiry(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "sasid")
	expiry := time.Now().Add(time.Hour).UTC()
	_, err := c.SetAccessPolicy(ctx, &container.SetAccessPolicyOptions{ContainerACL: []*container.SignedIdentifier{{
		ID: ptr("readpol"), AccessPolicy: &container.AccessPolicy{Permission: ptr("r"), Expiry: &expiry}}}})
	if err != nil {
		t.Fatal(err)
	}
	cname := c.URL()[strings.LastIndex(c.URL(), "/")+1:]
	b := c.NewBlockBlobClient("a")
	b.UploadBuffer(ctx, []byte("by policy"), nil)
	q, err := sas.BlobSignatureValues{Identifier: "readpol", ContainerName: cname, BlobName: "a"}.SignWithSharedKey(devCred(t))
	if err != nil {
		t.Fatal(err)
	}
	anon, _ := blob.NewClientWithNoCredential(b.URL()+"?"+q.Encode(), nil)
	mustEqual(t, download(t, anon), []byte("by policy"))
	// unknown policy id
	q, _ = sas.BlobSignatureValues{Identifier: "nopolicy", ContainerName: cname, BlobName: "a"}.SignWithSharedKey(devCred(t))
	anon, _ = blob.NewClientWithNoCredential(b.URL()+"?"+q.Encode(), nil)
	_, err = anon.DownloadStream(ctx, nil)
	wantCode(t, err, http.StatusForbidden, bloberror.AuthenticationFailed)
}

func TestSAS_AccountSasListsContainersAndRefusesOutsideItsResourceTypes(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "sasacct")
	c.NewBlockBlobClient("a").UploadBuffer(ctx, []byte("x"), nil)
	s := newSvc(t)
	u, err := s.GetSASURL(sas.AccountResourceTypes{Service: true, Container: true}, sas.AccountPermissions{Read: true, List: true}, time.Now().Add(time.Hour), nil)
	if err != nil {
		t.Fatal(err)
	}
	sc, err := service.NewClientWithNoCredential(u, nil)
	if err != nil {
		t.Fatal(err)
	}
	found := false
	pre := c.URL()[strings.LastIndex(c.URL(), "/")+1:]
	for p := sc.NewListContainersPager(&service.ListContainersOptions{Prefix: &pre}); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatalf("list containers with account SAS: %v", err)
		}
		found = found || len(pg.ContainerItems) == 1
	}
	if !found {
		t.Fatal("container not listed")
	}
	// object-level read needs the Object resource type
	anon, _ := blob.NewClientWithNoCredential(c.NewBlobClient("a").URL()+u[strings.Index(u, "?"):], nil)
	_, err = anon.DownloadStream(ctx, nil)
	wantCode(t, err, http.StatusForbidden, bloberror.AuthorizationResourceTypeMismatch)
}

func TestSAS_UserDelegationSasMadeFromAnOAuthClientWorksForAnotherClient(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "sasud")
	b := c.NewBlockBlobClient("ud.txt")
	b.UploadBuffer(ctx, []byte("delegated"), nil)
	// the delegation key is requested with a bearer token, which needs https
	httpsSvc, err := service.NewClient(emu.E.HTTPS("blob", emu.DevAccount), emu.Cred(), &service.ClientOptions{ClientOptions: coreOpts()})
	if err != nil {
		t.Fatal(err)
	}
	start := time.Now().UTC().Add(-5 * time.Minute)
	expiry := time.Now().UTC().Add(time.Hour)
	st, ex := start.Format(sas.TimeFormat), expiry.Format(sas.TimeFormat)
	cred, err := httpsSvc.GetUserDelegationCredential(ctx, service.KeyInfo{Start: &st, Expiry: &ex}, nil)
	if err != nil {
		t.Fatalf("get user delegation key: %v", err)
	}
	cname := c.URL()[strings.LastIndex(c.URL(), "/")+1:]
	q, err := sas.BlobSignatureValues{Protocol: sas.ProtocolHTTPSandHTTP, StartTime: start, ExpiryTime: expiry,
		Permissions: (&sas.BlobPermissions{Read: true}).String(), ContainerName: cname, BlobName: "ud.txt"}.SignWithUserDelegation(cred)
	if err != nil {
		t.Fatal(err)
	}
	anon, _ := blob.NewClientWithNoCredential(b.URL()+"?"+q.Encode(), nil)
	mustEqual(t, download(t, anon), []byte("delegated"))
}

// ---- batch ----

func TestBatch_DeleteManyBlobsInOneRequestReportsEachStatus(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "batch")
	for i := 0; i < 5; i++ {
		c.NewBlockBlobClient(fmt.Sprintf("b%d", i)).UploadBuffer(ctx, []byte("x"), nil)
	}
	bb, err := c.NewBatchBuilder()
	if err != nil {
		t.Fatal(err)
	}
	for _, n := range []string{"b0", "b1", "b2", "missing"} {
		if err := bb.Delete(n, nil); err != nil {
			t.Fatal(err)
		}
	}
	res, err := c.SubmitBatch(ctx, bb, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Responses) != 4 {
		t.Fatalf("responses %d", len(res.Responses))
	}
	for _, r := range res.Responses {
		want := true
		if *r.BlobName == "missing" {
			want = false
		}
		if want && r.Error != nil {
			t.Fatalf("delete %s: %v", *r.BlobName, r.Error)
		}
		if !want && (r.Error == nil || !bloberror.HasCode(r.Error, bloberror.BlobNotFound)) {
			t.Fatalf("delete of a missing blob: %v", r.Error)
		}
	}
	n := 0
	for p := c.NewListBlobsFlatPager(nil); p.More(); {
		pg, _ := p.NextPage(ctx)
		n += len(pg.Segment.BlobItems)
	}
	if n != 2 {
		t.Fatalf("%d blobs left, want 2", n)
	}
}

func TestBatch_SetTierOnManyBlobsAtServiceLevel(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "batchtier")
	cname := c.URL()[strings.LastIndex(c.URL(), "/")+1:]
	for i := 0; i < 3; i++ {
		c.NewBlockBlobClient(fmt.Sprintf("b%d", i)).UploadBuffer(ctx, []byte("x"), nil)
	}
	s := newSvc(t)
	bb, _ := s.NewBatchBuilder()
	for i := 0; i < 3; i++ {
		if err := bb.SetTier(cname, fmt.Sprintf("b%d", i), blob.AccessTierCool, nil); err != nil {
			t.Fatal(err)
		}
	}
	res, err := s.SubmitBatch(ctx, bb, nil)
	if err != nil {
		t.Fatal(err)
	}
	for _, r := range res.Responses {
		if r.Error != nil {
			t.Fatalf("tier %s: %v", *r.BlobName, r.Error)
		}
	}
	p, _ := c.NewBlobClient("b1").GetProperties(ctx, nil)
	if *p.AccessTier != "Cool" {
		t.Fatalf("tier %v", *p.AccessTier)
	}
}

// ---- append and page blobs ----

func TestAppendBlob_AppendBlocksConcatenateInOrderAndReportCommittedBlockCount(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "app")
	b := c.NewAppendBlobClient("log")
	if _, err := b.Create(ctx, nil); err != nil {
		t.Fatal(err)
	}
	var want []byte
	for i := 0; i < 4; i++ {
		part := []byte(fmt.Sprintf("line %d\n", i))
		want = append(want, part...)
		r, err := b.AppendBlock(ctx, streaming.NopCloser(bytes.NewReader(part)), nil)
		if err != nil {
			t.Fatal(err)
		}
		if *r.BlobCommittedBlockCount != int32(i+1) {
			t.Fatalf("committed block count %d after %d appends", *r.BlobCommittedBlockCount, i+1)
		}
	}
	mustEqual(t, download(t, b.BlobClient()), want)
	p, _ := b.GetProperties(ctx, nil)
	if *p.BlobType != blob.BlobTypeAppendBlob {
		t.Fatalf("type %v", *p.BlobType)
	}
}

func TestAppendBlob_AppendPositionConditionIsEnforced412(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "apppos")
	b := c.NewAppendBlobClient("log")
	b.Create(ctx, nil)
	b.AppendBlock(ctx, streaming.NopCloser(strings.NewReader("12345")), nil)
	_, err := b.AppendBlock(ctx, streaming.NopCloser(strings.NewReader("6")), &appendblob.AppendBlockOptions{
		AppendPositionAccessConditions: &appendblob.AppendPositionAccessConditions{AppendPosition: ptr(int64(0))}})
	wantCode(t, err, http.StatusPreconditionFailed, bloberror.AppendPositionConditionNotMet)
	if _, err := b.AppendBlock(ctx, streaming.NopCloser(strings.NewReader("6")), &appendblob.AppendBlockOptions{
		AppendPositionAccessConditions: &appendblob.AppendPositionAccessConditions{AppendPosition: ptr(int64(5))}}); err != nil {
		t.Fatalf("append at the right position: %v", err)
	}
}

func TestAppendBlob_SealedBlobRefusesFurtherAppends(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "appseal")
	b := c.NewAppendBlobClient("log")
	b.Create(ctx, nil)
	b.AppendBlock(ctx, streaming.NopCloser(strings.NewReader("x")), nil)
	if _, err := b.Seal(ctx, nil); err != nil {
		t.Fatal(err)
	}
	_, err := b.AppendBlock(ctx, streaming.NopCloser(strings.NewReader("y")), nil)
	wantCode(t, err, http.StatusConflict, bloberror.Code("BlobIsSealed"))
}

func TestPageBlob_UploadedPagesReadBackAndUnwrittenRangesAreZero(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "page")
	b := c.NewPageBlobClient("disk")
	if _, err := b.Create(ctx, 2048, nil); err != nil {
		t.Fatal(err)
	}
	data := randBytes(512)
	if _, err := b.UploadPages(ctx, streaming.NopCloser(bytes.NewReader(data)), blob.HTTPRange{Offset: 512, Count: 512}, nil); err != nil {
		t.Fatalf("upload pages: %v", err)
	}
	got := download(t, b.BlobClient())
	if len(got) != 2048 {
		t.Fatalf("length %d", len(got))
	}
	mustEqual(t, got[512:1024], data)
	mustEqual(t, got[:512], make([]byte, 512))
	mustEqual(t, got[1024:], make([]byte, 1024))
	var ranges []*pageblob.PageRange
	for p := b.NewGetPageRangesPager(nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		ranges = append(ranges, pg.PageList.PageRange...)
	}
	if len(ranges) != 1 || *ranges[0].Start != 512 || *ranges[0].End != 1023 {
		t.Fatalf("ranges %+v", ranges)
	}
	if _, err := b.ClearPages(ctx, blob.HTTPRange{Offset: 512, Count: 512}, nil); err != nil {
		t.Fatalf("clear pages: %v", err)
	}
	mustEqual(t, download(t, b.BlobClient())[512:1024], make([]byte, 512))
}

func TestPageBlob_UnalignedRangeIsInvalidPageRange416(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "pagebad")
	b := c.NewPageBlobClient("disk")
	b.Create(ctx, 1024, nil)
	_, err := b.UploadPages(ctx, streaming.NopCloser(bytes.NewReader(make([]byte, 100))), blob.HTTPRange{Offset: 0, Count: 100}, nil)
	wantCode(t, err, http.StatusRequestedRangeNotSatisfiable, bloberror.InvalidPageRange)
}

func TestPageBlob_ResizeChangesTheLength(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "pagersz")
	b := c.NewPageBlobClient("disk")
	b.Create(ctx, 1024, nil)
	if _, err := b.Resize(ctx, 4096, nil); err != nil {
		t.Fatal(err)
	}
	p, _ := b.GetProperties(ctx, nil)
	if *p.ContentLength != 4096 {
		t.Fatalf("length %d", *p.ContentLength)
	}
}

func TestPageBlob_SequenceNumberConditionIsEnforced(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "pageseq")
	b := c.NewPageBlobClient("disk")
	b.Create(ctx, 1024, &pageblob.CreateOptions{SequenceNumber: ptr(int64(5))})
	_, err := b.UploadPages(ctx, streaming.NopCloser(bytes.NewReader(make([]byte, 512))), blob.HTTPRange{Offset: 0, Count: 512}, &pageblob.UploadPagesOptions{
		SequenceNumberAccessConditions: &pageblob.SequenceNumberAccessConditions{IfSequenceNumberLessThan: ptr(int64(3))}})
	wantCode(t, err, http.StatusPreconditionFailed, bloberror.SequenceNumberConditionNotMet)
}

func TestBlob_WrongBlobTypeOperationIsInvalidBlobType409(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "wrongtype")
	c.NewBlockBlobClient("b").UploadBuffer(ctx, []byte("x"), nil)
	_, err := c.NewAppendBlobClient("b").AppendBlock(ctx, streaming.NopCloser(strings.NewReader("y")), nil)
	if err == nil {
		t.Fatal("appending to a block blob must fail")
	}
	wantCode(t, err, http.StatusConflict, bloberror.InvalidBlobType)
}
