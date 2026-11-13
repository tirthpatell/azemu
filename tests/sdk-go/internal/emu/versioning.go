package emu

import (
	"context"
	"fmt"
	"net/http"
	"testing"
	"time"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/arm"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/cloud"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/runtime"
	"github.com/Azure/azure-sdk-for-go/sdk/resourcemanager/resources/armresources"
	"github.com/Azure/azure-sdk-for-go/sdk/resourcemanager/storage/armstorage"
)

// AccountWithVersioning makes a storage account with blob versioning on and returns its name. Blob versioning is off until
// someone turns it on, and a team turns it on through the management API: a resource group, the storage account,
// and `isVersioningEnabled` on `blobServices/default`. Use the account with any key; the emulator checks keys of the
// development account only.
func AccountWithVersioning(t testing.TB, prefix string) string {
	t.Helper()
	ctx := context.Background()
	const sub = "11111111-2222-3333-4444-555555555555"
	opts := &arm.ClientOptions{ClientOptions: azcore.ClientOptions{
		Cloud: cloud.Configuration{
			ActiveDirectoryAuthorityHost: "https://login.localhost",
			Services: map[cloud.ServiceName]cloud.ServiceConfiguration{
				cloud.ResourceManager: {Endpoint: fmt.Sprintf("https://management.localhost:%d", E.Port), Audience: "https://management.azure.com"},
			},
		},
		Transport: &http.Client{Transport: E.NewTransport()},
	}}
	str := func(s string) *string { return &s }
	groups, err := armresources.NewResourceGroupsClient(sub, Cred(), opts)
	if err != nil {
		t.Fatal(err)
	}
	rg := Name("rgver")
	if _, err := groups.CreateOrUpdate(ctx, rg, armresources.ResourceGroup{Location: str("eastus")}, nil); err != nil {
		t.Fatalf("create resource group: %v", err)
	}
	accounts, err := armstorage.NewAccountsClient(sub, Cred(), opts)
	if err != nil {
		t.Fatal(err)
	}
	name := Name(prefix)
	kind, sku := armstorage.KindStorageV2, armstorage.SKUNameStandardLRS
	poller, err := accounts.BeginCreate(ctx, rg, name, armstorage.AccountCreateParameters{Kind: &kind, Location: str("eastus"), SKU: &armstorage.SKU{Name: &sku}}, nil)
	if err != nil {
		t.Fatalf("create storage account: %v", err)
	}
	if _, err := poller.PollUntilDone(ctx, &runtime.PollUntilDoneOptions{Frequency: 200 * time.Millisecond}); err != nil {
		t.Fatalf("storage account: %v", err)
	}
	services, err := armstorage.NewBlobServicesClient(sub, Cred(), opts)
	if err != nil {
		t.Fatal(err)
	}
	on := true
	got, err := services.SetServiceProperties(ctx, rg, name, armstorage.BlobServiceProperties{BlobServiceProperties: &armstorage.BlobServicePropertiesProperties{IsVersioningEnabled: &on}}, nil)
	if err != nil {
		t.Fatalf("switch versioning on: %v", err)
	}
	if p := got.BlobServiceProperties.BlobServiceProperties; p == nil || p.IsVersioningEnabled == nil || !*p.IsVersioningEnabled {
		t.Fatalf("versioning was not switched on: %+v", got)
	}
	return name
}
