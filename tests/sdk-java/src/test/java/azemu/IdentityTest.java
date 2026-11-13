package azemu;

import com.azure.core.credential.AccessToken;
import com.azure.core.credential.TokenCredential;
import com.azure.core.credential.TokenRequestContext;
import com.azure.core.exception.ClientAuthenticationException;
import com.azure.core.util.BinaryData;
import com.azure.core.util.Configuration;
import com.azure.identity.*;
import com.azure.security.keyvault.secrets.SecretClient;
import com.azure.security.keyvault.secrets.SecretClientBuilder;
import com.azure.storage.blob.BlobContainerClient;
import com.azure.storage.blob.BlobServiceClient;
import com.azure.storage.blob.BlobServiceClientBuilder;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import org.junit.jupiter.api.*;

import java.time.OffsetDateTime;
import java.util.Base64;
import java.util.Map;

import static org.junit.jupiter.api.Assertions.*;

@EmuTest
class IdentityTest {
    static Emu emu;
    static final ObjectMapper M = new ObjectMapper();
    static final String[] KEYS = {"IDENTITY_ENDPOINT", "IDENTITY_HEADER", "AZURE_POD_IDENTITY_AUTHORITY_HOST", "AZURE_CLIENT_ID", "AZURE_TENANT_ID",
        "AZURE_CLIENT_SECRET", "AZURE_AUTHORITY_HOST"};

    @BeforeAll static void setUp() { emu = Emu.current(); }

    @BeforeEach @AfterEach void clean() {
        for (String k : KEYS) { Configuration.getGlobalConfiguration().remove(k); setEnv(k, null); }
    }

    /**
     * azure-identity 1.19 reads IDENTITY_ENDPOINT and the other variables with System.getenv (MSAL for Java's managed identity client),
     * not from Configuration, and the port of the emulator is only known at run time. So the test puts the variable into
     * the process environment map of this JVM (needs --add-opens java.base/java.util, set in pom.xml). Not something an application does.
     */
    @SuppressWarnings("unchecked")
    static void setEnv(String name, String value) {
        try {
            Map<String, String> env = System.getenv();
            java.lang.reflect.Field f = env.getClass().getDeclaredField("m");
            f.setAccessible(true);
            Map<String, String> m = (Map<String, String>) f.get(env);
            if (value == null) m.remove(name); else m.put(name, value);
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException("cannot set the environment variable " + name, e);
        }
    }

    static void useAppServiceIdentity() {
        setEnv("IDENTITY_ENDPOINT", "http://localhost:" + emu.port + "/_azemu/identity/msi/token");
        setEnv("IDENTITY_HEADER", "azemu");
        Configuration.getGlobalConfiguration().put("IDENTITY_ENDPOINT", "http://localhost:" + emu.port + "/_azemu/identity/msi/token");
        Configuration.getGlobalConfiguration().put("IDENTITY_HEADER", "azemu");
    }

    static JsonNode claims(String jwt) throws Exception {
        String[] p = jwt.split("\\.");
        assertEquals(3, p.length, "a JWT has three parts");
        return M.readTree(Base64.getUrlDecoder().decode(p[1]));
    }

    String loginHost() { return "https://login.localhost:" + emu.port; }

    ClientSecretCredential secretCredential(String tenant, String client) {
        return new ClientSecretCredentialBuilder().tenantId(tenant).clientId(client).clientSecret("anything")
            .authorityHost(loginHost()).disableInstanceDiscovery().build();
    }

    // ---- managed identity ---------------------------------------------------------------------------------
    @Test void managedIdentity_throughIdentityEndpointAndHeader_returnsAJwtForTheRequestedResource() throws Exception {
        useAppServiceIdentity();
        TokenCredential cred = new ManagedIdentityCredentialBuilder().build();
        AccessToken t = cred.getTokenSync(new TokenRequestContext().addScopes("https://storage.azure.com/.default"));
        assertTrue(t.getExpiresAt().isAfter(OffsetDateTime.now().plusMinutes(30)));
        JsonNode c = claims(t.getToken());
        assertEquals("https://storage.azure.com", c.get("aud").asText());
        assertTrue(c.get("iss").asText().startsWith("https://sts.windows.net/"));
    }

    @Test void managedIdentity_userAssignedClientId_isSentAndAppearsInTheToken() throws Exception {
        useAppServiceIdentity();
        String clientId = "12345678-1234-1234-1234-123456789abc";
        AccessToken t = new ManagedIdentityCredentialBuilder().clientId(clientId).build()
            .getTokenSync(new TokenRequestContext().addScopes("https://vault.azure.net/.default"));
        JsonNode c = claims(t.getToken());
        assertEquals("https://vault.azure.net", c.get("aud").asText());
        assertEquals(clientId, c.get("appid").asText());
    }

    @Test void managedIdentity_secondCallIsServedFromTheSdkCache() throws Exception {
        useAppServiceIdentity();
        TokenCredential cred = new ManagedIdentityCredentialBuilder().build();
        TokenRequestContext ctx = new TokenRequestContext().addScopes("https://storage.azure.com/.default");
        assertEquals(cred.getTokenSync(ctx).getToken(), cred.getTokenSync(ctx).getToken());
    }

    @Test void imds_throughAzurePodIdentityAuthorityHost_returnsAToken() throws Exception {
        setEnv("AZURE_POD_IDENTITY_AUTHORITY_HOST", "http://localhost:" + emu.port);
        Configuration.getGlobalConfiguration().put("AZURE_POD_IDENTITY_AUTHORITY_HOST", "http://localhost:" + emu.port);
        AccessToken t = new ManagedIdentityCredentialBuilder().build().getTokenSync(new TokenRequestContext().addScopes("https://storage.azure.com/.default"));
        assertEquals("https://storage.azure.com", claims(t.getToken()).get("aud").asText());
    }

    @Test void defaultAzureCredential_picksTheManagedIdentityFromTheEnvironment_forBlobAndKeyVault() {
        useAppServiceIdentity();
        TokenCredential cred = new DefaultAzureCredentialBuilder().build();
        BlobServiceClient blob = new BlobServiceClientBuilder().endpoint(emu.https("blob", Emu.name("idb"))).credential(cred).buildClient();
        BlobContainerClient c = blob.createBlobContainer(Emu.dashed("c"));
        c.getBlobClient("x").upload(BinaryData.fromString("via default credential"));
        assertEquals("via default credential", c.getBlobClient("x").downloadContent().toString());
        SecretClient secrets = new SecretClientBuilder().vaultUrl(emu.vaultUrl(Emu.name("idv"))).credential(cred).buildClient();
        secrets.setSecret("s", "v1");
        assertEquals("v1", secrets.getSecret("s").getValue());
    }

    // ---- Entra flows against the emulator's login host ---------------------------------------------------
    @Test void clientSecretCredential_againstLoginLocalhost_withInstanceDiscoveryOff_returnsAToken() throws Exception {
        String tenant = "11111111-2222-3333-4444-555555555555";
        AccessToken t = secretCredential(tenant, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").getTokenSync(new TokenRequestContext().addScopes("https://storage.azure.com/.default"));
        JsonNode c = claims(t.getToken());
        assertEquals(tenant, c.get("tid").asText());
        assertEquals("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", c.get("appid").asText());
        assertEquals("https://storage.azure.com", c.get("aud").asText());
    }

    @Test void clientSecretCredential_isUsedForBlobAndKeyVaultCalls() {
        TokenCredential cred = secretCredential("22222222-2222-2222-2222-222222222222", "33333333-3333-3333-3333-333333333333");
        BlobServiceClient blob = new BlobServiceClientBuilder().endpoint(emu.https("blob", Emu.name("csb"))).credential(cred).buildClient();
        BlobContainerClient c = blob.createBlobContainer(Emu.dashed("c"));
        c.getBlobClient("x").upload(BinaryData.fromString("secret credential"));
        assertEquals("secret credential", c.getBlobClient("x").downloadContent().toString());
        // the vault challenge names a tenant other than the credential's, so multi tenant access is allowed explicitly
        TokenCredential multi = new ClientSecretCredentialBuilder().tenantId("22222222-2222-2222-2222-222222222222").clientId("33333333-3333-3333-3333-333333333333")
            .clientSecret("x").authorityHost(loginHost()).disableInstanceDiscovery().additionallyAllowedTenants("*").build();
        SecretClient secrets = new SecretClientBuilder().vaultUrl(emu.vaultUrl(Emu.name("csv"))).credential(multi).buildClient();
        secrets.setSecret("k", "v");
        assertEquals("v", secrets.getSecret("k").getValue());
    }

    @Test void environmentCredential_withTenantClientSecretAndAuthorityHost_works() {
        for (String[] kv : new String[][] {{"AZURE_TENANT_ID", "44444444-4444-4444-4444-444444444444"}, {"AZURE_CLIENT_ID", "55555555-5555-5555-5555-555555555555"},
            {"AZURE_CLIENT_SECRET", "shh"}, {"AZURE_AUTHORITY_HOST", loginHost()}}) setEnv(kv[0], kv[1]);
        Configuration.getGlobalConfiguration().put("AZURE_TENANT_ID", "44444444-4444-4444-4444-444444444444");
        Configuration.getGlobalConfiguration().put("AZURE_CLIENT_ID", "55555555-5555-5555-5555-555555555555");
        Configuration.getGlobalConfiguration().put("AZURE_CLIENT_SECRET", "shh");
        Configuration.getGlobalConfiguration().put("AZURE_AUTHORITY_HOST", loginHost());
        AccessToken t = new EnvironmentCredentialBuilder().build().getTokenSync(new TokenRequestContext().addScopes("https://storage.azure.com/.default"));
        assertNotNull(t.getToken());
    }

    @Test void tokenFromTheEndpoint_hasATrustedSignatureAccordingToTheEmulatorsKeySet() throws Exception {
        useAppServiceIdentity();
        AccessToken t = new ManagedIdentityCredentialBuilder().build().getTokenSync(new TokenRequestContext().addScopes("https://storage.azure.com/.default"));
        String kid = M.readTree(Base64.getUrlDecoder().decode(t.getToken().split("\\.")[0])).get("kid").asText();
        String keys = emu.get("/_azemu/identity/login/common/discovery/v2.0/keys");
        assertTrue(keys.contains(kid), "kid " + kid + " must be in " + keys);
    }
}
