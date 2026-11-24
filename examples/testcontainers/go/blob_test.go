package example

import (
	"bytes"
	"context"
	"io"
	"testing"

	"azemu-testcontainers-go/emulator"

	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
)

// The Azurite connection string, with the published port. Plain http needs no certificate.
func TestBlobOverHTTP(t *testing.T) {
	e := startEmulator(t)
	conn := "DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;" +
		"AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;" +
		"BlobEndpoint=" + e.URL("http", emulator.Blob, "/devstoreaccount1") + ";"
	client, err := azblob.NewClientFromConnectionString(conn, nil)
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()

	// "uploads" comes from seed.json; nothing here creates it.
	if _, err := client.UploadBuffer(ctx, "uploads", "hello.txt", []byte("hello"), nil); err != nil {
		t.Fatal(err)
	}
	res, err := client.DownloadStream(ctx, "uploads", "hello.txt", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer res.Body.Close()
	got, _ := io.ReadAll(res.Body)
	if !bytes.Equal(got, []byte("hello")) {
		t.Fatalf("got %q", got)
	}
}
