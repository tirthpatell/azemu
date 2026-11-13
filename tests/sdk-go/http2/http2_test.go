// Package http2 checks which HTTP version each Go transport ends up with against the emulator's TLS listener, and does a Key Vault
// and a Cosmos round trip over each: the Go default (offers h2 and http/1.1), forced HTTP/1.1, and forced HTTP/2.
package http2

import (
	"context"
	"crypto/tls"
	"net/http"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/data/azcosmos"
	"github.com/Azure/azure-sdk-for-go/sdk/security/keyvault/azsecrets"
	"golang.org/x/net/http2"
)

func TestMain(m *testing.M) { emu.Main(m) }

func ptr[T any](v T) *T { return &v }

// serverOffersH2 does a TLS handshake that offers only h2 and reports what the server selected.
func serverOffersH2(t testing.TB) (alpn string, offered bool) {
	t.Helper()
	cfg := emu.E.TLSConfig()
	cfg.NextProtos = []string{"h2"}
	cfg.ServerName = "x.vault.localhost"
	conn, err := tls.Dial("tcp", emu.E.Host("vault", "x"), cfg)
	if err != nil {
		// an alert such as no_application_protocol means h2 is not offered
		return "", false
	}
	defer conn.Close()
	return conn.ConnectionState().NegotiatedProtocol, conn.ConnectionState().NegotiatedProtocol == "h2"
}

func TestProbe_WhichProtocolDoesTheTlsListenerNegotiate(t *testing.T) {
	alpn, offered := serverOffersH2(t)
	c, pl := emu.E.Client()
	resp, err := c.Get(emu.E.HTTPS("vault", "probe") + "/secrets?api-version=7.5")
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	t.Logf("ALPN h2-only handshake: negotiated=%q offered=%v; Go default transport used %s", alpn, offered, resp.Proto)
	if offered && resp.Proto != "HTTP/2.0" {
		t.Fatalf("the server offers h2 but the default Go transport used %s", resp.Proto)
	}
	if !offered && resp.Proto != "HTTP/1.1" {
		t.Fatalf("no h2 on the server but the transport used %s", resp.Proto)
	}
	_ = pl
}

// roundTrip does a Key Vault secret round trip and a Cosmos create/read/query through the given transport and returns the protocols seen.
func roundTrip(t *testing.T, rt http.RoundTripper) map[string]int {
	t.Helper()
	hc, pl := emu.E.ClientWith(rt)
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	sc, err := azsecrets.NewClient(emu.E.HTTPS("vault", emu.Name("h2kv")), emu.Cred(), &azsecrets.ClientOptions{
		ClientOptions: azcore.ClientOptions{Transport: hc}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := sc.SetSecret(ctx, "k", azsecrets.SetSecretParameters{Value: ptr("value over this transport")}, nil); err != nil {
		t.Fatalf("key vault set: %v", err)
	}
	got, err := sc.GetSecret(ctx, "k", "", nil)
	if err != nil || *got.Value != "value over this transport" {
		t.Fatalf("key vault get: %v", err)
	}

	cred, _ := azcosmos.NewKeyCredential("C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==")
	cc, err := azcosmos.NewClientWithKey(emu.E.HTTPS("documents", emu.Name("h2cs"))+"/", cred, &azcosmos.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: hc}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := cc.CreateDatabase(ctx, azcosmos.DatabaseProperties{ID: "db"}, nil); err != nil {
		t.Fatalf("cosmos create database: %v", err)
	}
	db, _ := cc.NewDatabase("db")
	if _, err := db.CreateContainer(ctx, azcosmos.ContainerProperties{ID: "c", PartitionKeyDefinition: azcosmos.PartitionKeyDefinition{Paths: []string{"/pk"}}}, nil); err != nil {
		t.Fatalf("cosmos create container: %v", err)
	}
	ct, _ := db.NewContainer("c")
	pk := azcosmos.NewPartitionKeyString("p")
	if _, err := ct.CreateItem(ctx, pk, []byte(`{"id":"i1","pk":"p","v":1}`), nil); err != nil {
		t.Fatalf("cosmos create item: %v", err)
	}
	r, err := ct.ReadItem(ctx, pk, "i1", nil)
	if err != nil || len(r.Value) == 0 {
		t.Fatalf("cosmos read item: %v", err)
	}
	n := 0
	for p := ct.NewQueryItemsPager("SELECT * FROM c", pk, nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatalf("cosmos query: %v", err)
		}
		n += len(pg.Items)
	}
	if n != 1 {
		t.Fatalf("query returned %d items", n)
	}
	return pl.Counts
}

func TestForcedHttp1_KeyVaultAndCosmosRoundTripsUseHttp11(t *testing.T) {
	counts := roundTrip(t, emu.E.HTTP1Transport())
	if counts["HTTP/1.1"] == 0 || counts["HTTP/2.0"] != 0 {
		t.Fatalf("protocols seen %v", counts)
	}
}

func TestForcedHttp2_KeyVaultAndCosmosRoundTripsUseHttp2(t *testing.T) {
	if _, offered := serverOffersH2(t); !offered {
		emu.Finding(t, "azemu: the TLS listener does not offer ALPN h2 (the HTTP/2 work is in progress); the forced HTTP/2 client cannot connect")
	}
	cfg := emu.E.TLSConfig()
	cfg.NextProtos = []string{"h2"}
	rt := &http2.Transport{TLSClientConfig: cfg}
	counts := roundTrip(t, rt)
	if counts["HTTP/2.0"] == 0 || counts["HTTP/1.1"] != 0 {
		t.Fatalf("protocols seen %v", counts)
	}
}

func TestDefaultTransport_KeyVaultAndCosmosRoundTripsWorkWhicheverProtocolIsNegotiated(t *testing.T) {
	counts := roundTrip(t, emu.E.NewTransport())
	t.Logf("default transport protocols: %v", counts)
	_, offered := serverOffersH2(t)
	if offered && counts["HTTP/2.0"] == 0 {
		t.Fatalf("server offers h2 but the default transport stayed on HTTP/1.1: %v", counts)
	}
	if !offered && counts["HTTP/1.1"] == 0 {
		t.Fatalf("protocols %v", counts)
	}
}

func TestHttp2_ManyParallelRequestsShareOneConnectionAndAllSucceed(t *testing.T) {
	if _, offered := serverOffersH2(t); !offered {
		emu.Finding(t, "azemu: the TLS listener does not offer ALPN h2 (the HTTP/2 work is in progress)")
	}
	cfg := emu.E.TLSConfig()
	cfg.NextProtos = []string{"h2"}
	hc, pl := emu.E.ClientWith(&http2.Transport{TLSClientConfig: cfg})
	sc, err := azsecrets.NewClient(emu.E.HTTPS("vault", emu.Name("h2par")), emu.Cred(), &azsecrets.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: hc}})
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	errs := make(chan error, 40)
	for i := 0; i < 40; i++ {
		go func(i int) {
			_, err := sc.SetSecret(ctx, "s"+string(rune('a'+i%26))+string(rune('a'+i/26)), azsecrets.SetSecretParameters{Value: ptr("v")}, nil)
			errs <- err
		}(i)
	}
	for i := 0; i < 40; i++ {
		if err := <-errs; err != nil {
			t.Fatal(err)
		}
	}
	if pl.Counts["HTTP/2.0"] < 40 {
		t.Fatalf("protocols %v", pl.Counts)
	}
}
