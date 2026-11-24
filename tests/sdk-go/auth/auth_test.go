// Package auth runs the Go SDK against the emulator started with --strict-auth (docs/strict-auth.md): a principal with no role is
// refused, one with the role is accepted, for Blob and Key Vault. The test shows what a Go program sees: an *azcore.ResponseError
// with the status and the error code.
package auth

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"testing"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/cloud"
	"github.com/Azure/azure-sdk-for-go/sdk/azidentity"
	"github.com/Azure/azure-sdk-for-go/sdk/security/keyvault/azsecrets"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/bloberror"
)

func TestMain(m *testing.M) { emu.Main(m, "--strict-auth") }

const (
	tenant   = "11111111-2222-3333-4444-555555555555"
	withRole = "aaaaaaaa-0000-4000-8000-000000000011"
	noRole   = "aaaaaaaa-0000-4000-8000-000000000012"
)

func transport() *http.Client { return &http.Client{Transport: emu.E.NewTransport()} }

// cred is a client secret credential for one principal, against the emulator's token endpoint (the secret is not checked).
func cred(t *testing.T, client string) *azidentity.ClientSecretCredential {
	t.Helper()
	c, err := azidentity.NewClientSecretCredential(tenant, client, "not-checked", &azidentity.ClientSecretCredentialOptions{
		ClientOptions: azcore.ClientOptions{
			Transport: transport(),
			Cloud:     cloud.Configuration{ActiveDirectoryAuthorityHost: fmt.Sprintf("https://localhost:%d/_azemu/identity/login", emu.E.Port)},
		},
		DisableInstanceDiscovery:   true,
		AdditionallyAllowedTenants: []string{"*"},
	})
	if err != nil {
		t.Fatal(err)
	}
	return c
}

// give makes the principal (named by client id) hold a role at a short scope, through the admin API.
func give(t *testing.T, client, role, scope string) {
	t.Helper()
	body, _ := json.Marshal(map[string]string{"clientId": client, "role": role, "scope": scope})
	r, err := http.Post(emu.E.Gateway()+"/_azemu/auth/assignments", "application/json", bytes.NewReader(body))
	if err != nil {
		t.Fatal(err)
	}
	defer r.Body.Close()
	if r.StatusCode != 201 && r.StatusCode != 200 {
		b, _ := io.ReadAll(r.Body)
		t.Fatalf("give %s %s: %d %s", role, scope, r.StatusCode, b)
	}
}

func responseError(t *testing.T, err error) *azcore.ResponseError {
	t.Helper()
	var re *azcore.ResponseError
	if !errors.As(err, &re) {
		t.Fatalf("not an *azcore.ResponseError: %T %v", err, err)
	}
	return re
}

func TestBlob_APrincipalWithoutTheRoleIsRefusedAndOneWithItIsAccepted(t *testing.T) {
	ctx := context.Background()
	account := emu.Name("goauth")
	url := emu.E.HTTPS("blob", account)
	give(t, withRole, "Storage Blob Data Contributor", "storage/"+account)

	opts := &azblob.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: transport()}}
	good, err := azblob.NewClient(url, cred(t, withRole), opts)
	if err != nil {
		t.Fatal(err)
	}
	bad, err := azblob.NewClient(url, cred(t, noRole), opts)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := good.CreateContainer(ctx, "data", nil); err != nil {
		t.Fatalf("with the role: %v", err)
	}
	if _, err := good.UploadBuffer(ctx, "data", "b1", []byte("hello"), nil); err != nil {
		t.Fatalf("with the role: %v", err)
	}
	// without the role: the error a Go program sees
	_, err = bad.DownloadStream(ctx, "data", "b1", nil)
	re := responseError(t, err)
	if re.StatusCode != 403 || re.ErrorCode != string(bloberror.AuthorizationPermissionMismatch) {
		t.Fatalf("status %d code %q", re.StatusCode, re.ErrorCode)
	}
	if !bloberror.HasCode(err, bloberror.AuthorizationPermissionMismatch) {
		t.Fatal("bloberror.HasCode does not see the code")
	}
	_, err = bad.CreateContainer(ctx, "other", nil)
	if !bloberror.HasCode(err, bloberror.AuthorizationFailure) {
		t.Fatalf("create container without the role: %v", err)
	}
	// the data is unchanged and the one with the role still reads it
	r, err := good.DownloadStream(ctx, "data", "b1", nil)
	if err != nil {
		t.Fatal(err)
	}
	b, _ := io.ReadAll(r.Body)
	if string(b) != "hello" {
		t.Fatalf("read back %q", b)
	}
}

func TestKeyVault_APrincipalWithoutTheRoleIsForbiddenByRbac(t *testing.T) {
	ctx := context.Background()
	vault := emu.Name("gokv")
	give(t, withRole, "Key Vault Secrets Officer", "keyvault/"+vault)
	url := emu.E.HTTPS("vault", vault)
	opts := &azsecrets.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: transport()}}
	good, err := azsecrets.NewClient(url, cred(t, withRole), opts)
	if err != nil {
		t.Fatal(err)
	}
	bad, err := azsecrets.NewClient(url, cred(t, noRole), opts)
	if err != nil {
		t.Fatal(err)
	}
	v := "s3cret"
	if _, err := good.SetSecret(ctx, "k1", azsecrets.SetSecretParameters{Value: &v}, nil); err != nil {
		t.Fatalf("with the role: %v", err)
	}
	got, err := good.GetSecret(ctx, "k1", "", nil)
	if err != nil || *got.Value != v {
		t.Fatalf("read back: %v", err)
	}
	_, err = bad.GetSecret(ctx, "k1", "", nil)
	re := responseError(t, err)
	if re.StatusCode != 403 || re.ErrorCode != "Forbidden" {
		t.Fatalf("status %d code %q", re.StatusCode, re.ErrorCode)
	}
	body := re.Error()
	for _, want := range []string{"Caller is not authorized to perform action on resource.", "Action: 'Microsoft.KeyVault/vaults/secrets/getSecret/action'", "ForbiddenByRbac"} {
		if !strings.Contains(body, want) {
			t.Errorf("the error does not say %q:\n%s", want, body)
		}
	}
	_, err = bad.SetSecret(ctx, "k2", azsecrets.SetSecretParameters{Value: &v}, nil)
	if re := responseError(t, err); re.StatusCode != 403 {
		t.Fatalf("set without the role: %d", re.StatusCode)
	}
	// a Secrets User reads and does not write
	give(t, noRole, "Key Vault Secrets User", "keyvault/"+vault)
	if _, err := bad.GetSecret(ctx, "k1", "", nil); err != nil {
		t.Fatalf("with a reading role: %v", err)
	}
	if _, err := bad.SetSecret(ctx, "k2", azsecrets.SetSecretParameters{Value: &v}, nil); err == nil {
		t.Fatal("a Secrets User may not set a secret")
	}
}
