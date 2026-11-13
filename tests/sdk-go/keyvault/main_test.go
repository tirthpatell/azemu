package keyvault

import (
	"context"
	"errors"
	"io"
	"net/http"
	"strings"
	"testing"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/security/keyvault/azcertificates"
	"github.com/Azure/azure-sdk-for-go/sdk/security/keyvault/azkeys"
	"github.com/Azure/azure-sdk-for-go/sdk/security/keyvault/azsecrets"
)

func TestMain(m *testing.M) { emu.Main(m) }

// newVault returns the vault URL for a fresh vault name.
func newVault() string { return emu.E.HTTPS("vault", emu.Name("kv")) }

func coreOpts() azcore.ClientOptions {
	return azcore.ClientOptions{Transport: emu.E.Transport()}
}

func secretsClient(t *testing.T, url string) *azsecrets.Client {
	t.Helper()
	c, err := azsecrets.NewClient(url, emu.Cred(), &azsecrets.ClientOptions{ClientOptions: coreOpts()})
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func keysClient(t *testing.T, url string) *azkeys.Client {
	t.Helper()
	c, err := azkeys.NewClient(url, emu.Cred(), &azkeys.ClientOptions{ClientOptions: coreOpts()})
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func certsClient(t *testing.T, url string) *azcertificates.Client {
	t.Helper()
	c, err := azcertificates.NewClient(url, emu.Cred(), &azcertificates.ClientOptions{ClientOptions: coreOpts()})
	if err != nil {
		t.Fatal(err)
	}
	return c
}

// respErr asserts err is an azcore.ResponseError with the given status and error code.
func respErr(t *testing.T, err error, status int, code string) {
	t.Helper()
	var re *azcore.ResponseError
	if !errors.As(err, &re) {
		t.Fatalf("want ResponseError %d %s, got %v", status, code, err)
	}
	if re.StatusCode != status {
		t.Fatalf("status = %d, want %d (%v)", re.StatusCode, status, err)
	}
	if code != "" && re.ErrorCode != code {
		t.Fatalf("error code = %q, want %q", re.ErrorCode, code)
	}
}

// respErrInner is respErr plus a check that the error body names the inner error code
// (Key Vault puts "SecretDisabled" and friends in error.innererror.code).
func respErrInner(t *testing.T, err error, status int, code, inner string) {
	t.Helper()
	respErr(t, err, status, code)
	var re *azcore.ResponseError
	errors.As(err, &re)
	b, _ := io.ReadAll(re.RawResponse.Body)
	if !strings.Contains(string(b), `"`+inner+`"`) {
		t.Fatalf("error body lacks inner code %s: %s", inner, b)
	}
}

var _ = http.StatusOK

func ptr[T any](v T) *T { return &v }

// The challenge names the domain of the request (vault.localhost:<port>), so the clients' own check passes with no switch.
// The three helpers above use the defaults; this test also shows that the old switch still works.
func TestChallengeResource_NoSwitchNeededAndTheOldSwitchStillWorks(t *testing.T) {
	ctx := context.Background()
	url := newVault()
	sc := secretsClient(t, url)
	if _, err := sc.SetSecret(ctx, "k", azsecrets.SetSecretParameters{Value: ptr("v")}, nil); err != nil {
		t.Fatalf("a client with the default check: %v", err)
	}
	old, err := azsecrets.NewClient(newVault(), emu.Cred(), &azsecrets.ClientOptions{ClientOptions: coreOpts(), DisableChallengeResourceVerification: true})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := old.SetSecret(ctx, "k", azsecrets.SetSecretParameters{Value: ptr("v")}, nil); err != nil {
		t.Fatalf("a client with the switch: %v", err)
	}
	kc := keysClient(t, newVault())
	if _, err := kc.CreateKey(ctx, "ek", azkeys.CreateKeyParameters{Kty: ptr(azkeys.KeyTypeEC), Curve: ptr(azkeys.CurveNameP256)}, nil); err != nil {
		t.Fatalf("keys client: %v", err)
	}
	cc := certsClient(t, newVault())
	if p := cc.NewListCertificatePropertiesPager(nil); p.More() {
		if _, err := p.NextPage(ctx); err != nil {
			t.Fatalf("certificates client: %v", err)
		}
	}
}
