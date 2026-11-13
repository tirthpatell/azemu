package identity

import (
	"context"
	"crypto"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"math/big"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/cloud"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/policy"
	"github.com/Azure/azure-sdk-for-go/sdk/azidentity"
	"github.com/Azure/azure-sdk-for-go/sdk/security/keyvault/azsecrets"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
)

func TestMain(m *testing.M) { emu.Main(m) }

const (
	tenant = "11111111-2222-3333-4444-555555555555"
	client = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
)

// localOnly refuses every host that is not the emulator, so a test never reaches the internet.
// It also counts requests per path.
type localOnly struct {
	inner http.RoundTripper
	reqs  atomic.Int64
}

func (l *localOnly) RoundTrip(r *http.Request) (*http.Response, error) {
	h := r.URL.Hostname()
	if h != "localhost" && !strings.HasSuffix(h, ".localhost") && !strings.HasPrefix(h, "127.") {
		return nil, fmt.Errorf("test transport refuses non-local host %s", h)
	}
	l.reqs.Add(1)
	return l.inner.RoundTrip(r)
}

func newTransport() (*http.Client, *localOnly) {
	l := &localOnly{inner: emu.Dump(emu.E.NewTransport())}
	return &http.Client{Transport: l}, l
}

// claims decodes the payload of a JWT without verifying it.
func claims(t *testing.T, jwt string) map[string]any {
	t.Helper()
	parts := strings.Split(jwt, ".")
	if len(parts) != 3 {
		t.Fatalf("not a JWT: %q", jwt)
	}
	b, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		t.Fatal(err)
	}
	var m map[string]any
	if err := json.Unmarshal(b, &m); err != nil {
		t.Fatal(err)
	}
	return m
}

func setIMDSEnv(t *testing.T) {
	t.Setenv("IDENTITY_ENDPOINT", emu.E.Gateway()+"/_azemu/identity/msi/token")
	t.Setenv("IDENTITY_HEADER", "azemu")
	// no other credential may be configured through the environment
	for _, k := range []string{"AZURE_CLIENT_ID", "AZURE_TENANT_ID", "AZURE_CLIENT_SECRET", "AZURE_FEDERATED_TOKEN_FILE", "AZURE_AUTHORITY_HOST", "AZURE_USERNAME", "AZURE_TOKEN_CREDENTIALS"} {
		t.Setenv(k, "") // registers the restore; then really unset (azidentity tells "set but empty" from "not set")
		os.Unsetenv(k)
	}
}

func authority() string { return fmt.Sprintf("https://login.localhost:%d", emu.E.Port) }

func secretOpts(c *http.Client) azcore.ClientOptions {
	return azcore.ClientOptions{Transport: c, Cloud: cloud.Configuration{ActiveDirectoryAuthorityHost: authority()}}
}

func TestManagedIdentity_AppServiceEndpointReturnsAJWTForTheRequestedScope(t *testing.T) {
	setIMDSEnv(t)
	hc, _ := newTransport()
	cred, err := azidentity.NewManagedIdentityCredential(&azidentity.ManagedIdentityCredentialOptions{ClientOptions: azcore.ClientOptions{Transport: hc}})
	if err != nil {
		t.Fatal(err)
	}
	tok, err := cred.GetToken(context.Background(), policy.TokenRequestOptions{Scopes: []string{"https://storage.azure.com/.default"}})
	if err != nil {
		t.Fatal(err)
	}
	c := claims(t, tok.Token)
	if c["aud"] != "https://storage.azure.com" {
		t.Fatalf("aud = %v", c["aud"])
	}
	if time.Until(tok.ExpiresOn) < 30*time.Minute {
		t.Fatalf("token expires in %v", time.Until(tok.ExpiresOn))
	}
}

func TestManagedIdentity_UserAssignedClientIDIsTheTokensAppID(t *testing.T) {
	setIMDSEnv(t)
	hc, _ := newTransport()
	id := "99999999-8888-7777-6666-555555555555"
	cred, err := azidentity.NewManagedIdentityCredential(&azidentity.ManagedIdentityCredentialOptions{
		ClientOptions: azcore.ClientOptions{Transport: hc}, ID: azidentity.ClientID(id)})
	if err != nil {
		t.Fatal(err)
	}
	tok, err := cred.GetToken(context.Background(), policy.TokenRequestOptions{Scopes: []string{"https://vault.azure.net/.default"}})
	if err != nil {
		t.Fatal(err)
	}
	if c := claims(t, tok.Token); c["appid"] != id {
		t.Fatalf("appid = %v, want %s", c["appid"], id)
	}
}

func TestManagedIdentity_SecondGetTokenComesFromTheCacheNotTheEndpoint(t *testing.T) {
	setIMDSEnv(t)
	hc, counter := newTransport()
	cred, _ := azidentity.NewManagedIdentityCredential(&azidentity.ManagedIdentityCredentialOptions{ClientOptions: azcore.ClientOptions{Transport: hc}})
	opts := policy.TokenRequestOptions{Scopes: []string{"https://storage.azure.com/.default"}}
	if _, err := cred.GetToken(context.Background(), opts); err != nil {
		t.Fatal(err)
	}
	before := counter.reqs.Load()
	if _, err := cred.GetToken(context.Background(), opts); err != nil {
		t.Fatal(err)
	}
	if counter.reqs.Load() != before {
		t.Fatalf("second call sent %d requests", counter.reqs.Load()-before)
	}
}

func TestDefaultAzureCredential_UsesManagedIdentityForBlobAndKeyVaultCalls(t *testing.T) {
	setIMDSEnv(t)
	hc, _ := newTransport()
	cred, err := azidentity.NewDefaultAzureCredential(&azidentity.DefaultAzureCredentialOptions{ClientOptions: azcore.ClientOptions{Transport: hc}})
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	bc, err := azblob.NewClient(emu.E.HTTPS("blob", emu.Name("idacct")), cred, &azblob.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: hc}})
	if err != nil {
		t.Fatal(err)
	}
	cont := emu.Name("idc")
	if _, err := bc.CreateContainer(ctx, cont, nil); err != nil {
		t.Fatalf("blob create container with a bearer token: %v", err)
	}
	if _, err := bc.UploadBuffer(ctx, cont, "b", []byte("via-token"), nil); err != nil {
		t.Fatal(err)
	}
	r, err := bc.DownloadStream(ctx, cont, "b", nil)
	if err != nil {
		t.Fatal(err)
	}
	b, _ := io.ReadAll(r.Body)
	if string(b) != "via-token" {
		t.Fatalf("body %q", b)
	}

	sc, err := azsecrets.NewClient(emu.E.HTTPS("vault", emu.Name("idkv")), cred, &azsecrets.ClientOptions{
		ClientOptions: azcore.ClientOptions{Transport: hc}})
	if err != nil {
		t.Fatal(err)
	}
	v := "s3cret"
	if _, err := sc.SetSecret(ctx, "k", azsecrets.SetSecretParameters{Value: &v}, nil); err != nil {
		t.Fatalf("key vault set secret: %v", err)
	}
	got, err := sc.GetSecret(ctx, "k", "", nil)
	if err != nil || *got.Value != v {
		t.Fatalf("get: %v %v", got.Value, err)
	}
}

func TestClientSecretCredential_GetsATokenFromTheEmulatorLoginHost(t *testing.T) {
	hc, _ := newTransport()
	cred, err := azidentity.NewClientSecretCredential(tenant, client, "any-secret", &azidentity.ClientSecretCredentialOptions{
		ClientOptions: secretOpts(hc), DisableInstanceDiscovery: true})
	if err != nil {
		t.Fatal(err)
	}
	tok, err := cred.GetToken(context.Background(), policy.TokenRequestOptions{Scopes: []string{"https://vault.azure.net/.default"}})
	if err != nil {
		t.Fatal(err)
	}
	c := claims(t, tok.Token)
	if c["appid"] != client || c["tid"] != tenant || c["aud"] != "https://vault.azure.net" {
		t.Fatalf("claims %v", c)
	}
}

func TestClientSecretCredential_TokenWorksForBlobAndKeyVault(t *testing.T) {
	hc, _ := newTransport()
	cred, _ := azidentity.NewClientSecretCredential(tenant, client, "x", &azidentity.ClientSecretCredentialOptions{
		ClientOptions: secretOpts(hc), DisableInstanceDiscovery: true,
		// the vault's challenge names a tenant the emulator makes up; the credential refuses other tenants unless told
		AdditionallyAllowedTenants: []string{"*"}})
	ctx := context.Background()
	bc, _ := azblob.NewClient(emu.E.HTTPS("blob", emu.Name("csacct")), cred, &azblob.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: hc}})
	cont := emu.Name("csc")
	if _, err := bc.CreateContainer(ctx, cont, nil); err != nil {
		t.Fatal(err)
	}
	sc, _ := azsecrets.NewClient(emu.E.HTTPS("vault", emu.Name("cskv")), cred, &azsecrets.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: hc}})
	v := "a"
	if _, err := sc.SetSecret(ctx, "k", azsecrets.SetSecretParameters{Value: &v}, nil); err != nil {
		t.Fatal(err)
	}
}

func TestClientSecretCredential_WithInstanceDiscoveryOnAsksMicrosoftNotTheEmulator(t *testing.T) {
	// MSAL validates an authority it does not know against login.microsoftonline.com, never against the
	// emulator. The test transport refuses that host, so the credential fails; with
	// DisableInstanceDiscovery the same setup works (previous tests).
	hc, _ := newTransport()
	o := secretOpts(hc)
	o.Retry.MaxRetries = -1
	cred, _ := azidentity.NewClientSecretCredential(tenant, client, "x", &azidentity.ClientSecretCredentialOptions{ClientOptions: o})
	_, err := cred.GetToken(context.Background(), policy.TokenRequestOptions{Scopes: []string{"https://storage.azure.com/.default"}})
	if err == nil {
		t.Fatal("expected an error: instance discovery cannot reach the emulator")
	}
	if !strings.Contains(err.Error(), "login.microsoftonline.com") {
		t.Fatalf("error does not mention the Microsoft host: %v", err)
	}
}

func TestClientSecretCredential_TokenIsCachedBetweenCalls(t *testing.T) {
	hc, counter := newTransport()
	cred, _ := azidentity.NewClientSecretCredential(tenant, client, "x", &azidentity.ClientSecretCredentialOptions{ClientOptions: secretOpts(hc), DisableInstanceDiscovery: true})
	o := policy.TokenRequestOptions{Scopes: []string{"https://storage.azure.com/.default"}}
	if _, err := cred.GetToken(context.Background(), o); err != nil {
		t.Fatal(err)
	}
	n := counter.reqs.Load()
	if _, err := cred.GetToken(context.Background(), o); err != nil {
		t.Fatal(err)
	}
	if counter.reqs.Load() != n {
		t.Fatal("second GetToken made requests")
	}
}

func TestClientSecretCredential_DifferentScopesGiveDifferentAudiences(t *testing.T) {
	hc, _ := newTransport()
	cred, _ := azidentity.NewClientSecretCredential(tenant, client, "x", &azidentity.ClientSecretCredentialOptions{ClientOptions: secretOpts(hc), DisableInstanceDiscovery: true})
	a, err := cred.GetToken(context.Background(), policy.TokenRequestOptions{Scopes: []string{"https://management.azure.com/.default"}})
	if err != nil {
		t.Fatal(err)
	}
	b, _ := cred.GetToken(context.Background(), policy.TokenRequestOptions{Scopes: []string{"https://servicebus.azure.net/.default"}})
	if claims(t, a.Token)["aud"] == claims(t, b.Token)["aud"] {
		t.Fatal("audiences equal")
	}
}

func TestClientSecretCredential_TokenSignatureVerifiesWithTheEmulatorsKeySet(t *testing.T) {
	hc, _ := newTransport()
	cred, _ := azidentity.NewClientSecretCredential(tenant, client, "x", &azidentity.ClientSecretCredentialOptions{ClientOptions: secretOpts(hc), DisableInstanceDiscovery: true})
	tok, err := cred.GetToken(context.Background(), policy.TokenRequestOptions{Scopes: []string{"https://storage.azure.com/.default"}})
	if err != nil {
		t.Fatal(err)
	}
	parts := strings.Split(tok.Token, ".")
	var hdr struct{ Kid, Alg string }
	hb, _ := base64.RawURLEncoding.DecodeString(parts[0])
	json.Unmarshal(hb, &hdr)
	if hdr.Alg != "RS256" {
		t.Fatalf("alg %s", hdr.Alg)
	}
	resp, err := hc.Get(fmt.Sprintf("https://login.localhost:%d/%s/discovery/v2.0/keys", emu.E.Port, tenant))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	var jwks struct {
		Keys []struct{ Kid, N, E string }
	}
	if err := json.NewDecoder(resp.Body).Decode(&jwks); err != nil {
		t.Fatal(err)
	}
	for _, k := range jwks.Keys {
		if k.Kid != hdr.Kid {
			continue
		}
		nb, _ := base64.RawURLEncoding.DecodeString(k.N)
		eb, _ := base64.RawURLEncoding.DecodeString(k.E)
		pub := &rsa.PublicKey{N: new(big.Int).SetBytes(nb), E: int(new(big.Int).SetBytes(eb).Int64())}
		sig, _ := base64.RawURLEncoding.DecodeString(parts[2])
		h := sha256.Sum256([]byte(parts[0] + "." + parts[1]))
		if err := rsa.VerifyPKCS1v15(pub, crypto.SHA256, h[:], sig); err != nil {
			t.Fatalf("signature: %v", err)
		}
		return
	}
	t.Fatalf("kid %s not in key set", hdr.Kid)
}

func TestClientCertificateCredential_AssertionSignedWithACertificateIsAccepted(t *testing.T) {
	key, _ := rsa.GenerateKey(rand.Reader, 2048)
	tmpl := &x509.Certificate{SerialNumber: big.NewInt(7), Subject: pkix.Name{CommonName: "cc"}, NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(time.Hour)}
	der, _ := x509.CreateCertificate(rand.Reader, tmpl, tmpl, &key.PublicKey, key)
	cert, _ := x509.ParseCertificate(der)
	hc, _ := newTransport()
	cred, err := azidentity.NewClientCertificateCredential(tenant, client, []*x509.Certificate{cert}, key, &azidentity.ClientCertificateCredentialOptions{
		ClientOptions: secretOpts(hc), DisableInstanceDiscovery: true})
	if err != nil {
		t.Fatal(err)
	}
	tok, err := cred.GetToken(context.Background(), policy.TokenRequestOptions{Scopes: []string{"https://storage.azure.com/.default"}})
	if err != nil {
		t.Fatal(err)
	}
	if claims(t, tok.Token)["appid"] != client {
		t.Fatal("wrong appid")
	}
}

func TestWorkloadIdentityCredential_ReadsTheFederatedTokenFile(t *testing.T) {
	dir := t.TempDir()
	f := filepath.Join(dir, "token")
	os.WriteFile(f, []byte("federated-assertion"), 0o600)
	hc, _ := newTransport()
	cred, err := azidentity.NewWorkloadIdentityCredential(&azidentity.WorkloadIdentityCredentialOptions{
		ClientOptions: secretOpts(hc), TenantID: tenant, ClientID: client, TokenFilePath: f, DisableInstanceDiscovery: true})
	if err != nil {
		t.Fatal(err)
	}
	tok, err := cred.GetToken(context.Background(), policy.TokenRequestOptions{Scopes: []string{"https://vault.azure.net/.default"}})
	if err != nil {
		t.Fatal(err)
	}
	if claims(t, tok.Token)["appid"] != client {
		t.Fatal("wrong appid")
	}
}

func TestEnvironmentCredential_ReadsTenantClientSecretAndAuthorityHostFromTheEnvironment(t *testing.T) {
	t.Setenv("AZURE_TENANT_ID", tenant)
	t.Setenv("AZURE_CLIENT_ID", client)
	t.Setenv("AZURE_CLIENT_SECRET", "s")
	t.Setenv("AZURE_AUTHORITY_HOST", authority())
	hc, _ := newTransport()
	cred, err := azidentity.NewEnvironmentCredential(&azidentity.EnvironmentCredentialOptions{ClientOptions: azcore.ClientOptions{Transport: hc}, DisableInstanceDiscovery: true})
	if err != nil {
		t.Fatal(err)
	}
	tok, err := cred.GetToken(context.Background(), policy.TokenRequestOptions{Scopes: []string{"https://storage.azure.com/.default"}})
	if err != nil {
		t.Fatal(err)
	}
	if claims(t, tok.Token)["tid"] != tenant {
		t.Fatal("wrong tenant")
	}
}

func TestDeviceCodeCredential_PromptRunsAndTheCodeIsApprovedAtOnce(t *testing.T) {
	hc, _ := newTransport()
	prompted := false
	cred, err := azidentity.NewDeviceCodeCredential(&azidentity.DeviceCodeCredentialOptions{
		ClientOptions: secretOpts(hc), TenantID: tenant, ClientID: client, DisableInstanceDiscovery: true,
		UserPrompt: func(ctx context.Context, m azidentity.DeviceCodeMessage) error {
			prompted = m.UserCode != ""
			return nil
		}})
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	tok, err := cred.GetToken(ctx, policy.TokenRequestOptions{Scopes: []string{"https://storage.azure.com/.default"}})
	if err != nil {
		t.Fatal(err)
	}
	if !prompted || tok.Token == "" {
		t.Fatalf("prompted=%v", prompted)
	}
}

func TestOnBehalfOfCredential_ExchangesAUserAssertionForAToken(t *testing.T) {
	hc, _ := newTransport()
	hdr := base64.RawURLEncoding.EncodeToString([]byte(`{"alg":"none"}`))
	body := base64.RawURLEncoding.EncodeToString([]byte(`{"upn":"u@example.test","sub":"x","tid":"` + tenant + `"}`))
	assertion := hdr + "." + body + "."
	cred, err := azidentity.NewOnBehalfOfCredentialWithSecret(tenant, client, assertion, "s", &azidentity.OnBehalfOfCredentialOptions{
		ClientOptions: secretOpts(hc), DisableInstanceDiscovery: true})
	if err != nil {
		t.Fatal(err)
	}
	tok, err := cred.GetToken(context.Background(), policy.TokenRequestOptions{Scopes: []string{"https://storage.azure.com/.default"}})
	if err != nil {
		t.Fatal(err)
	}
	if claims(t, tok.Token)["upn"] != "u@example.test" {
		t.Fatalf("claims %v", claims(t, tok.Token))
	}
}
