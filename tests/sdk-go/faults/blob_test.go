package faults

import (
	"context"
	"errors"
	"net/http"
	"strings"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/policy"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/bloberror"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/service"
)

func blobClient(t testing.TB, retry policy.RetryOptions, transport *http.Client) *service.Client {
	t.Helper()
	cred, err := azblob.NewSharedKeyCredential(emu.DevAccount, emu.DevKey)
	if err != nil {
		t.Fatal(err)
	}
	opts := &service.ClientOptions{ClientOptions: azcore.ClientOptions{Retry: retry}}
	if transport != nil {
		opts.Transport = transport
	}
	c, err := service.NewClientWithSharedKeyCredential(emu.E.HTTP("blob", emu.DevAccount), cred, opts)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

// aBlob makes a container and a blob with a client that is not faulted yet.
func aBlob(t testing.TB) (container string) {
	t.Helper()
	c := blobClient(t, policy.RetryOptions{}, nil)
	container = emu.Name("fault")
	if _, err := c.CreateContainer(context.Background(), container, nil); err != nil {
		t.Fatal(err)
	}
	cc := c.NewContainerClient(container)
	if _, err := cc.NewBlockBlobClient("b1").UploadStream(context.Background(), strings.NewReader("hello"), nil); err != nil {
		t.Fatal(err)
	}
	return container
}

// freshConnections is a transport that opens a new connection for every request. net/http itself repeats a GET that was sent on
// a kept-alive connection which the server then closed without answering, before azcore sees anything; a new connection each time
// shows what the SDK's retry policy does.
func freshConnections() *http.Client {
	return &http.Client{Transport: &http.Transport{DisableKeepAlives: true}}
}

func TestBlobRetriesAThrottledReadAndWaitsForRetryAfter(t *testing.T) {
	container := aBlob(t)
	id := addRule(t, rule{"service": "blob", "method": "GET", "path": "/" + container + "/*", "times": 2,
		"fault": rule{"kind": "throttle", "retryAfterSeconds": 1}})
	c := blobClient(t, policy.RetryOptions{MaxRetries: 3, RetryDelay: 10 * time.Millisecond}, nil)
	start := time.Now()
	resp, err := c.NewContainerClient(container).NewBlobClient("b1").DownloadStream(context.Background(), nil)
	took := time.Since(start)
	if err != nil {
		t.Fatalf("download after two throttled answers: %v", err)
	}
	body := make([]byte, 5)
	if _, err := resp.Body.Read(body); err != nil && err.Error() != "EOF" {
		t.Fatal(err)
	}
	resp.Body.Close()
	if string(body) != "hello" {
		t.Fatalf("body %q", body)
	}
	if n := fired(t, id); n != 2 {
		t.Fatalf("rule fired %d times, want 2", n)
	}
	// azcore reads Retry-After: two waits of one second
	if took < 1900*time.Millisecond || took > 10*time.Second {
		t.Fatalf("took %v, want about 2s (two Retry-After waits of 1 s)", took)
	}
}

func TestBlobWithRetriesOffRaisesServerBusy(t *testing.T) {
	container := aBlob(t)
	addRule(t, rule{"service": "blob", "method": "GET", "path": "/" + container + "/*",
		"fault": rule{"kind": "throttle", "retryAfterSeconds": 3}})
	c := blobClient(t, policy.RetryOptions{MaxRetries: -1}, nil)
	_, err := c.NewContainerClient(container).NewBlobClient("b1").DownloadStream(context.Background(), nil)
	var re *azcore.ResponseError
	if !errors.As(err, &re) {
		t.Fatalf("not a ResponseError: %v", err)
	}
	if re.StatusCode != 503 || re.ErrorCode != string(bloberror.ServerBusy) {
		t.Fatalf("status %d code %q", re.StatusCode, re.ErrorCode)
	}
	if got := re.RawResponse.Header.Get("Retry-After"); got != "3" {
		t.Fatalf("Retry-After %q", got)
	}
	if got := re.RawResponse.Header.Get("x-azemu-fault"); got != "throttle" {
		t.Fatalf("x-azemu-fault %q", got)
	}
	if !bloberror.HasCode(err, bloberror.ServerBusy) {
		t.Fatal("bloberror.HasCode does not see ServerBusy")
	}
}

func TestBlobErrorRuleGivesTheCodeItSpellsOut(t *testing.T) {
	container := aBlob(t)
	addRule(t, rule{"service": "blob", "method": "DELETE", "path": "/" + container + "/*",
		"fault": rule{"kind": "error", "status": 500, "code": "InternalError", "message": "on purpose"}})
	c := blobClient(t, policy.RetryOptions{MaxRetries: -1}, nil)
	_, err := c.NewContainerClient(container).NewBlobClient("b1").Delete(context.Background(), nil)
	var re *azcore.ResponseError
	if !errors.As(err, &re) || re.StatusCode != 500 || re.ErrorCode != "InternalError" || !strings.Contains(err.Error(), "on purpose") {
		t.Fatalf("got %v", err)
	}
}

func TestBlobRetriesAfterADroppedConnection(t *testing.T) {
	container := aBlob(t)
	id := addRule(t, rule{"service": "blob", "method": "GET", "path": "/" + container + "/*", "times": 1, "fault": rule{"kind": "drop"}})
	c := blobClient(t, policy.RetryOptions{MaxRetries: 3, RetryDelay: 10 * time.Millisecond}, freshConnections())
	resp, err := c.NewContainerClient(container).NewBlobClient("b1").DownloadStream(context.Background(), nil)
	if err != nil {
		t.Fatalf("the dropped connection should be retried: %v", err)
	}
	resp.Body.Close()
	if n := fired(t, id); n != 1 {
		t.Fatalf("fired %d", n)
	}
}

func TestBlobWithRetriesOffSeesADroppedConnectionAsATransportError(t *testing.T) {
	container := aBlob(t)
	addRule(t, rule{"service": "blob", "method": "GET", "path": "/" + container + "/*", "times": 1, "fault": rule{"kind": "drop"}})
	c := blobClient(t, policy.RetryOptions{MaxRetries: -1}, freshConnections())
	_, err := c.NewContainerClient(container).NewBlobClient("b1").DownloadStream(context.Background(), nil)
	if err == nil {
		t.Fatal("expected an error")
	}
	var re *azcore.ResponseError
	if errors.As(err, &re) {
		t.Fatalf("a dropped connection has no response, got %v", re)
	}
}

func TestBlobLatencyLongerThanTheContextDeadlineFails(t *testing.T) {
	container := aBlob(t)
	addRule(t, rule{"service": "blob", "method": "GET", "path": "/" + container + "/*", "times": 1, "fault": rule{"kind": "latency", "ms": 3000}})
	c := blobClient(t, policy.RetryOptions{MaxRetries: -1}, nil)
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	start := time.Now()
	_, err := c.NewContainerClient(container).NewBlobClient("b1").DownloadStream(ctx, nil)
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("expected a deadline error, got %v", err)
	}
	if took := time.Since(start); took > 2500*time.Millisecond {
		t.Fatalf("gave up after %v, not at the deadline", took)
	}
	// with time to wait the call works, late
	addRule(t, rule{"service": "blob", "method": "GET", "path": "/" + container + "/*", "times": 1, "fault": rule{"kind": "latency", "ms": 600}})
	start = time.Now()
	resp, err := c.NewContainerClient(container).NewBlobClient("b1").DownloadStream(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if took := time.Since(start); took < 550*time.Millisecond {
		t.Fatalf("took %v, want the 600 ms of the rule", took)
	}
}
