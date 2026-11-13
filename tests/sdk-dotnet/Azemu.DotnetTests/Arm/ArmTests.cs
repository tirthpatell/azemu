using Azure;
using Azure.Core;
using Azure.ResourceManager;
using Azure.ResourceManager.Resources;
using Azure.ResourceManager.Resources.Models;
using Azure.ResourceManager.ServiceBus;
using Azure.ResourceManager.ServiceBus.Models;
using Azure.ResourceManager.Storage;
using Azure.ResourceManager.Storage.Models;
using Azure.Messaging.ServiceBus;
using Azure.Storage;
using Azure.Storage.Blobs;
using Azemu.DotnetTests.Storage;

namespace Azemu.DotnetTests.ArmTests;

public sealed class ArmFixture : EmuFixture { public ArmFixture() : base("arm") { } }

/// <summary>Azure.ResourceManager against https://management.localhost:{port}, then the data-plane SDKs on what it created.</summary>
public class ArmTests : IClassFixture<ArmFixture>
{
    readonly Emu _emu;
    const string Sub = "11111111-2222-3333-4444-555555555555";
    public ArmTests(ArmFixture f) { _emu = f.Emu; }

    ArmClient Arm()
    {
        var endpoint = new Uri($"https://management.localhost:{_emu.Port}");
        var env = new ArmEnvironment(endpoint, $"https://management.localhost:{_emu.Port}/.default");
        return new ArmClient(new FakeTokenCredential(), Sub, new ArmClientOptions { Environment = env, Transport = _emu.Transport() });
    }

    async Task<ResourceGroupResource> Group(string? name = null)
    {
        var sub = await Arm().GetDefaultSubscriptionAsync();
        var op = await sub.GetResourceGroups().CreateOrUpdateAsync(WaitUntil.Completed, name ?? Names.Dashed("rg"), new ResourceGroupData(AzureLocation.EastUS) { Tags = { ["env"] = "test" } });
        return op.Value;
    }

    [Fact]
    public async Task Subscription_CanBeReadAndHasLocations()
    {
        var sub = await Arm().GetDefaultSubscriptionAsync();
        Assert.Equal(Sub, sub.Data.SubscriptionId);
        var locs = new List<LocationExpanded>(); await foreach (var l in sub.GetLocationsAsync()) locs.Add(l);
        Assert.Contains(locs, l => l.Name == "eastus");
    }

    [Fact]
    public async Task ResourceGroup_CreateReadTagsListDelete()
    {
        var rg = await Group();
        Assert.Equal("test", rg.Data.Tags["env"]);
        Assert.Equal("Succeeded", rg.Data.ResourceGroupProvisioningState);
        var sub = await Arm().GetDefaultSubscriptionAsync();
        Assert.True((await sub.GetResourceGroups().ExistsAsync(rg.Data.Name)).Value);
        var names = new List<string>(); await foreach (var g in sub.GetResourceGroups().GetAllAsync()) names.Add(g.Data.Name);
        Assert.Contains(rg.Data.Name, names);
        await rg.DeleteAsync(WaitUntil.Completed);
        Assert.False((await sub.GetResourceGroups().ExistsAsync(rg.Data.Name)).Value);
    }

    [Fact]
    public async Task ResourceGroup_Missing_Fails404ResourceGroupNotFound()
    {
        var sub = await Arm().GetDefaultSubscriptionAsync();
        var ex = await Assert.ThrowsAsync<RequestFailedException>(async () => await sub.GetResourceGroups().GetAsync(Names.Dashed("nope")));
        Assert.Equal(404, ex.Status);
        Assert.Equal("ResourceGroupNotFound", ex.ErrorCode);
    }

    [Fact]
    public async Task StorageAccount_Create_ThenContainerCreatedThroughArmIsUsableFromTheBlobSdk()
    {
        var rg = await Group();
        var name = Names.Unique("sa");
        var acct = (await rg.GetStorageAccounts().CreateOrUpdateAsync(WaitUntil.Completed, name, new StorageAccountCreateOrUpdateContent(new StorageSku(StorageSkuName.StandardLrs), StorageKind.StorageV2, AzureLocation.EastUS))).Value;
        Assert.Equal(name, acct.Data.Name);
        Assert.Equal(StorageSkuName.StandardLrs, acct.Data.Sku.Name);
        Assert.Equal("Succeeded", acct.Data.ProvisioningState.ToString());
        var blobService = acct.GetBlobService();
        var containers = blobService.GetBlobContainers();
        var cname = Names.Unique("c");
        var cont = (await containers.CreateOrUpdateAsync(WaitUntil.Completed, cname, new BlobContainerData())).Value;
        Assert.Equal(cname, cont.Data.Name);
        // the container exists in the Blob service
        var key = Convert.ToBase64String(new byte[32]);
        var svc = new BlobServiceClient(new Uri($"https://{name}.blob.core.localhost:{_emu.Port}"), new StorageSharedKeyCredential(name, key), new BlobClientOptions { Transport = _emu.TransportWithoutHeadContentType() });
        Assert.True((await svc.GetBlobContainerClient(cname).ExistsAsync()).Value);
        await svc.GetBlobContainerClient(cname).GetBlobClient("b").UploadAsync(BinaryData.FromString("data"));
        var listed = new List<string>(); await foreach (var c in containers.GetAllAsync()) listed.Add(c.Data.Name);
        Assert.Contains(cname, listed);
    }

    [Fact]
    public async Task StorageAccount_ListKeys_AndCheckNameAvailability()
    {
        var rg = await Group();
        var name = Names.Unique("sa");
        var acct = (await rg.GetStorageAccounts().CreateOrUpdateAsync(WaitUntil.Completed, name, new StorageAccountCreateOrUpdateContent(new StorageSku(StorageSkuName.StandardLrs), StorageKind.StorageV2, AzureLocation.EastUS))).Value;
        var keys = new List<StorageAccountKey>(); await foreach (var k in acct.GetKeysAsync()) keys.Add(k);
        Assert.Equal(2, keys.Count);
        Assert.All(keys, k => Assert.False(string.IsNullOrEmpty(k.Value)));
        var sub = await Arm().GetDefaultSubscriptionAsync();
        var avail = (await sub.CheckStorageAccountNameAvailabilityAsync(new StorageAccountNameAvailabilityContent(name))).Value;
        Assert.False(avail.IsNameAvailable);
    }

    [Fact]
    public async Task ServiceBus_NamespaceAndQueueCreatedThroughArm_WorkOverAmqp()
    {
        var rg = await Group();
        var ns = Names.Dashed("ns");
        var nsRes = (await rg.GetServiceBusNamespaces().CreateOrUpdateAsync(WaitUntil.Completed, ns, new ServiceBusNamespaceData(AzureLocation.EastUS) { Sku = new ServiceBusSku(ServiceBusSkuName.Standard) })).Value;
        Assert.Equal(ns, nsRes.Data.Name);
        var qname = "orders";
        var q = (await nsRes.GetServiceBusQueues().CreateOrUpdateAsync(WaitUntil.Completed, qname, new ServiceBusQueueData { MaxDeliveryCount = 4, LockDuration = TimeSpan.FromSeconds(45), RequiresSession = false })).Value;
        Assert.Equal(4, q.Data.MaxDeliveryCount);
        Assert.Equal(TimeSpan.FromSeconds(45), q.Data.LockDuration);
        // use it with the data-plane SDK
        var cs = $"Endpoint=sb://{ns}.servicebus.localhost:{_emu.Port};SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true";
        await using var client = new ServiceBusClient(cs);
        await client.CreateSender(qname).SendMessageAsync(new ServiceBusMessage("from arm queue"));
        var m = await client.CreateReceiver(qname).ReceiveMessageAsync(TimeSpan.FromSeconds(10));
        Assert.Equal("from arm queue", m!.Body.ToString());
        // and the queue's runtime counts show in ARM
        var re = (await nsRes.GetServiceBusQueueAsync(qname)).Value;
        Assert.Equal(4, re.Data.MaxDeliveryCount);
    }

    [Fact]
    public async Task ServiceBus_TopicAndSubscriptionAndRule_ThroughArm_RouteMessages()
    {
        var rg = await Group();
        var ns = Names.Dashed("ns");
        var nsRes = (await rg.GetServiceBusNamespaces().CreateOrUpdateAsync(WaitUntil.Completed, ns, new ServiceBusNamespaceData(AzureLocation.EastUS))).Value;
        var topic = (await nsRes.GetServiceBusTopics().CreateOrUpdateAsync(WaitUntil.Completed, "events", new ServiceBusTopicData())).Value;
        var sub = (await topic.GetServiceBusSubscriptions().CreateOrUpdateAsync(WaitUntil.Completed, "red", new ServiceBusSubscriptionData { MaxDeliveryCount = 5 })).Value;
        // like Azure, a subscription made through ARM starts with the $Default rule (every message); drop it
        await (await sub.GetServiceBusRuleAsync("$Default")).Value.DeleteAsync(WaitUntil.Completed);
        await sub.GetServiceBusRules().CreateOrUpdateAsync(WaitUntil.Completed, "only-red", new ServiceBusRuleData { FilterType = ServiceBusFilterType.SqlFilter, SqlFilter = new ServiceBusSqlFilter { SqlExpression = "color = 'red'" } });
        var cs = $"Endpoint=sb://{ns}.servicebus.localhost:{_emu.Port};SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true";
        await using var client = new ServiceBusClient(cs);
        var red = new ServiceBusMessage("r"); red.ApplicationProperties["color"] = "red";
        var blue = new ServiceBusMessage("b"); blue.ApplicationProperties["color"] = "blue";
        await client.CreateSender("events").SendMessagesAsync(new[] { red, blue });
        var recv = client.CreateReceiver("events", "red");
        var got = await recv.ReceiveMessagesAsync(5, TimeSpan.FromSeconds(3));
        Assert.Equal(new[] { "r" }, got.Select(g => g.Body.ToString()).ToArray());
    }

    [Fact]
    public async Task GenericResources_ListedInAGroup_AndTagsUpdated()
    {
        var rg = await Group();
        var name = Names.Unique("sa");
        await rg.GetStorageAccounts().CreateOrUpdateAsync(WaitUntil.Completed, name, new StorageAccountCreateOrUpdateContent(new StorageSku(StorageSkuName.StandardLrs), StorageKind.StorageV2, AzureLocation.EastUS));
        var res = new List<GenericResource>(); await foreach (var r in rg.GetGenericResourcesAsync()) res.Add(r);
        Assert.Contains(res, r => r.Data.Name == name && r.Data.ResourceType.ToString() == "Microsoft.Storage/storageAccounts");
        await rg.AddTagAsync("added", "yes");
        Assert.Equal("yes", (await rg.GetAsync()).Value.Data.Tags["added"]);
    }

    [Fact]
    public async Task Deployment_OfATemplate_CreatesTheStorageAccountItDescribes()
    {
        var rg = await Group();
        var name = Names.Unique("tpl");
        var template = "{\"$schema\":\"https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#\",\"contentVersion\":\"1.0.0.0\",\"resources\":[{\"type\":\"Microsoft.Storage/storageAccounts\",\"apiVersion\":\"2023-01-01\",\"name\":\"" + name + "\",\"location\":\"eastus\",\"sku\":{\"name\":\"Standard_LRS\"},\"kind\":\"StorageV2\"}]}";
        var content = new ArmDeploymentContent(new ArmDeploymentProperties(ArmDeploymentMode.Incremental) { Template = BinaryData.FromString(template) });
        // SDK limit: WaitUntil.Completed ends in "No ModelReaderWriterTypeBuilder found for ArmDeploymentData" (Azure.ResourceManager.Resources 1.12.0,
        // thrown in ArmDeploymentOperationSource before any response content is looked at), so start it and read the deployment with Get
        var depName = Names.Dashed("dep");
        await rg.GetArmDeployments().CreateOrUpdateAsync(WaitUntil.Started, depName, content);
        var ok = await Wait.UntilAsync(async () => (await rg.GetArmDeploymentAsync(depName)).Value.Data.Properties.ProvisioningState?.ToString() == "Succeeded", 20);
        Assert.True(ok);
        Assert.True((await rg.GetStorageAccounts().ExistsAsync(name)).Value);
    }
}
