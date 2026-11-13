package azemu;

import com.azure.core.credential.AccessToken;
import com.azure.core.credential.TokenCredential;
import com.azure.core.exception.HttpResponseException;
import com.azure.core.exception.ResourceExistsException;
import com.azure.core.exception.ResourceNotFoundException;
import com.azure.core.util.polling.LongRunningOperationStatus;
import com.azure.core.util.polling.PollResponse;
import com.azure.core.util.polling.SyncPoller;
import com.azure.security.keyvault.certificates.*;
import com.azure.security.keyvault.certificates.models.*;
import com.azure.security.keyvault.keys.*;
import com.azure.security.keyvault.keys.cryptography.*;
import com.azure.security.keyvault.keys.cryptography.models.*;
import com.azure.security.keyvault.keys.models.*;
import com.azure.security.keyvault.secrets.*;
import com.azure.security.keyvault.secrets.models.*;
import org.junit.jupiter.api.*;
import reactor.core.publisher.Mono;

import java.io.ByteArrayInputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.*;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.time.Duration;
import java.time.OffsetDateTime;
import java.util.*;
import java.util.stream.Collectors;

import static org.junit.jupiter.api.Assertions.*;

/** Key Vault secrets, keys (with CryptographyClient) and certificates. The vault host is not *.vault.azure.net, so challenge verification is off. */
@EmuTest
class KeyVaultTest {
    static Emu emu;
    static String vault;
    static final TokenCredential CRED = r -> Mono.just(new AccessToken("test-token", OffsetDateTime.now().plusHours(1)));

    @BeforeAll static void setUp() {
        emu = Emu.current();
        vault = Emu.name("kvjava");
    }

    SecretClient secrets() { return new SecretClientBuilder().vaultUrl(emu.vaultUrl(vault)).credential(CRED).buildClient(); }
    KeyClient keys() { return new KeyClientBuilder().vaultUrl(emu.vaultUrl(vault)).credential(CRED).buildClient(); }
    CertificateClient certs() { return new CertificateClientBuilder().vaultUrl(emu.vaultUrl(vault)).credential(CRED).buildClient(); }

    /**
     * The challenge names the domain of the request (vault.localhost:port), so the client's own check passes with no switch.
     * The three helpers above use the defaults. The old switch still works.
     */
    @Test void challengeResource_noSwitchNeeded_andTheOldSwitchStillWorks() {
        secrets().setSecret("chk", "v");
        assertEquals("v", secrets().getSecret("chk").getValue());
        assertEquals("chk-key", keys().createEcKey(new com.azure.security.keyvault.keys.models.CreateEcKeyOptions("chk-key")).getName());
        assertTrue(certs().listPropertiesOfCertificates().stream().count() >= 0, "the certificates client passed the challenge too");
        SecretClient old = new SecretClientBuilder().vaultUrl(emu.vaultUrl(vault)).credential(CRED).disableChallengeResourceVerification().buildClient();
        assertEquals("v", old.getSecret("chk").getValue());
    }

    /** createRsaKey sends public_exponent 0, which azemu refuses in older builds; createKey(name, RSA) does not send it. */
    static KeyVaultKey rsa(KeyClient c, String name) { return c.createKey(name, KeyType.RSA); }

    // ---- secrets ----------------------------------------------------------------------------------------------
    @Test void secret_setGetVersionsAndUpdate() {
        SecretClient c = secrets();
        String n = Emu.dashed("s");
        KeyVaultSecret v1 = c.setSecret(n, "one");
        KeyVaultSecret v2 = c.setSecret(new KeyVaultSecret(n, "two").setProperties(new SecretProperties().setContentType("text/plain")
            .setTags(Map.of("env", "test")).setExpiresOn(OffsetDateTime.now().plusDays(1).withNano(0))));
        assertNotEquals(v1.getProperties().getVersion(), v2.getProperties().getVersion());
        assertEquals("two", c.getSecret(n).getValue());
        assertEquals("one", c.getSecret(n, v1.getProperties().getVersion()).getValue());
        KeyVaultSecret got = c.getSecret(n);
        assertEquals("text/plain", got.getProperties().getContentType());
        assertEquals("test", got.getProperties().getTags().get("env"));
        assertNotNull(got.getProperties().getExpiresOn());
        assertTrue(got.getProperties().isEnabled());
        assertTrue(got.getId().startsWith(emu.vaultUrl(vault) + "/secrets/" + n + "/"));

        got.getProperties().setContentType("application/x").setEnabled(false);
        c.updateSecretProperties(got.getProperties());
        // a disabled secret cannot be read (403 SecretDisabled), but its properties are listed
        HttpResponseException disabled = assertThrows(HttpResponseException.class, () -> c.getSecret(n));
        assertEquals(403, disabled.getResponse().getStatusCode());
        List<SecretProperties> versions = c.listPropertiesOfSecretVersions(n).stream().collect(Collectors.toList());
        assertEquals(2, versions.size());
        assertEquals(1, versions.stream().filter(v -> Boolean.FALSE.equals(v.isEnabled())).count());
    }

    @Test void secret_getMissing_is404() {
        assertThrows(ResourceNotFoundException.class, () -> secrets().getSecret(Emu.dashed("missing")));
    }

    @Test void secret_list_pagesThroughAll() {
        SecretClient c = secrets();
        String prefix = Emu.dashed("ls");
        for (int i = 0; i < 12; i++) c.setSecret(prefix + "-" + i, "v");
        long n = c.listPropertiesOfSecrets().stream().filter(p -> p.getName().startsWith(prefix)).count();
        assertEquals(12, n);
    }

    @Test void secret_deleteRecoverAndPurge_softDeleteLifecycle() {
        SecretClient c = secrets();
        String n = Emu.dashed("d");
        c.setSecret(n, "x");
        DeletedSecret deleted = c.beginDeleteSecret(n).waitForCompletion(Duration.ofSeconds(30)).getValue();
        assertNotNull(deleted.getDeletedOn());
        assertNotNull(deleted.getRecoveryId());
        assertThrows(ResourceNotFoundException.class, () -> c.getSecret(n));
        assertEquals(n, c.getDeletedSecret(n).getName());
        c.beginRecoverDeletedSecret(n).waitForCompletion(Duration.ofSeconds(30));
        assertEquals("x", c.getSecret(n).getValue());
        c.beginDeleteSecret(n).waitForCompletion(Duration.ofSeconds(30));
        c.purgeDeletedSecret(n);
        assertThrows(ResourceNotFoundException.class, () -> c.getDeletedSecret(n));
    }

    @Test void secret_backupAndRestore() {
        SecretClient c = secrets();
        String n = Emu.dashed("b");
        c.setSecret(n, "backed up");
        byte[] backup = c.backupSecret(n);
        assertTrue(backup.length > 0);
        c.beginDeleteSecret(n).waitForCompletion(Duration.ofSeconds(30));
        c.purgeDeletedSecret(n);
        KeyVaultSecret restored = c.restoreSecretBackup(backup);
        assertEquals("backed up", c.getSecret(restored.getName()).getValue());
    }

    @Test void secret_unicodeAndLargeValue_roundTrip() {
        SecretClient c = secrets();
        String n = Emu.dashed("u");
        String value = "日本語 ünï \n\"q\" " + "x".repeat(20_000);
        c.setSecret(n, value);
        assertEquals(value, c.getSecret(n).getValue());
    }

    // ---- keys -------------------------------------------------------------------------------------------------
    @Test void rsaKey_createGetAndListVersions() {
        KeyClient c = keys();
        String n = Emu.dashed("rsa");
        KeyVaultKey k = c.createKey(new CreateKeyOptions(n, KeyType.RSA).setExpiresOn(OffsetDateTime.now().plusDays(1).withNano(0)).setTags(Map.of("a", "b")));
        assertEquals(KeyType.RSA, k.getKeyType());
        assertNotNull(k.getKey().getN());
        assertNotNull(k.getKey().getE());
        assertTrue(k.getKeyOperations().contains(KeyOperation.SIGN));
        assertEquals(k.getId(), c.getKey(n).getId());
        rsa(c, n);
        assertEquals(2, c.listPropertiesOfKeyVersions(n).stream().count());
    }

    @Test void createRsaKey_withTheSdkDefaults_works() {
        KeyVaultKey k = keys().createRsaKey(new CreateRsaKeyOptions(Emu.dashed("rsa")).setKeySize(2048));
        assertEquals(KeyType.RSA, k.getKeyType());
    }

    @Test void ecKey_createWithCurves() {
        KeyClient c = keys();
        for (KeyCurveName curve : List.of(KeyCurveName.P_256, KeyCurveName.P_384, KeyCurveName.P_521)) {
            KeyVaultKey k = c.createEcKey(new CreateEcKeyOptions(Emu.dashed("ec")).setCurveName(curve));
            assertEquals(KeyType.EC, k.getKeyType());
            assertEquals(curve, k.getKey().getCurveName());
            assertNotNull(k.getKey().getX());
        }
    }

    @Test void importKey_thenUseIt_forRemoteDecrypt() throws Exception {
        KeyPairGenerator g = KeyPairGenerator.getInstance("RSA");
        g.initialize(2048);
        KeyPair kp = g.generateKeyPair();
        java.security.interfaces.RSAPrivateCrtKey priv = (java.security.interfaces.RSAPrivateCrtKey) kp.getPrivate();
        JsonWebKey jwk = JsonWebKey.fromRsa(kp);
        String n = Emu.dashed("imp");
        KeyClient c = keys();
        KeyVaultKey imported = c.importKey(new ImportKeyOptions(n, jwk).setHardwareProtected(false));
        assertEquals(KeyType.RSA, imported.getKeyType());
        CryptographyClient crypto = c.getCryptographyClient(n);
        byte[] plain = "imported key secret".getBytes(StandardCharsets.UTF_8);
        javax.crypto.Cipher enc = javax.crypto.Cipher.getInstance("RSA/ECB/OAEPWithSHA-1AndMGF1Padding");
        enc.init(javax.crypto.Cipher.ENCRYPT_MODE, kp.getPublic());
        byte[] cipher = enc.doFinal(plain);
        assertArrayEquals(plain, crypto.decrypt(EncryptionAlgorithm.RSA_OAEP, cipher).getPlainText());
    }

    @Test void cryptography_encryptDecrypt_roundTripsWithRsaOaep() {
        KeyClient c = keys();
        String n = Emu.dashed("enc");
        rsa(c, n);
        CryptographyClient crypto = c.getCryptographyClient(n);
        byte[] plain = Util.bytes(100);
        for (EncryptionAlgorithm alg : List.of(EncryptionAlgorithm.RSA_OAEP, EncryptionAlgorithm.RSA_OAEP_256, EncryptionAlgorithm.RSA1_5)) {
            EncryptResult e = crypto.encrypt(alg, plain);
            assertEquals(alg, e.getAlgorithm());
            assertEquals(256, e.getCipherText().length);
            assertArrayEquals(plain, crypto.decrypt(alg, e.getCipherText()).getPlainText(), alg.toString());
        }
    }

    @Test void cryptography_signAndVerify_rsaAndEc() throws Exception {
        KeyClient c = keys();
        String rsa = Emu.dashed("sig"), ec = Emu.dashed("sigec");
        rsa(c, rsa);
        c.createEcKey(new CreateEcKeyOptions(ec).setCurveName(KeyCurveName.P_256));
        byte[] data = "sign me".getBytes(StandardCharsets.UTF_8);
        byte[] sha256 = MessageDigest.getInstance("SHA-256").digest(data);
        CryptographyClient rc = c.getCryptographyClient(rsa);
        SignResult rs = rc.sign(SignatureAlgorithm.RS256, sha256);
        assertEquals(256, rs.getSignature().length);
        assertTrue(rc.verify(SignatureAlgorithm.RS256, sha256, rs.getSignature()).isValid());
        assertFalse(rc.verify(SignatureAlgorithm.RS256, MessageDigest.getInstance("SHA-256").digest("other".getBytes()), rs.getSignature()).isValid());
        SignResult rd = rc.signData(SignatureAlgorithm.PS256, data);
        assertTrue(rc.verifyData(SignatureAlgorithm.PS256, data, rd.getSignature()).isValid());

        CryptographyClient ecc = c.getCryptographyClient(ec);
        SignResult es = ecc.sign(SignatureAlgorithm.ES256, sha256);
        assertEquals(64, es.getSignature().length);
        assertTrue(ecc.verify(SignatureAlgorithm.ES256, sha256, es.getSignature()).isValid());
    }

    @Test void cryptography_wrapAndUnwrapAKey() {
        KeyClient c = keys();
        String n = Emu.dashed("wrap");
        rsa(c, n);
        CryptographyClient crypto = c.getCryptographyClient(n);
        byte[] cek = Util.bytes(32);
        WrapResult w = crypto.wrapKey(KeyWrapAlgorithm.RSA_OAEP, cek);
        assertArrayEquals(cek, crypto.unwrapKey(KeyWrapAlgorithm.RSA_OAEP, w.getEncryptedKey()).getKey());
    }

    @Test void key_deleteRecoverPurge_andBackupRestore_andRotate() {
        KeyClient c = keys();
        String n = Emu.dashed("kdel");
        rsa(c, n);
        byte[] backup = c.backupKey(n);
        KeyVaultKey rotated = c.rotateKey(n);
        assertNotNull(rotated);
        assertEquals(2, c.listPropertiesOfKeyVersions(n).stream().count());
        c.beginDeleteKey(n).waitForCompletion(Duration.ofSeconds(30));
        assertThrows(ResourceNotFoundException.class, () -> c.getKey(n));
        c.beginRecoverDeletedKey(n).waitForCompletion(Duration.ofSeconds(30));
        assertNotNull(c.getKey(n));
        c.beginDeleteKey(n).waitForCompletion(Duration.ofSeconds(30));
        c.purgeDeletedKey(n);
        KeyVaultKey restored = c.restoreKeyBackup(backup);
        assertEquals(n, restored.getName());
    }

    @Test void key_getRandomBytes() {
        assertEquals(32, keys().getRandomBytes(32).length);
    }

    // ---- certificates -----------------------------------------------------------------------------------------
    @Test void certificate_createSelfSigned_throughThePoller_thenReadItBack() throws Exception {
        CertificateClient c = certs();
        String n = Emu.dashed("cert");
        CertificatePolicy policy = new CertificatePolicy("Self", "CN=azemu-java.example.com");
        SyncPoller<CertificateOperation, KeyVaultCertificateWithPolicy> poller = c.beginCreateCertificate(n, policy);
        PollResponse<CertificateOperation> first = poller.poll();
        assertNotNull(first.getValue());
        KeyVaultCertificateWithPolicy cert = poller.getFinalResult();
        assertEquals(n, cert.getName());
        assertNotNull(cert.getCer());
        X509Certificate x509 = (X509Certificate) CertificateFactory.getInstance("X.509").generateCertificate(new ByteArrayInputStream(cert.getCer()));
        assertTrue(x509.getSubjectX500Principal().getName().contains("azemu-java.example.com"));
        KeyVaultCertificateWithPolicy got = c.getCertificate(n);
        assertEquals("Self", got.getPolicy().getIssuerName());
        assertNotNull(got.getSecretId());
        assertNotNull(got.getKeyId());
        // the certificate's private key is available as a secret (PKCS#12 by default)
        KeyVaultSecret asSecret = secrets().getSecret(n);
        assertNotNull(asSecret.getValue());
    }

    static byte[] pkcs12() throws Exception {
        Path dir = Files.createTempDirectory("azemu-java-p12");
        Path file = dir.resolve("c.p12");
        Process p = new ProcessBuilder(Paths.get(System.getProperty("java.home"), "bin", "keytool").toString(), "-genkeypair", "-alias", "a",
            "-keyalg", "RSA", "-keysize", "2048", "-storetype", "PKCS12", "-keystore", file.toString(), "-storepass", "changeit",
            "-dname", "CN=imported.example.com", "-validity", "30").redirectErrorStream(true).start();
        p.getInputStream().readAllBytes();
        assertEquals(0, p.waitFor());
        byte[] b = Files.readAllBytes(file);
        Files.delete(file);
        Files.delete(dir);
        return b;
    }

    @Test void certificate_importPkcs12_thenGetIt() throws Exception {
        CertificateClient c = certs();
        String n = Emu.dashed("p12");
        KeyVaultCertificateWithPolicy cert = c.importCertificate(new ImportCertificateOptions(n, pkcs12()).setPassword("changeit"));
        assertEquals(n, cert.getName());
        X509Certificate x509 = (X509Certificate) CertificateFactory.getInstance("X.509").generateCertificate(new ByteArrayInputStream(cert.getCer()));
        assertTrue(x509.getSubjectX500Principal().getName().contains("imported.example.com"));
        assertEquals(CertificateContentType.PKCS12, c.getCertificatePolicy(n).getContentType());
    }

    @Test void certificate_importPem_withThePemContentTypeInThePolicy() throws Exception {
        KeyStore ks = KeyStore.getInstance("PKCS12");
        ks.load(new ByteArrayInputStream(pkcs12()), "changeit".toCharArray());
        PrivateKey key = (PrivateKey) ks.getKey("a", "changeit".toCharArray());
        X509Certificate x = (X509Certificate) ks.getCertificate("a");
        String pem = "-----BEGIN PRIVATE KEY-----\n" + Base64.getMimeEncoder(64, "\n".getBytes()).encodeToString(key.getEncoded()) + "\n-----END PRIVATE KEY-----\n"
            + "-----BEGIN CERTIFICATE-----\n" + Base64.getMimeEncoder(64, "\n".getBytes()).encodeToString(x.getEncoded()) + "\n-----END CERTIFICATE-----\n";
        CertificateClient c = certs();
        String n = Emu.dashed("pem");
        KeyVaultCertificateWithPolicy cert = c.importCertificate(new ImportCertificateOptions(n, pem.getBytes(StandardCharsets.UTF_8))
            .setPolicy(new CertificatePolicy("Self", "CN=imported.example.com").setContentType(CertificateContentType.PEM)));
        assertEquals(n, cert.getName());
        assertNotNull(cert.getCer());
    }

    @Test void certificate_listUpdateDeleteRecoverPurge() throws Exception {
        CertificateClient c = certs();
        String n = Emu.dashed("clife");
        c.beginCreateCertificate(n, new CertificatePolicy("Self", "CN=l.example.com")).waitForCompletion(Duration.ofSeconds(60));
        assertTrue(c.listPropertiesOfCertificates().stream().anyMatch(p -> p.getName().equals(n)));
        KeyVaultCertificateWithPolicy got = c.getCertificate(n);
        got.getProperties().setTags(Map.of("t", "v"));
        c.updateCertificateProperties(got.getProperties());
        assertEquals("v", c.getCertificate(n).getProperties().getTags().get("t"));
        c.beginDeleteCertificate(n).waitForCompletion(Duration.ofSeconds(30));
        assertThrows(ResourceNotFoundException.class, () -> c.getCertificate(n));
        c.beginRecoverDeletedCertificate(n).waitForCompletion(Duration.ofSeconds(30));
        assertNotNull(c.getCertificate(n));
        c.beginDeleteCertificate(n).waitForCompletion(Duration.ofSeconds(30));
        c.purgeDeletedCertificate(n);
    }

    @Test void certificate_issuersAndContacts() {
        CertificateClient c = certs();
        String issuer = Emu.dashed("iss");
        CertificateIssuer created = c.createIssuer(new CertificateIssuer(issuer, "Test"));
        assertEquals(issuer, created.getName());
        assertEquals("Test", c.getIssuer(issuer).getProvider());
        assertTrue(c.listPropertiesOfIssuers().stream().anyMatch(i -> i.getName().equals(issuer)));
        c.deleteIssuer(issuer);
        // setContacts, listContacts and deleteContacts return lazy paged iterables: nothing is sent until they are read
        assertEquals(1, c.setContacts(List.of(new CertificateContact().setEmail("a@example.com").setName("A"))).stream().count());
        assertEquals(1, c.listContacts().stream().count());
        assertEquals(1, c.deleteContacts().stream().count());
    }

    @Test void certificate_getPending_afterBeginCreateWithAnUnknownIssuer_hasAnOperationInProgress() {
        CertificateClient c = certs();
        String n = Emu.dashed("pending");
        SyncPoller<CertificateOperation, KeyVaultCertificateWithPolicy> p = c.beginCreateCertificate(n, new CertificatePolicy("Unknown", "CN=pending.example.com"));
        CertificateOperation op = p.poll().getValue();
        assertEquals("inProgress", op.getStatus());
        assertNotNull(op.getCsr());
    }
}
