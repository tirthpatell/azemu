// One container for every test in this package, and a reset after each test. Start-up is paid once.
package shared

import (
	"context"
	"fmt"
	"os"
	"testing"

	"azemu-testcontainers-go/emulator"

	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
)

var emu *emulator.Emulator

func TestMain(m *testing.M) {
	var err error
	emu, err = emulator.Start(context.Background(), "../seed.json")
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	code := m.Run()
	emu.Terminate() // os.Exit skips deferred calls
	os.Exit(code)
}

// client uses the Azurite connection string; the test starts from the seed and leaves the emulator as it found it.
func client(t *testing.T) *azblob.Client {
	t.Helper()
	t.Cleanup(func() {
		if err := emu.Reset(); err != nil {
			t.Error(err)
		}
	})
	conn := "DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;" +
		"AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;" +
		"BlobEndpoint=" + emu.URL("http", emulator.Blob, "/devstoreaccount1") + ";"
	c, err := azblob.NewClientFromConnectionString(conn, nil)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func TestFirstTestWritesABlob(t *testing.T) {
	c := client(t)
	if _, err := c.UploadBuffer(context.Background(), "uploads", "left-behind.txt", []byte("x"), nil); err != nil {
		t.Fatal(err)
	}
}

// The reset after the first test emptied the container and made the seeded "uploads" again.
func TestSecondTestFindsAnEmptyContainer(t *testing.T) {
	c := client(t)
	pager := c.NewListBlobsFlatPager("uploads", nil)
	page, err := pager.NextPage(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if n := len(page.Segment.BlobItems); n != 0 {
		t.Fatalf("%d blobs left over from the other test", n)
	}
}
