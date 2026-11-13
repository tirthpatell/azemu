package blob

import (
	"bytes"
	"context"
	"net/http"
	"testing"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/blockblob"
)

func TestHttps_ParallelUploadStreamAndDownloadBufferWithTheCaTrustedByTheClient(t *testing.T) {
	ctx := context.Background()
	hc, pl := emu.E.Client()
	cl, err := azblob.NewClientWithSharedKeyCredential(emu.E.HTTPS("blob", emu.DevAccount), devCred(t), &azblob.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: hc}})
	if err != nil {
		t.Fatal(err)
	}
	cont := emu.Name("tls")
	if _, err := cl.CreateContainer(ctx, cont, nil); err != nil {
		t.Fatal(err)
	}
	data := randBytes(6<<20 + 11)
	if _, err := cl.UploadStream(ctx, cont, "big", plainReader{bytes.NewReader(data)}, &blockblob.UploadStreamOptions{BlockSize: 1 << 20, Concurrency: 4}); err != nil {
		t.Fatalf("upload over https: %v", err)
	}
	buf := make([]byte, len(data))
	n, err := cl.DownloadBuffer(ctx, cont, "big", buf, &azblob.DownloadBufferOptions{BlockSize: 1 << 20, Concurrency: 4})
	if err != nil || n != int64(len(data)) {
		t.Fatalf("download: %d %v", n, err)
	}
	mustEqual(t, buf, data)
	t.Logf("protocols used: %v", pl.Counts)
}

func TestHttps_ServiceSasUsedOverHttpsByAnAnonymousClient(t *testing.T) {
	ctx := context.Background()
	hc, _ := emu.E.Client()
	opts := &azblob.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: hc}}
	cl, _ := azblob.NewClientWithSharedKeyCredential(emu.E.HTTPS("blob", emu.DevAccount), devCred(t), opts)
	cont := emu.Name("tlssas")
	cl.CreateContainer(ctx, cont, nil)
	cl.UploadBuffer(ctx, cont, "a", []byte("sas over tls"), nil)
	u, err := cl.ServiceClient().NewContainerClient(cont).NewBlobClient("a").GetSASURL(blobReadPerm, futureTime(), nil)
	if err != nil {
		t.Fatal(err)
	}
	anon, err := azblob.NewClientWithNoCredential(emu.E.HTTPS("blob", emu.DevAccount), opts)
	if err != nil {
		t.Fatal(err)
	}
	_ = anon
	resp, err := hc.Get(u)
	if err != nil || resp.StatusCode != http.StatusOK {
		t.Fatalf("GET with SAS: %v %v", err, resp)
	}
	resp.Body.Close()
}
