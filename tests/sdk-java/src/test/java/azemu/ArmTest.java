package azemu;

import com.azure.core.credential.AccessToken;
import com.azure.core.credential.TokenCredential;
import com.azure.core.management.AzureEnvironment;
import com.azure.core.management.Region;
import com.azure.core.management.exception.ManagementException;
import com.azure.core.management.profile.AzureProfile;
import com.azure.messaging.servicebus.*;
import com.azure.resourcemanager.resources.ResourceManager;
import com.azure.resourcemanager.resources.models.ResourceGroup;
import com.azure.resourcemanager.servicebus.ServiceBusManager;
import com.azure.resourcemanager.servicebus.models.*;
import com.azure.resourcemanager.storage.StorageManager;
import com.azure.resourcemanager.storage.models.*;
import com.azure.storage.blob.BlobServiceClient;
import com.azure.storage.blob.BlobServiceClientBuilder;
import com.azure.storage.common.StorageSharedKeyCredential;
import org.junit.jupiter.api.*;
import reactor.core.publisher.Mono;

import java.time.Duration;
import java.time.OffsetDateTime;
import java.util.*;
import java.util.stream.Collectors;

import static org.junit.jupiter.api.Assertions.*;

/** ARM through azure-resourcemanager: a resource made there exists in the data plane. */
@EmuTest
class ArmTest {
    static Emu emu;
    static AzureProfile profile;
    static final String SUB = "11111111-2222-3333-4444-555555555555";
    static final TokenCredential CRED = r -> Mono.just(new AccessToken("arm-token", OffsetDateTime.now().plusHours(1)));
    static ResourceManager resources;
    static StorageManager storage;
    static ServiceBusManager serviceBus;

    @BeforeAll static void setUp() {
        emu = Emu.current();
        Map<String, String> ep = new HashMap<>(AzureEnvironment.AZURE.getEndpoints());
        String arm = emu.armUrl() + "/";
        ep.put("resourceManagerEndpointUrl", arm);
        ep.put("managementEndpointUrl", arm);
        profile = new AzureProfile("22222222-2222-2222-2222-222222222222", SUB, new AzureEnvironment(ep));
        resources = ResourceManager.authenticate(CRED, profile).withSubscription(SUB);
        storage = StorageManager.authenticate(CRED, profile);
        serviceBus = ServiceBusManager.authenticate(CRED, profile);
    }

    @Test void resourceGroup_createGetListTagAndDelete() {
        String name = Emu.dashed("rg");
        ResourceGroup rg = resources.resourceGroups().define(name).withRegion(Region.US_EAST).withTag("env", "test").create();
        assertEquals(name, rg.name());
        assertEquals("eastus", rg.regionName());
        assertEquals("test", rg.tags().get("env"));
        assertEquals("Succeeded", rg.provisioningState());
        assertTrue(resources.resourceGroups().contain(name));
        assertTrue(resources.resourceGroups().list().stream().anyMatch(g -> g.name().equals(name)));
        resources.resourceGroups().getByName(name).update().withTag("extra", "1").apply();
        assertEquals("1", resources.resourceGroups().getByName(name).tags().get("extra"));
        resources.resourceGroups().deleteByName(name);
        assertFalse(resources.resourceGroups().contain(name));
    }

    @Test void resourceGroup_getMissing_failsWith404() {
        ManagementException e = assertThrows(ManagementException.class, () -> resources.resourceGroups().getByName(Emu.dashed("nope")));
        assertEquals(404, e.getResponse().getStatusCode());
    }

    @Test void storageAccount_andBlobContainer_madeThroughArm_areUsableWithTheBlobSdk() {
        String rg = Emu.dashed("rg");
        resources.resourceGroups().define(rg).withRegion(Region.US_EAST).create();
        String acct = Emu.name("armst");
        StorageAccount sa = storage.storageAccounts().define(acct).withRegion(Region.US_EAST).withExistingResourceGroup(rg)
            .withSku(StorageAccountSkuType.STANDARD_LRS).create();
        assertEquals(acct, sa.name());
        assertEquals("Succeeded", sa.provisioningState().toString());
        assertFalse(sa.getKeys().isEmpty());
        BlobContainer container = storage.blobContainers().defineContainer("arm-container").withExistingStorageAccount(rg, acct)
            .withPublicAccess(PublicAccess.NONE).withMetadata("owner", "arm").create();
        assertEquals("arm-container", container.name());

        // the container exists in the Blob service
        String key = sa.getKeys().get(0).value();
        BlobServiceClient blob = new BlobServiceClientBuilder().endpoint(emu.blobUrl(acct)).credential(new StorageSharedKeyCredential(acct, Emu.DEV_KEY)).buildClient();
        var c = blob.getBlobContainerClient("arm-container");
        assertTrue(c.exists(), "the container made through ARM exists in the Blob service");
        assertEquals("arm", c.getProperties().getMetadata().get("owner"));
        c.getBlobClient("hello.txt").upload(com.azure.core.util.BinaryData.fromString("via arm"));
        assertEquals("via arm", c.getBlobClient("hello.txt").downloadContent().toString());
        assertTrue(storage.blobContainers().list(rg, acct).stream().anyMatch(i -> i.name().equals("arm-container")));
        // (a container made with the Blob SDK is not listed by ARM: the ARM record is the record, see docs/services/arm.md)
    }

    @Test void storageAccount_checkNameAvailability_andDelete() {
        String rg = Emu.dashed("rg");
        resources.resourceGroups().define(rg).withRegion(Region.US_EAST).create();
        String acct = Emu.name("armdel");
        assertTrue(storage.storageAccounts().checkNameAvailability(acct).isAvailable());
        storage.storageAccounts().define(acct).withRegion(Region.US_EAST).withExistingResourceGroup(rg).withSku(StorageAccountSkuType.STANDARD_LRS).create();
        assertFalse(storage.storageAccounts().checkNameAvailability(acct).isAvailable());
        storage.storageAccounts().deleteByResourceGroup(rg, acct);
        assertTrue(storage.storageAccounts().list().stream().noneMatch(a -> a.name().equals(acct)));
    }

    @Test void serviceBusNamespaceAndQueue_madeThroughArm_workWithTheServiceBusSdk() {
        String rg = Emu.dashed("rg");
        resources.resourceGroups().define(rg).withRegion(Region.US_EAST).create();
        String ns = Emu.name("armsb");
        ServiceBusNamespace namespace = serviceBus.namespaces().define(ns).withRegion(Region.US_EAST).withExistingResourceGroup(rg)
            .withSku(NamespaceSku.STANDARD).create();
        assertEquals(ns, namespace.name());
        com.azure.resourcemanager.servicebus.models.Queue q = namespace.queues().define("arm-queue").withMessageLockDurationInSeconds(45).withSession().create();
        assertEquals("arm-queue", q.name());
        assertEquals(45, q.lockDurationInSeconds());
        assertTrue(q.isSessionEnabled());

        ServiceBusClientBuilder b = Sb.client(emu, ns);
        try (ServiceBusSenderClient s = b.sender().queueName("arm-queue").buildClient()) {
            s.sendMessage(new ServiceBusMessage("made through ARM").setSessionId("s1"));
        }
        try (ServiceBusSessionReceiverClient sr = b.sessionReceiver().queueName("arm-queue").disableAutoComplete().buildClient();
             ServiceBusReceiverClient r = sr.acceptSession("s1")) {
            ServiceBusReceivedMessage m = Sb.receiveOne(r);
            assertEquals("made through ARM", m.getBody().toString());
            r.complete(m);
        }
        // the queue settings seen over the administration API agree with ARM
        var admin = Sb.admin(emu, ns);
        assertEquals(Duration.ofSeconds(45), admin.getQueue("arm-queue").getLockDuration());
        assertTrue(admin.getQueue("arm-queue").isSessionRequired());
        // a topic and subscription
        Topic topic = namespace.topics().define("arm-topic").create();
        topic.subscriptions().define("sub1").create();
        assertTrue(admin.getSubscriptionExists("arm-topic", "sub1"));
    }

    @Test void resourceGroup_withResourcesInside_listsThemGenerically() {
        String rg = Emu.dashed("rg");
        resources.resourceGroups().define(rg).withRegion(Region.US_EAST).create();
        String acct = Emu.name("armlist");
        storage.storageAccounts().define(acct).withRegion(Region.US_EAST).withExistingResourceGroup(rg).withSku(StorageAccountSkuType.STANDARD_LRS).create();
        List<String> types = resources.genericResources().listByResourceGroup(rg).stream().map(r -> r.type()).collect(Collectors.toList());
        assertTrue(types.contains("Microsoft.Storage/storageAccounts"), types.toString());
    }
}
