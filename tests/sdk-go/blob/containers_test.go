package blob

import (
	"bytes"
	"context"
	"encoding/base64"
	"fmt"
	"net/http"
	"testing"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/bloberror"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/container"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/service"
)

func wantCode(t testing.TB, err error, status int, code bloberror.Code) {
	t.Helper()
	if err == nil {
		t.Fatalf("expected %d %s, got success", status, code)
	}
	var re *azcore.ResponseError
	if !asRE(err, &re) {
		t.Fatalf("not a ResponseError: %v", err)
	}
	if re.StatusCode != status {
		t.Fatalf("status %d, want %d: %v", re.StatusCode, status, err)
	}
	if !bloberror.HasCode(err, code) {
		t.Fatalf("error code %q, want %q", re.ErrorCode, code)
	}
}

func TestContainer_CreateTwiceIsContainerAlreadyExists409(t *testing.T) {
	c := newContainer(t, "dup")
	_, err := c.Create(context.Background(), nil)
	wantCode(t, err, http.StatusConflict, bloberror.ContainerAlreadyExists)
}

func TestContainer_GetPropertiesOnMissingContainerIsContainerNotFound404(t *testing.T) {
	s := newSvc(t)
	_, err := s.NewContainerClient(emu.Name("missing")).GetProperties(context.Background(), nil)
	wantCode(t, err, http.StatusNotFound, bloberror.ContainerNotFound)
}

func TestContainer_MetadataRoundTripsAndNamesComeBackLowerCased(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "meta")
	if _, err := c.SetMetadata(ctx, &container.SetMetadataOptions{Metadata: map[string]*string{"Owner": ptr("alice"), "project": ptr("x y")}}); err != nil {
		t.Fatal(err)
	}
	p, err := c.GetProperties(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	// Go's HTTP stack canonicalises header names, the SDK keeps the case the service sent after "x-ms-meta-".
	got := map[string]string{}
	for k, v := range p.Metadata {
		got[lower(k)] = *v
	}
	if got["owner"] != "alice" || got["project"] != "x y" {
		t.Fatalf("metadata %v", got)
	}
}

func TestContainer_ListContainersPagesByMaxResultsAndFiltersByPrefix(t *testing.T) {
	ctx := context.Background()
	s := newSvc(t)
	pre := emu.Name("lsc")
	for i := 0; i < 5; i++ {
		if _, err := s.CreateContainer(ctx, fmt.Sprintf("%sx%d", pre, i), nil); err != nil {
			t.Fatal(err)
		}
	}
	s.CreateContainer(ctx, emu.Name("other"), nil)
	pager := s.NewListContainersPager(&service.ListContainersOptions{Prefix: &pre, MaxResults: ptr(int32(2)), Include: service.ListContainersInclude{Metadata: true}})
	pages, n := 0, 0
	for pager.More() {
		pg, err := pager.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		pages++
		n += len(pg.ContainerItems)
		if len(pg.ContainerItems) > 2 {
			t.Fatalf("page has %d items", len(pg.ContainerItems))
		}
	}
	if n != 5 || pages != 3 {
		t.Fatalf("n=%d pages=%d", n, pages)
	}
}

func TestContainer_DeleteThenGetIsNotFoundAndRecreateWorks(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "del")
	if _, err := c.Delete(ctx, nil); err != nil {
		t.Fatal(err)
	}
	_, err := c.GetProperties(ctx, nil)
	wantCode(t, err, http.StatusNotFound, bloberror.ContainerNotFound)
	if _, err := c.Create(ctx, nil); err != nil {
		t.Fatalf("recreate: %v", err)
	}
}

func TestContainer_PublicAccessBlobAllowsAnonymousReadsOnly(t *testing.T) {
	ctx := context.Background()
	s := newSvc(t)
	name := emu.Name("pub")
	if _, err := s.CreateContainer(ctx, name, &service.CreateContainerOptions{Access: ptr(azblob.PublicAccessTypeBlob)}); err != nil {
		t.Fatal(err)
	}
	c := s.NewContainerClient(name)
	if _, err := c.NewBlockBlobClient("p.txt").UploadBuffer(ctx, []byte("public"), nil); err != nil {
		t.Fatal(err)
	}
	anon, err := container.NewClientWithNoCredential(c.URL(), nil)
	if err != nil {
		t.Fatal(err)
	}
	r, err := anon.NewBlobClient("p.txt").DownloadStream(ctx, nil)
	if err != nil {
		t.Fatalf("anonymous read of public blob: %v", err)
	}
	mustEqual(t, readAll(t, r.Body), []byte("public"))
	_, err = anon.NewBlockBlobClient("w.txt").UploadBuffer(ctx, []byte("x"), nil)
	if err == nil {
		t.Fatal("anonymous write must fail")
	}
	var re *azcore.ResponseError
	if !asRE(err, &re) || (re.StatusCode != 401 && re.StatusCode != 403) {
		t.Fatalf("anonymous write: %v", err)
	}
}

func TestContainer_StoredAccessPolicyRoundTrips(t *testing.T) {
	ctx := context.Background()
	c := newContainer(t, "acl")
	_, err := c.SetAccessPolicy(ctx, &container.SetAccessPolicyOptions{ContainerACL: []*container.SignedIdentifier{{
		ID: ptr("policy1"), AccessPolicy: &container.AccessPolicy{Permission: ptr("rl")}}}})
	if err != nil {
		t.Fatal(err)
	}
	got, err := c.GetAccessPolicy(ctx, nil)
	if err != nil || len(got.SignedIdentifiers) != 1 || *got.SignedIdentifiers[0].ID != "policy1" || *got.SignedIdentifiers[0].AccessPolicy.Permission != "rl" {
		t.Fatalf("acl: %+v %v", got.SignedIdentifiers, err)
	}
}

func TestSharedKey_WrongKeyIsRefusedWithAuthenticationFailed403(t *testing.T) {
	// a different but valid base64 key for the well-known account
	wrong, err := azblob.NewSharedKeyCredential(emu.DevAccount, base64.StdEncoding.EncodeToString(bytes.Repeat([]byte("x"), 64)))
	if err != nil {
		t.Fatal(err)
	}
	s, _ := service.NewClientWithSharedKeyCredential(serviceURL(), wrong, nil)
	_, err = s.CreateContainer(context.Background(), emu.Name("wrongkey"), nil)
	wantCode(t, err, http.StatusForbidden, bloberror.AuthenticationFailed)
}

func TestSharedKey_RightKeyWorksForTheDevelopmentAccount(t *testing.T) {
	s := newSvc(t)
	if _, err := s.GetProperties(context.Background(), nil); err != nil {
		t.Fatal(err)
	}
}

func TestService_AccountInformationReportsSKUAndKind(t *testing.T) {
	r, err := newSvc(t).GetAccountInfo(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	if r.SKUName == nil || r.AccountKind == nil {
		t.Fatalf("%+v", r)
	}
}
