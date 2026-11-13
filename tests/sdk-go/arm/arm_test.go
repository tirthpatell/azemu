package arm

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"strings"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/arm"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/cloud"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/runtime"
	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azservicebus"
	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azservicebus/admin"
	"github.com/Azure/azure-sdk-for-go/sdk/resourcemanager/resources/armresources"
	"github.com/Azure/azure-sdk-for-go/sdk/resourcemanager/servicebus/armservicebus"
	"github.com/Azure/azure-sdk-for-go/sdk/resourcemanager/storage/armstorage"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/container"
)

func TestMain(m *testing.M) { emu.Main(m) }

const sub = "11111111-2222-3333-4444-555555555555"

func ptr[T any](v T) *T { return &v }

// armOpts points an ARM client at the emulator: a cloud configuration with the management endpoint, and the CA-trusting transport.
func armOpts() *arm.ClientOptions {
	return &arm.ClientOptions{ClientOptions: azcore.ClientOptions{
		Cloud: cloud.Configuration{
			ActiveDirectoryAuthorityHost: "https://login.localhost",
			Services: map[cloud.ServiceName]cloud.ServiceConfiguration{
				cloud.ResourceManager: {Endpoint: fmt.Sprintf("https://management.localhost:%d", emu.E.Port), Audience: "https://management.azure.com"},
			},
		},
		Transport: &http.Client{Transport: emu.Dump(emu.E.NewTransport())},
	}}
}

var pollOpts = &runtime.PollUntilDoneOptions{Frequency: 200 * time.Millisecond}

func newGroup(t testing.TB) string {
	t.Helper()
	rg, err := armresources.NewResourceGroupsClient(sub, emu.Cred(), armOpts())
	if err != nil {
		t.Fatal(err)
	}
	name := emu.Name("rg")
	if _, err := rg.CreateOrUpdate(context.Background(), name, armresources.ResourceGroup{Location: ptr("eastus")}, nil); err != nil {
		t.Fatalf("create group: %v", err)
	}
	return name
}

func TestResourceGroup_CreateGetCheckExistenceTagsListAndDeleteWithPoller(t *testing.T) {
	ctx := context.Background()
	c, _ := armresources.NewResourceGroupsClient(sub, emu.Cred(), armOpts())
	name := emu.Name("rg")
	created, err := c.CreateOrUpdate(ctx, name, armresources.ResourceGroup{Location: ptr("East US"), Tags: map[string]*string{"team": ptr("a")}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if *created.Name != name || *created.Location != "eastus" || *created.Tags["team"] != "a" || *created.Properties.ProvisioningState != "Succeeded" {
		t.Fatalf("created %+v", created.ResourceGroup)
	}
	ex, err := c.CheckExistence(ctx, name, nil)
	if err != nil || !ex.Success {
		t.Fatalf("exists: %v %v", ex.Success, err)
	}
	found := false
	for p := c.NewListPager(nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		for _, g := range pg.Value {
			found = found || *g.Name == name
		}
	}
	if !found {
		t.Fatal("group not listed")
	}
	poller, err := c.BeginDelete(ctx, name, nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := poller.PollUntilDone(ctx, pollOpts); err != nil {
		t.Fatalf("delete poller: %v", err)
	}
	ex, err = c.CheckExistence(ctx, name, nil)
	if err != nil || ex.Success {
		t.Fatalf("group still exists: %v %v", ex.Success, err)
	}
	_, err = c.Get(ctx, name, nil)
	var re *azcore.ResponseError
	if !errors.As(err, &re) || re.StatusCode != http.StatusNotFound || re.ErrorCode != "ResourceGroupNotFound" {
		t.Fatalf("get after delete: %v", err)
	}
}

func TestStorage_AccountCreatedThroughArmIsUsableWithTheBlobSdkAndItsListedKey(t *testing.T) {
	ctx := context.Background()
	rg := newGroup(t)
	accts, err := armstorage.NewAccountsClient(sub, emu.Cred(), armOpts())
	if err != nil {
		t.Fatal(err)
	}
	name := emu.Name("sa")
	poller, err := accts.BeginCreate(ctx, rg, name, armstorage.AccountCreateParameters{
		Kind: ptr(armstorage.KindStorageV2), Location: ptr("eastus"), SKU: &armstorage.SKU{Name: ptr(armstorage.SKUNameStandardLRS)},
	}, nil)
	if err != nil {
		t.Fatalf("begin create: %v", err)
	}
	res, err := poller.PollUntilDone(ctx, pollOpts)
	if err != nil {
		t.Fatalf("create poller: %v", err)
	}
	if *res.Name != name || *res.Properties.ProvisioningState != armstorage.ProvisioningStateSucceeded {
		t.Fatalf("account %+v", res.Account)
	}
	blobURL := *res.Properties.PrimaryEndpoints.Blob
	if !strings.Contains(blobURL, name+".blob.") {
		t.Fatalf("blob endpoint %q", blobURL)
	}
	// container through ARM
	bc, err := armstorage.NewBlobContainersClient(sub, emu.Cred(), armOpts())
	if err != nil {
		t.Fatal(err)
	}
	contName := "from-arm"
	if _, err := bc.Create(ctx, rg, name, contName, armstorage.BlobContainer{ContainerProperties: &armstorage.ContainerProperties{Metadata: map[string]*string{"origin": ptr("arm")}}}, nil); err != nil {
		t.Fatalf("create container: %v", err)
	}
	keys, err := accts.ListKeys(ctx, rg, name, nil)
	if err != nil || len(keys.Keys) != 2 || *keys.Keys[0].Value == "" {
		t.Fatalf("list keys: %v", err)
	}
	cred, err := azblob.NewSharedKeyCredential(name, *keys.Keys[0].Value)
	if err != nil {
		t.Fatal(err)
	}
	cc, err := container.NewClientWithSharedKeyCredential(strings.TrimSuffix(blobURL, "/")+"/"+contName, cred, nil)
	if err != nil {
		t.Fatal(err)
	}
	p, err := cc.GetProperties(ctx, nil)
	if err != nil {
		t.Fatalf("the container made by ARM is not there for the data plane: %v", err)
	}
	var origin string
	for k, v := range p.Metadata {
		if strings.EqualFold(k, "origin") {
			origin = *v
		}
	}
	if origin != "arm" {
		t.Fatalf("metadata %v", p.Metadata)
	}
	if _, err := cc.NewBlockBlobClient("hello.txt").UploadBuffer(ctx, []byte("through arm"), nil); err != nil {
		t.Fatal(err)
	}
	// list it back through ARM
	n := 0
	for pg := bc.NewListPager(rg, name, nil); pg.More(); {
		r, err := pg.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		for _, it := range r.Value {
			if *it.Name == contName {
				n++
			}
		}
	}
	if n != 1 {
		t.Fatalf("container listed %d times through ARM", n)
	}
}

func TestStorage_CheckNameAvailabilityReportsATakenName(t *testing.T) {
	ctx := context.Background()
	rg := newGroup(t)
	accts, _ := armstorage.NewAccountsClient(sub, emu.Cred(), armOpts())
	name := emu.Name("sb")
	free, err := accts.CheckNameAvailability(ctx, armstorage.AccountCheckNameAvailabilityParameters{Name: &name, Type: ptr("Microsoft.Storage/storageAccounts")}, nil)
	if err != nil || !*free.NameAvailable {
		t.Fatalf("free name: %v %+v", err, free)
	}
	p, err := accts.BeginCreate(ctx, rg, name, armstorage.AccountCreateParameters{Kind: ptr(armstorage.KindStorageV2), Location: ptr("eastus"), SKU: &armstorage.SKU{Name: ptr(armstorage.SKUNameStandardLRS)}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	p.PollUntilDone(ctx, pollOpts)
	taken, err := accts.CheckNameAvailability(ctx, armstorage.AccountCheckNameAvailabilityParameters{Name: &name, Type: ptr("Microsoft.Storage/storageAccounts")}, nil)
	if err != nil || *taken.NameAvailable || *taken.Reason != armstorage.ReasonAlreadyExists {
		t.Fatalf("taken name: %v %+v", err, taken)
	}
}

func TestServiceBus_NamespaceAndQueueCreatedThroughArmCarryTheirPropertiesToTheDataPlane(t *testing.T) {
	ctx := context.Background()
	rg := newGroup(t)
	nsClient, err := armservicebus.NewNamespacesClient(sub, emu.Cred(), armOpts())
	if err != nil {
		t.Fatal(err)
	}
	ns := emu.Name("sbns")
	p, err := nsClient.BeginCreateOrUpdate(ctx, rg, ns, armservicebus.SBNamespace{Location: ptr("eastus"), SKU: &armservicebus.SBSKU{Name: ptr(armservicebus.SKUNameStandard), Tier: ptr(armservicebus.SKUTierStandard)}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	nres, err := p.PollUntilDone(ctx, pollOpts)
	if err != nil {
		t.Fatalf("namespace poller: %v", err)
	}
	if *nres.Name != ns || nres.Properties.ServiceBusEndpoint == nil {
		t.Fatalf("namespace %+v", nres.SBNamespace)
	}
	qc, _ := armservicebus.NewQueuesClient(sub, emu.Cred(), armOpts())
	q, err := qc.CreateOrUpdate(ctx, rg, ns, "arm-queue", armservicebus.SBQueue{Properties: &armservicebus.SBQueueProperties{
		LockDuration: ptr("PT45S"), MaxDeliveryCount: ptr(int32(4)), RequiresSession: ptr(false), DeadLetteringOnMessageExpiration: ptr(true),
	}}, nil)
	if err != nil {
		t.Fatalf("create queue: %v", err)
	}
	if *q.Properties.LockDuration != "PT45S" || *q.Properties.MaxDeliveryCount != 4 {
		t.Fatalf("queue %+v", q.Properties)
	}
	// the data plane sees the same settings through the namespace's management API
	adm, err := admin.NewClientFromConnectionString(emu.E.ServiceBusConnStr(ns), &admin.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: emu.E.Transport()}})
	if err != nil {
		t.Fatal(err)
	}
	got, err := adm.GetQueue(ctx, "arm-queue", nil)
	if err != nil || got == nil {
		t.Fatalf("queue not visible in the data plane: %v", err)
	}
	if *got.LockDuration != "PT45S" || *got.MaxDeliveryCount != 4 || !*got.DeadLetteringOnMessageExpiration {
		t.Fatalf("data plane properties %+v", got.QueueProperties)
	}
	// and AMQP works on it
	c, err := azservicebus.NewClientFromConnectionString(emu.E.ServiceBusConnStr(ns), nil)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close(ctx)
	s, _ := c.NewSender("arm-queue", nil)
	if err := s.SendMessage(ctx, &azservicebus.Message{Body: []byte("via arm queue")}, nil); err != nil {
		t.Fatal(err)
	}
	s.Close(ctx)
	r, _ := c.NewReceiverForQueue("arm-queue", nil)
	defer r.Close(ctx)
	rctx, cancel := context.WithTimeout(ctx, 20*time.Second)
	defer cancel()
	ms, err := r.ReceiveMessages(rctx, 1, nil)
	if err != nil || len(ms) != 1 || string(ms[0].Body) != "via arm queue" {
		t.Fatalf("receive: %v %d", err, len(ms))
	}
}

func TestServiceBus_ListKeysOfTheRootRuleGivesAConnectionStringThatWorks(t *testing.T) {
	ctx := context.Background()
	rg := newGroup(t)
	nsClient, _ := armservicebus.NewNamespacesClient(sub, emu.Cred(), armOpts())
	ns := emu.Name("sbk")
	p, err := nsClient.BeginCreateOrUpdate(ctx, rg, ns, armservicebus.SBNamespace{Location: ptr("eastus"), SKU: &armservicebus.SBSKU{Name: ptr(armservicebus.SKUNameStandard)}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	p.PollUntilDone(ctx, pollOpts)
	keys, err := nsClient.ListKeys(ctx, rg, ns, "RootManageSharedAccessKey", nil)
	if err != nil {
		t.Fatalf("list keys: %v", err)
	}
	if keys.PrimaryKey == nil || *keys.PrimaryKey == "" || keys.PrimaryConnectionString == nil || !strings.Contains(*keys.PrimaryConnectionString, "SharedAccessKey=") {
		t.Fatalf("keys %+v", keys.AccessKeys)
	}
}

func TestDeployment_ArmTemplateCreatesAStorageAccountThatTheDataPlaneKnows(t *testing.T) {
	ctx := context.Background()
	rg := newGroup(t)
	dc, err := armresources.NewDeploymentsClient(sub, emu.Cred(), armOpts())
	if err != nil {
		t.Fatal(err)
	}
	acct := emu.Name("tpl")
	tmpl := map[string]any{
		"$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#", "contentVersion": "1.0.0.0",
		"parameters": map[string]any{"acct": map[string]any{"type": "string"}},
		"resources": []any{map[string]any{
			"type": "Microsoft.Storage/storageAccounts", "apiVersion": "2023-01-01", "name": "[parameters('acct')]", "location": "eastus",
			"sku": map[string]any{"name": "Standard_LRS"}, "kind": "StorageV2", "properties": map[string]any{},
		}},
		"outputs": map[string]any{"name": map[string]any{"type": "string", "value": "[parameters('acct')]"}},
	}
	p, err := dc.BeginCreateOrUpdate(ctx, rg, "dep1", armresources.Deployment{Properties: &armresources.DeploymentProperties{
		Mode: ptr(armresources.DeploymentModeIncremental), Template: tmpl, Parameters: map[string]any{"acct": map[string]any{"value": acct}},
	}}, nil)
	if err != nil {
		t.Fatalf("begin deployment: %v", err)
	}
	res, err := p.PollUntilDone(ctx, pollOpts)
	if err != nil {
		t.Fatalf("deployment: %v", err)
	}
	if *res.Properties.ProvisioningState != armresources.ProvisioningStateSucceeded {
		t.Fatalf("state %v", *res.Properties.ProvisioningState)
	}
	accts, _ := armstorage.NewAccountsClient(sub, emu.Cred(), armOpts())
	got, err := accts.GetProperties(ctx, rg, acct, nil)
	if err != nil || *got.Name != acct {
		t.Fatalf("account from the template: %v", err)
	}
}

func TestResource_UnknownGroupIsResourceGroupNotFound404(t *testing.T) {
	accts, _ := armstorage.NewAccountsClient(sub, emu.Cred(), armOpts())
	_, err := accts.GetProperties(context.Background(), emu.Name("nogroup"), "someacct1", nil)
	var re *azcore.ResponseError
	if !errors.As(err, &re) || re.StatusCode != http.StatusNotFound {
		t.Fatalf("got %v", err)
	}
}
