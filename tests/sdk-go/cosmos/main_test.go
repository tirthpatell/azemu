package cosmos

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"testing"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/data/azcosmos"
)

func TestMain(m *testing.M) { emu.Main(m) }

const cosmosKey = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw=="

func endpoint() string { return emu.E.HTTPS("documents", emu.Name("cs")) + "/" }

func newClient(t testing.TB, opts ...func(*azcosmos.ClientOptions)) *azcosmos.Client {
	t.Helper()
	cred, err := azcosmos.NewKeyCredential(cosmosKey)
	if err != nil {
		t.Fatal(err)
	}
	o := &azcosmos.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: &http.Client{Transport: emu.Dump(emu.E.NewTransport())}}}
	for _, f := range opts {
		f(o)
	}
	c, err := azcosmos.NewClientWithKey(endpoint(), cred, o)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func withContent(o *azcosmos.ClientOptions) { o.EnableContentResponseOnWrite = true }

func newDB(t testing.TB, c *azcosmos.Client) *azcosmos.DatabaseClient {
	t.Helper()
	id := emu.Name("db")
	if _, err := c.CreateDatabase(context.Background(), azcosmos.DatabaseProperties{ID: id}, nil); err != nil {
		t.Fatalf("create database: %v", err)
	}
	db, err := c.NewDatabase(id)
	if err != nil {
		t.Fatal(err)
	}
	return db
}

// newContainer creates a database and a container partitioned by /pk.
func newContainer(t testing.TB, c *azcosmos.Client) *azcosmos.ContainerClient {
	t.Helper()
	return newContainerWith(t, c, azcosmos.ContainerProperties{PartitionKeyDefinition: azcosmos.PartitionKeyDefinition{Paths: []string{"/pk"}}}, nil)
}

func newContainerWith(t testing.TB, c *azcosmos.Client, p azcosmos.ContainerProperties, th *azcosmos.ThroughputProperties) *azcosmos.ContainerClient {
	t.Helper()
	db := newDB(t, c)
	p.ID = emu.Name("ct")
	var o *azcosmos.CreateContainerOptions
	if th != nil {
		o = &azcosmos.CreateContainerOptions{ThroughputProperties: th}
	}
	if _, err := db.CreateContainer(context.Background(), p, o); err != nil {
		t.Fatalf("create container: %v", err)
	}
	ct, err := db.NewContainer(p.ID)
	if err != nil {
		t.Fatal(err)
	}
	return ct
}

func status(t testing.TB, err error, want int) {
	t.Helper()
	if err == nil {
		t.Fatalf("expected status %d, got success", want)
	}
	var re *azcore.ResponseError
	if !errors.As(err, &re) {
		t.Fatalf("not a ResponseError: %v", err)
	}
	if re.StatusCode != want {
		t.Fatalf("status %d, want %d: %v", re.StatusCode, want, err)
	}
}

func doc(m map[string]any) []byte {
	b, err := json.Marshal(m)
	if err != nil {
		panic(err)
	}
	return b
}

func parse(t testing.TB, b []byte) map[string]any {
	t.Helper()
	var m map[string]any
	if err := json.Unmarshal(b, &m); err != nil {
		t.Fatalf("%v: %s", err, b)
	}
	return m
}

func queryAll(t testing.TB, ct *azcosmos.ContainerClient, q string, pk azcosmos.PartitionKey, o *azcosmos.QueryOptions) ([]map[string]any, int) {
	t.Helper()
	var out []map[string]any
	pages := 0
	for p := ct.NewQueryItemsPager(q, pk, o); p.More(); {
		pg, err := p.NextPage(context.Background())
		if err != nil {
			t.Fatalf("query %q: %v", q, err)
		}
		pages++
		for _, it := range pg.Items {
			out = append(out, parse(t, it))
		}
	}
	return out, pages
}

func ptr[T any](v T) *T { return &v }
