package blob

import (
	"bytes"
	"context"
	"crypto/rand"
	"errors"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/sas"
	"io"
	"strings"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/blob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/container"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/service"
)

func TestMain(m *testing.M) { emu.Main(m) }

// All tests use the well-known development account on plain HTTP; the emulator checks its Shared Key and SAS signatures.
func devCred(t testing.TB) *azblob.SharedKeyCredential {
	t.Helper()
	c, err := azblob.NewSharedKeyCredential(emu.DevAccount, emu.DevKey)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func serviceURL() string { return emu.E.HTTP("blob", emu.DevAccount) }

func newSvc(t testing.TB) *service.Client {
	t.Helper()
	s, err := service.NewClientWithSharedKeyCredential(serviceURL(), devCred(t), nil)
	if err != nil {
		t.Fatal(err)
	}
	return s
}

// newContainer creates a container with a unique name.
func newContainer(t testing.TB, prefix string) *container.Client {
	t.Helper()
	s := newSvc(t)
	name := emu.Name(prefix)
	if _, err := s.CreateContainer(context.Background(), name, nil); err != nil {
		t.Fatalf("create container %s: %v", name, err)
	}
	return s.NewContainerClient(name)
}

func randBytes(n int) []byte {
	b := make([]byte, n)
	rand.Read(b)
	return b
}

func readAll(t testing.TB, r io.ReadCloser) []byte {
	t.Helper()
	defer r.Close()
	b, err := io.ReadAll(r)
	if err != nil {
		t.Fatal(err)
	}
	return b
}

func download(t testing.TB, b *blob.Client) []byte {
	t.Helper()
	r, err := b.DownloadStream(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	return readAll(t, r.Body)
}

func mustEqual(t testing.TB, got, want []byte) {
	t.Helper()
	if !bytes.Equal(got, want) {
		t.Fatalf("content differs: got %d bytes, want %d bytes", len(got), len(want))
	}
}

func ptr[T any](v T) *T { return &v }

func asRE(err error, target **azcore.ResponseError) bool { return errors.As(err, target) }

func lower(s string) string { return strings.ToLower(s) }

func emu_unique(p string) string { return emu.Name(p) }

type azblob_svcSetOpts = service.SetPropertiesOptions
type azblob_retention = service.RetentionPolicy

func azcoreETag(s string) azcore.ETag { return azcore.ETag(s) }

// metaGet reads a metadata value ignoring the case of the name: Go's HTTP stack canonicalises header names
// ("Third"), and the service may lower-case them.
func metaGet(m map[string]*string, name string) string {
	for k, v := range m {
		if strings.EqualFold(k, name) && v != nil {
			return *v
		}
	}
	return ""
}

func coreOpts() azcore.ClientOptions { return azcore.ClientOptions{Transport: emu.E.Transport()} }

var blobReadPerm = sas.BlobPermissions{Read: true}

func futureTime() time.Time { return time.Now().Add(time.Hour) }
