package azemu;

import com.azure.core.credential.AccessToken;
import com.azure.core.credential.TokenCredential;
import com.azure.core.management.AzureEnvironment;
import com.azure.core.management.Region;
import com.azure.core.management.profile.AzureProfile;
import com.azure.resourcemanager.resources.ResourceManager;
import com.azure.resourcemanager.storage.StorageManager;
import com.azure.resourcemanager.storage.fluent.models.BlobServicePropertiesInner;
import com.azure.resourcemanager.storage.models.StorageAccountSkuType;
import reactor.core.publisher.Mono;

import java.time.OffsetDateTime;
import java.util.HashMap;
import java.util.Map;

/**
 * Blob versioning is off until someone turns it on, and a team turns it on through the management API. This makes a
 * storage account through ARM and sets {@code isVersioningEnabled} on {@code blobServices/default}; the Blob endpoint of the account
 * then behaves like an Azure account with versioning. Any key works with it (the emulator checks the keys of the development
 * account only).
 */
final class Versioning {
    private static final String SUB = "11111111-2222-3333-4444-555555555555";
    private static final TokenCredential CRED = r -> Mono.just(new AccessToken("arm-token", OffsetDateTime.now().plusHours(1)));

    private Versioning() {}

    /** The name of a new storage account with versioning on. */
    static String account(Emu emu, String prefix) {
        Map<String, String> ep = new HashMap<>(AzureEnvironment.AZURE.getEndpoints());
        String arm = emu.armUrl() + "/";
        ep.put("resourceManagerEndpointUrl", arm);
        ep.put("managementEndpointUrl", arm);
        AzureProfile profile = new AzureProfile("22222222-2222-2222-2222-222222222222", SUB, new AzureEnvironment(ep));
        ResourceManager resources = ResourceManager.authenticate(CRED, profile).withSubscription(SUB);
        StorageManager storage = StorageManager.authenticate(CRED, profile);
        String group = Emu.dashed("rgver");
        resources.resourceGroups().define(group).withRegion(Region.US_EAST).create();
        String account = Emu.name(prefix);
        storage.storageAccounts().define(account).withRegion(Region.US_EAST).withExistingResourceGroup(group)
            .withSku(StorageAccountSkuType.STANDARD_LRS).create();
        BlobServicePropertiesInner on = storage.serviceClient().getBlobServices()
            .setServiceProperties(group, account, new BlobServicePropertiesInner().withIsVersioningEnabled(true));
        if (!Boolean.TRUE.equals(on.isVersioningEnabled())) throw new IllegalStateException("versioning was not switched on");
        return account;
    }
}
