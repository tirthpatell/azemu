package faults

import (
	"context"
	"errors"
	"net/http"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/policy"
	"github.com/Azure/azure-sdk-for-go/sdk/data/azcosmos"
)

const cosmosKey = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw=="

func cosmosClient(t testing.TB, account string, retry policy.RetryOptions) *azcosmos.Client {
	t.Helper()
	cred, err := azcosmos.NewKeyCredential(cosmosKey)
	if err != nil {
		t.Fatal(err)
	}
	o := &azcosmos.ClientOptions{ClientOptions: azcore.ClientOptions{Retry: retry, Transport: &http.Client{Transport: emu.E.NewTransport()}}}
	c, err := azcosmos.NewClientWithKey(emu.E.HTTPS("documents", account)+"/", cred, o)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

// anItem makes a database, a container and an item with a client that is not faulted yet. Returns the account, the database
// and the container names.
func anItem(t testing.TB) (account, db, ct string) {
	t.Helper()
	account, db, ct = emu.Name("fa"), emu.Name("db"), emu.Name("ct")
	c := cosmosClient(t, account, policy.RetryOptions{})
	ctx := context.Background()
	if _, err := c.CreateDatabase(ctx, azcosmos.DatabaseProperties{ID: db}, nil); err != nil {
		t.Fatal(err)
	}
	d, _ := c.NewDatabase(db)
	if _, err := d.CreateContainer(ctx, azcosmos.ContainerProperties{ID: ct, PartitionKeyDefinition: azcosmos.PartitionKeyDefinition{Paths: []string{"/pk"}}}, nil); err != nil {
		t.Fatal(err)
	}
	k, _ := d.NewContainer(ct)
	if _, err := k.CreateItem(ctx, azcosmos.NewPartitionKeyString("a"), []byte(`{"id":"1","pk":"a","v":1}`), nil); err != nil {
		t.Fatal(err)
	}
	return account, db, ct
}

func TestCosmosWaitsForTheRetryHeaderAndThenSucceeds(t *testing.T) {
	account, db, ct := anItem(t)
	id := addRule(t, rule{"service": "cosmos", "account": account, "method": "GET", "path": "/dbs/" + db + "/colls/" + ct + "/docs/*", "times": 2,
		"fault": rule{"kind": "throttle", "retryAfterMs": 600}})
	c := cosmosClient(t, account, policy.RetryOptions{MaxRetries: 3, RetryDelay: 10 * time.Millisecond})
	d, _ := c.NewDatabase(db)
	k, _ := d.NewContainer(ct)
	start := time.Now()
	resp, err := k.ReadItem(context.Background(), azcosmos.NewPartitionKeyString("a"), "1", nil)
	took := time.Since(start)
	if err != nil {
		t.Fatalf("read after two throttled answers: %v", err)
	}
	if string(resp.Value) == "" {
		t.Fatal("empty item")
	}
	if n := fired(t, id); n != 2 {
		t.Fatalf("fired %d, want 2", n)
	}
	// azcore reads x-ms-retry-after-ms: two waits of 600 ms
	if took < 1100*time.Millisecond || took > 8*time.Second {
		t.Fatalf("took %v, want about 1.2 s", took)
	}
}

func TestCosmosWithRetriesOffRaisesThe429(t *testing.T) {
	account, db, ct := anItem(t)
	addRule(t, rule{"service": "cosmos", "account": account, "method": "GET", "path": "/dbs/" + db + "/colls/" + ct + "/docs/*",
		"fault": rule{"kind": "throttle", "retryAfterMs": 50}})
	c := cosmosClient(t, account, policy.RetryOptions{MaxRetries: -1})
	d, _ := c.NewDatabase(db)
	k, _ := d.NewContainer(ct)
	_, err := k.ReadItem(context.Background(), azcosmos.NewPartitionKeyString("a"), "1", nil)
	var re *azcore.ResponseError
	if !errors.As(err, &re) {
		t.Fatalf("not a ResponseError: %v", err)
	}
	if re.StatusCode != 429 {
		t.Fatalf("status %d", re.StatusCode)
	}
	if got := re.RawResponse.Header.Get("x-ms-retry-after-ms"); got != "50" {
		t.Fatalf("x-ms-retry-after-ms %q", got)
	}
	if got := re.RawResponse.Header.Get("x-ms-substatus"); got != "3200" {
		t.Fatalf("x-ms-substatus %q", got)
	}
}

func TestCosmosRetriesAfterADroppedConnection(t *testing.T) {
	account, db, ct := anItem(t)
	id := addRule(t, rule{"service": "cosmos", "account": account, "method": "GET", "path": "/dbs/" + db + "/colls/" + ct + "/docs/*", "times": 1, "fault": rule{"kind": "drop"}})
	c := cosmosClient(t, account, policy.RetryOptions{MaxRetries: 3, RetryDelay: 10 * time.Millisecond})
	d, _ := c.NewDatabase(db)
	k, _ := d.NewContainer(ct)
	if _, err := k.ReadItem(context.Background(), azcosmos.NewPartitionKeyString("a"), "1", nil); err != nil {
		t.Fatalf("the dropped connection should be retried: %v", err)
	}
	if n := fired(t, id); n != 1 {
		t.Fatalf("fired %d", n)
	}
}

func TestCosmosLatencyLongerThanTheDeadlineFails(t *testing.T) {
	account, db, ct := anItem(t)
	addRule(t, rule{"service": "cosmos", "account": account, "method": "GET", "path": "/dbs/" + db + "/colls/" + ct + "/docs/*", "times": 1,
		"fault": rule{"kind": "latency", "ms": 3000}})
	c := cosmosClient(t, account, policy.RetryOptions{MaxRetries: -1})
	d, _ := c.NewDatabase(db)
	k, _ := d.NewContainer(ct)
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	start := time.Now()
	_, err := k.ReadItem(ctx, azcosmos.NewPartitionKeyString("a"), "1", nil)
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("expected a deadline error, got %v", err)
	}
	if took := time.Since(start); took > 2500*time.Millisecond {
		t.Fatalf("gave up after %v", took)
	}
}
