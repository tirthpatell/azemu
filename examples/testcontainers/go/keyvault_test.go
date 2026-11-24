package example

import (
	"context"
	"testing"

	"azemu-testcontainers-go/emulator"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azidentity"
	"github.com/Azure/azure-sdk-for-go/sdk/security/keyvault/azsecrets"
)

// Key Vault needs https and a token. The credential is the one a service uses on Azure; two variables send it to the
// emulator's token endpoint. The client trusts this container's CA through its transport.
func TestKeyVaultOverHTTPS(t *testing.T) {
	e := startEmulator(t)
	t.Setenv("IDENTITY_ENDPOINT", e.URL("http", emulator.Gateway, "/_azemu/identity/msi/token"))
	t.Setenv("IDENTITY_HEADER", "azemu")
	credential, err := azidentity.NewManagedIdentityCredential(nil)
	if err != nil {
		t.Fatal(err)
	}
	secrets, err := azsecrets.NewClient(e.URL("https", emulator.KeyVault, ""), credential, &azsecrets.ClientOptions{
		ClientOptions: azcore.ClientOptions{Transport: trusting(t, e)},
		// A host with one label ("localhost") fails the client's check of the Key Vault challenge: switch it off.
		DisableChallengeResourceVerification: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()

	// "db-password" comes from seed.json.
	seeded, err := secrets.GetSecret(ctx, "db-password", "", nil)
	if err != nil || *seeded.Value != "local-only-password" {
		t.Fatalf("seeded secret: %v %v", seeded.Value, err)
	}
	value := "hunter2"
	if _, err := secrets.SetSecret(ctx, "api-key", azsecrets.SetSecretParameters{Value: &value}, nil); err != nil {
		t.Fatal(err)
	}
	got, err := secrets.GetSecret(ctx, "api-key", "", nil)
	if err != nil || *got.Value != value {
		t.Fatalf("secret: %v %v", got.Value, err)
	}

	// After a reset the secret that a test made is gone and the seeded one is back.
	if err := e.Reset(); err != nil {
		t.Fatal(err)
	}
	if _, err := secrets.GetSecret(ctx, "api-key", "", nil); err == nil {
		t.Fatal("api-key survived the reset")
	}
	if _, err := secrets.GetSecret(ctx, "db-password", "", nil); err != nil {
		t.Fatal(err)
	}
}
